/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Parameter mutation propagation and deferred cross-library mutation candidates.

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::class::ClassTable;
use crate::class::FieldKind;
use crate::effects::ArgSlot;
use crate::effects::CallData;
use crate::effects::Effect;
use crate::effects::EffectData;
use crate::effects::EffectKind;
use crate::effects::EffectTable;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::imports::ImportGraph;
use crate::module_safety::MutatedParam;
use crate::module_safety::MutationCandidate;
use crate::module_safety::MutationCandidateSite;
use crate::module_safety::ParamPosition;
use crate::project::AnalysisMap;
use crate::project::calls::Call;
use crate::project::info::ProjectInfo;
use crate::project::info::constructor_method_names;
use crate::project::info::resolve_enclosing_module;
use crate::project::state::GlobalAnalysisState;
use crate::traits::ModuleNameExt;

/// Per-scope context for mutation-candidate detection
#[derive(Clone, Copy)]
struct MutationCandidateScope<'a> {
    /// The enclosing module.
    module: ModuleName,
    /// Scope-local caller name (`None` at module scope)
    caller_function: Option<ModuleName>,
    /// Module's unresolved-import sets
    missing: Option<&'a AHashSet<ModuleName>>,
    ambiguous: Option<&'a AHashSet<ModuleName>>,
}

/// Whether `callee` (or one of its parents) is an unresolved import of the
/// enclosing module.
fn callee_is_unresolved(
    callee: &ModuleName,
    missing: Option<&AHashSet<ModuleName>>,
    ambiguous: Option<&AHashSet<ModuleName>>,
) -> bool {
    let is_unresolved = |name: &ModuleName| {
        missing.is_some_and(|m| m.contains(name)) || ambiguous.is_some_and(|a| a.contains(name))
    };
    is_unresolved(callee)
        || callee
            .iter_parents()
            .any(|(parent, _)| is_unresolved(&parent))
}

/// Number of implicit leading parameters (the receiver) for a method call.
/// Returns `None` when the method's kind is unknown (e.g. class could not be resolved).
fn method_receiver_offset(method: &ModuleName, classes: &ClassTable, bound: bool) -> Option<usize> {
    let (class, leaf) = method.split_attr()?;
    Some(match classes.lookup(&class)?.get_field(&leaf)?.kind {
        FieldKind::StaticMethod => 0,
        FieldKind::ClassMethod => 1,
        FieldKind::InstanceMethod => usize::from(bound),
        _ => return None,
    })
}

/// Yield each `(callee, arg_offset)` a call binds to: the function(s) whose
/// parameters the call's explicit arguments map to, paired with the number of
/// implicit leading parameters (the receiver) to skip.
pub(super) fn iter_callees<'a>(
    eff: &'a Effect,
    classes: &'a ClassTable,
) -> impl Iterator<Item = (ModuleName, usize)> + 'a {
    let is_method = matches!(
        eff.kind,
        EffectKind::MethodCall | EffectKind::UnboundMethodCall | EffectKind::SuperMethodCall
    );
    let is_constructor = !is_method && classes.contains(&eff.name);

    // Constructor calls dispatch to each constructor method (offset 1); empty
    // for non-constructor calls.
    let constructors = is_constructor
        .then(|| constructor_method_names(eff.name, classes))
        .into_iter()
        .flatten()
        .map(|method| (method, 1));

    // Method / plain function calls resolve to a single callee (`eff.name`) at
    // one or two offsets; empty for constructor calls.
    let (offset, extra_offset) = match eff.kind {
        _ if is_constructor => (None, None),
        // A `super()` call is bound, and its recorded name is the caller's own
        // method, whose signature an override mirrors.
        EffectKind::MethodCall | EffectKind::SuperMethodCall => {
            match method_receiver_offset(&eff.name, classes, true) {
                Some(offset) => (Some(offset), None),
                // Unknown kind (e.g. a builtin / third-party class): assume an
                // implicit receiver, as bound calls usually have one.
                None => (Some(1), None),
            }
        }
        EffectKind::UnboundMethodCall => match method_receiver_offset(&eff.name, classes, false) {
            Some(offset) => (Some(offset), None),
            // Unknown kind: the receiver may be explicit (0) or implicit (1), so
            // consider both to avoid missing a mutated parameter.
            None => (Some(0), Some(1)),
        },
        _ => (Some(0), None),
    };
    let name = eff.name;
    let single = offset
        .into_iter()
        .chain(extra_offset)
        .map(move |offset| (name, offset));

    constructors.chain(single)
}

/// Per function, the set of parameters that are method-mutated either directly or transitively via
/// parameter forwarding.
///
/// TODO(T237092592):
/// - Cross-library forwarding is not tracked
/// - Unknown callee (no EffectTable entry) similarly drops the edge, inconsistent with other places
///   where unknown => unsafe
/// - `**kwargs` forwarding of a parameter is not tracked precisely; only named
///   keyword arguments are added to edges.
pub(super) fn compute_mutated_params(
    effect_table: &EffectTable,
    functions: &AHashMap<ModuleName, ModuleName>,
    classes: &ClassTable,
    analysis_map: &AnalysisMap,
) -> AHashMap<ModuleName, AHashSet<ModuleName>> {
    // Compute a fixpoint over the call graph. The "seed" is the base set of functions that directly
    // mutate their parameters, and an "edge" is a transitive mutation from callee->caller (reversed
    // from the call graph since mutations flow in the opposite direction to calls).

    // (function, mutated parameter)
    type SeedMutation = (ModuleName, ModuleName);
    // ((callee, target parameter), (caller, caller parameter))
    type Edge = ((ModuleName, ModuleName), SeedMutation);

    // Scan every scope's effects in parallel
    let (all_seeds, all_edges): (Vec<SeedMutation>, Vec<Edge>) = effect_table
        .par_iter()
        .fold(
            || (Vec::new(), Vec::new()),
            |(mut seeds, mut edges), (scope, effs)| {
                for eff in effs {
                    // Seed: parameters mutated directly in this function.
                    if eff.kind == EffectKind::ParamMethodCall {
                        seeds.push((*scope, eff.name));
                        continue;
                    }
                    // Parameter-forwarding edges recorded on call effects.
                    let Some(call_data) = eff.call_data() else {
                        continue;
                    };
                    let forwarded = call_data.forwarded_params();
                    if forwarded.is_empty() {
                        continue;
                    }
                    // A call may bind to several callees (a constructor's
                    // __init__/__new__); resolve each owner's parameter names once.
                    for (owner, arg_offset) in iter_callees(eff, classes) {
                        let owner_param_names = functions
                            .get(&owner)
                            .and_then(|module| analysis_map.get(module))
                            .and_then(|m| m.definitions.param_names.get(&owner));
                        for (slot, param) in forwarded {
                            match slot {
                                ArgSlot::Keyword(name) => {
                                    edges.push(((owner, *name), (*scope, *param)));
                                }
                                ArgSlot::Positional(idx) => {
                                    if let Some(target) = owner_param_names
                                        .and_then(|names| names.get(idx + arg_offset))
                                        .map(ModuleName::from_name)
                                    {
                                        edges.push(((owner, target), (*scope, *param)));
                                    }
                                }
                                ArgSlot::StarExpansion(start) => {
                                    // `*param` unpacks into the callee's positional
                                    // parameters from the star's position onward, so
                                    // forward to each parameter at or after that slot
                                    // (offset by the receiver). Leading positional args
                                    // before the star fill earlier params, which `*param`
                                    // cannot reach.
                                    if let Some(names) = owner_param_names {
                                        edges.extend(names.iter().skip(start + arg_offset).map(
                                            |name| {
                                                (
                                                    (owner, ModuleName::from_name(name)),
                                                    (*scope, *param),
                                                )
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                (seeds, edges)
            },
        )
        .reduce(
            || (Vec::new(), Vec::new()),
            |(mut seeds, mut edges), (s, e)| {
                seeds.extend(s);
                edges.extend(e);
                (seeds, edges)
            },
        );

    // Merge into `mutated` (seeds the fixpoint) and `dependents`, the reverse
    // index: (owner `g`, parameter `q`) -> callers whose parameter becomes mutated
    // once `q` is known mutated.
    // One entry per mutated scope (~3 seeds/scope)
    let mut mutated: AHashMap<ModuleName, AHashSet<ModuleName>> =
        AHashMap::with_capacity(all_seeds.len() / 3);
    let mut worklist: Vec<SeedMutation> = Vec::new();
    for (scope, param) in all_seeds {
        if mutated.entry(scope).or_default().insert(param) {
            worklist.push((scope, param));
        }
    }

    // One entry per mutated (owner, param), not per edge (~2 edges/key).
    let dependents: DashMap<(ModuleName, ModuleName), Vec<SeedMutation>> =
        DashMap::with_capacity(all_edges.len() / 2);
    all_edges.into_par_iter().for_each(|(key, dep)| {
        dependents.entry(key).or_default().push(dep);
    });

    // Fixpoint: when (g, q) becomes mutated, each caller forwarding a parameter
    // into q has that parameter become mutated too.
    while let Some((g, q)) = worklist.pop() {
        let Some(deps) = dependents.get(&(g, q)) else {
            continue;
        };
        for &(f, p) in deps.value() {
            if mutated.entry(f).or_default().insert(p) {
                worklist.push((f, p));
            }
        }
    }

    mutated
}

impl ProjectInfo {
    /// Collect calls that pass an imported object to a callee unresolved in this library (a
    /// cross-library candidate), grouped by the caller's module.
    /// Callees resolvable in this library are skipped; the map step already handled them via
    /// `call_mutates_imported_arg`. Callees that are not imported from another library (builtins,
    /// stdlib used without import, locals) are also skipped: they can never resolve to a cached
    /// library function at reduce time.
    pub(super) fn collect_mutation_candidates(
        &self,
        import_graph: &ImportGraph,
    ) -> AHashMap<ModuleName, Vec<MutationCandidate>> {
        let mut flat: Vec<(ModuleName, MutationCandidate)> = self
            .effect_table
            .par_iter()
            .fold(Vec::new, |mut acc, (scope, effs)| {
                let mut caller_scope = None;
                for eff in effs {
                    let EffectData::Call(ref call_data) = eff.data else {
                        continue;
                    };
                    if !call_data.has_unsafe_args() {
                        continue;
                    }
                    for (callee, arg_offset) in iter_callees(eff, &self.classes) {
                        // Resolvable in this library -> already handled by the map step.
                        if self.functions.contains_key(&callee)
                            || self.mutated_params.contains_key(&callee)
                        {
                            continue;
                        }
                        // Resolve and cache the scope's module, caller name, and the module's
                        // unresolved-import sets.
                        let Some(MutationCandidateScope {
                            module,
                            caller_function,
                            missing,
                            ambiguous,
                        }) = *caller_scope.get_or_insert_with(|| {
                            self.mutation_candidate_scope(scope, import_graph)
                        })
                        else {
                            // `None` means the module is unknown or has no unresolved imports,
                            // so no callee here can resolve cross-library.

                            continue;
                        };
                        // Only defer callees that could actually resolve to another
                        // library: those imported here but unresolved locally. Skips
                        // builtins and unimported names, which are never confirmed at reduce.
                        if !callee_is_unresolved(&callee, missing, ambiguous) {
                            continue;
                        }
                        let site = match &caller_function {
                            Some(name) => MutationCandidateSite::Function { name: *name },
                            None => MutationCandidateSite::ModuleScope { call: eff.name },
                        };
                        acc.push((
                            module,
                            MutationCandidate {
                                callee,
                                site,
                                arg_offset,
                                imported_args: call_data.imported_args().clone(),
                                range: eff.range,
                                from_main_guard: eff.from_main_guard,
                            },
                        ));
                    }
                }
                acc
            })
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            });

        // Reduce-time application is order-sensitive, so sort to keep the sequence
        // independent of how the parallel collection happened to interleave.
        flat.par_sort_unstable();

        let mut result: AHashMap<ModuleName, Vec<MutationCandidate>> = AHashMap::new();
        for (module, candidate) in flat {
            result.entry(module).or_default().push(candidate);
        }
        result
    }

    /// Resolve the enclosing module and scope-local caller name for `scope`, together with the
    /// module's unresolved-import sets (fetched once per scope).
    /// Returns `None` when the module can't be located, or when it has no unresolved imports.
    fn mutation_candidate_scope<'a>(
        &self,
        scope: &ModuleName,
        import_graph: &'a ImportGraph,
    ) -> Option<MutationCandidateScope<'a>> {
        // A module-scope effect has no enclosing function; anything else is
        // named relative to the module that contains it.
        let (module, caller_function) = if self.analysis_map.contains_key(scope) {
            (*scope, None)
        } else {
            let (module, local) =
                resolve_enclosing_module(scope, |name| self.analysis_map.contains_key(name))?;
            (module, Some(ModuleName::from_str(local)))
        };
        let missing = import_graph.get_missing_imports(&module);
        let ambiguous = import_graph.get_ambiguous_imports(&module);
        if missing.is_none() && ambiguous.is_none() {
            return None;
        }
        Some(MutationCandidateScope {
            module,
            caller_function,
            missing,
            ambiguous,
        })
    }

    /// Resolve a function's mutated parameters to positional indices using the
    /// callee's definition table.
    pub(super) fn resolve_cached_mutated_params_for(
        &self,
        func: &ModuleName,
    ) -> Option<Vec<MutatedParam>> {
        let params = self.mutated_params.get(func)?;
        let callee_module = self.functions.get(func).copied().unwrap_or(*func);
        let defs = self
            .analysis_map
            .get(&callee_module)
            .map(|m| &m.definitions);

        let mut resolved = Vec::with_capacity(params.len());
        for param in params {
            // Distinguish an unresolvable signature (no `defs`) from a resolved
            // keyword-only parameter, so the reduce never treats an unresolved
            // positional parameter as keyword-only (which would miss a positional
            // match and be a false-safe).
            let position = match defs {
                Some(d) => d.classify_param(func, param.as_str()),
                None => ParamPosition::Unresolved,
            };
            resolved.push(MutatedParam {
                name: *param,
                position,
            });
        }
        Some(resolved)
    }

    /// Whether `call_data` passes an imported variable into a (transitively)
    /// mutated parameter of `callee`, with explicit args starting at `arg_offset`.
    /// (Helper function for `call_mutates_imported_arg()` below.)
    fn callee_mutates_imported_arg(
        &self,
        call_data: &CallData,
        callee: &ModuleName,
        arg_offset: usize,
    ) -> bool {
        let Some(mutated) = self.mutated_params.get(callee) else {
            return false;
        };
        let callee_module = self.functions.get(callee).copied().unwrap_or(*callee);
        let defs = self
            .analysis_map
            .get(&callee_module)
            .map(|m| &m.definitions);
        call_data.imported_args().hits_any_param(
            mutated.iter().map(|param| {
                let name = param.as_str();
                let position = match defs {
                    Some(d) => d.classify_param(callee, name),
                    None => ParamPosition::Unresolved,
                };
                (name, position)
            }),
            arg_offset,
        )
    }

    /// Whether `call_effect` passes an imported variable to a parameter the callee
    /// (transitively) mutates, i.e. running this call mutates imported state.
    pub(super) fn call_mutates_imported_arg(&self, call_effect: &Effect) -> bool {
        let Some(call_data) = call_effect.call_data() else {
            return false;
        };
        if !call_data.has_unsafe_args() {
            return false;
        }
        iter_callees(call_effect, &self.classes)
            .any(|(callee, offset)| self.callee_mutates_imported_arg(call_data, &callee, offset))
    }

    /// Whether `call_effect` is a call that passes an imported variable as an
    /// argument. Such a call must be deferred to the reduce step when its callee
    /// is unresolved in this library (the callee may mutate the argument).
    pub(super) fn defers_cross_library_mutation(&self, call_effect: &Effect) -> bool {
        call_effect
            .call_data()
            .is_some_and(|data| data.has_unsafe_args())
    }

    pub(super) fn check_call_params(&self, call: &Call, state: &GlobalAnalysisState) {
        if self.call_mutates_imported_arg(call.effect) {
            let err = SafetyError::new_from_effect(ErrorKind::ImportedVarArgument, call.effect);
            state.add_error_to_module(call.caller_module, err);
        }
    }
}
