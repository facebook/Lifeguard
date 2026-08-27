/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::cache::CONSTRUCTOR_METHODS;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::hasher::union_larger;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::MutationCandidate;
use crate::module_safety::MutationCandidateSite;
use crate::traits::ModuleNameExt;

pub(crate) struct ResolutionOutcome {
    pub promoted: Vec<(ModuleName, String)>,
    pub globally_safe: AHashSet<String>,
    pub resolved_to_safe: bool,
}

/// The name an error contributes to the promotion index, or `None` if it
/// contributes nothing.
///
/// Only a bare name is indexed: a qualified one resolves through its module's
/// safety map instead, so putting it in the global index would let a same-named
/// function in an unrelated module clear it. The trailing `()` is stripped
/// because error metadata renders a call as `pkg.mod.f()` while the index is
/// keyed on the name alone; metadata that is already a plain name passes
/// through untouched.
pub(crate) fn unqualified_index_key(metadata: &str) -> Option<&str> {
    let name = metadata.trim_end_matches("()");
    (!name.contains('.')).then_some(name)
}

/// Run the cross-library semantic resolution phases in their canonical order.
pub(crate) fn resolve_program<'a>(
    module_names: &AHashSet<ModuleName>,
    function_safety: &mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    candidates: impl Iterator<Item = (ModuleName, &'a [MutationCandidate])>,
    mut needed_unqualified: AHashSet<String>,
    mut module_scope_error: impl FnMut(ModuleName, String),
) -> ResolutionOutcome {
    let resolved_to_safe = apply_mutation_candidates(
        candidates,
        module_names,
        function_safety,
        |module, metadata| {
            if let Some(name) = unqualified_index_key(&metadata) {
                needed_unqualified.insert(name.to_owned());
            }
            module_scope_error(module, metadata);
        },
    );
    let (promoted, globally_safe) =
        promote_fixpoint(module_names, function_safety, needed_unqualified);
    ResolutionOutcome {
        promoted,
        globally_safe,
        resolved_to_safe,
    }
}

/// Get a function's safety info from the nested module -> name map.
pub(crate) fn get_function_safety<'a>(
    map: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    module: &ModuleName,
    name: &str,
) -> Option<&'a FunctionSafetyInfo> {
    map.get(module)?.get(name)
}

/// Mutable counterpart used when resolution commits a verdict transition.
fn get_function_safety_mut<'a>(
    map: &'a mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    module: &ModuleName,
    name: &str,
) -> Option<&'a mut FunctionSafetyInfo> {
    map.get_mut(module)?.get_mut(name)
}

/// Resolve a callee FQN against the longest module prefix in the merged program.
fn lookup_callee_info<'a>(
    callee: &ModuleName,
    module_names: &AHashSet<ModuleName>,
    function_safety: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> Option<&'a FunctionSafetyInfo> {
    for (parent, dot_pos) in callee.iter_parents() {
        if module_names.contains(&parent) {
            return get_function_safety(function_safety, &parent, &callee.as_str()[dot_pos + 1..]);
        }
    }
    None
}

/// Resolve cached cross-library mutation candidates against merged function verdicts.
///
/// Confirmed module-scope candidates emit `ImportedVarArgument`; confirmed
/// function candidates become hard `Unsafe`. An unconfirmed candidate is
/// discharged only when its callee is unresolved or verified safe. A resolved
/// non-safe callee must remain in `missing_dep_callees`, otherwise promotion
/// could incorrectly turn its caller safe.
fn apply_mutation_candidates<'a>(
    modules: impl Iterator<Item = (ModuleName, &'a [MutationCandidate])>,
    module_names: &AHashSet<ModuleName>,
    function_safety: &mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    mut module_scope_error: impl FnMut(ModuleName, String),
) -> bool {
    let pairs: Vec<(ModuleName, &MutationCandidate)> = modules
        .flat_map(|(module, candidates)| {
            candidates.iter().map(move |candidate| (module, candidate))
        })
        .collect();
    // Confirmation reads only `mutated_params`, which this phase never writes,
    // so all candidates can be checked against the same frozen state in parallel.
    let confirmed: Vec<bool> = pairs
        .par_iter()
        .map(|(_, candidate)| candidate_mutates(candidate, module_names, function_safety))
        .collect();

    // Apply in original order: verdict writes and `callee_resolves_unsafe`
    // reads are order-dependent even though confirmation above is not.
    let mut resolved_to_safe = false;
    for (&(module, candidate), &confirmed) in pairs.iter().zip(&confirmed) {
        match (&candidate.site, confirmed) {
            (MutationCandidateSite::ModuleScope { call }, true) => {
                module_scope_error(module, call.as_str().to_owned());
            }
            (MutationCandidateSite::ModuleScope { .. }, false) => {}
            (MutationCandidateSite::Function { name }, true) => {
                if let Some(info) = get_function_safety_mut(function_safety, &module, name.as_str())
                {
                    info.verdict.insert(FunctionSafety::Unsafe);
                    // The callee is resolved as mutating the imported argument;
                    // discharge that missing dependency while retaining `Unsafe`.
                    info.missing_dep_callees.remove(&candidate.callee);
                    if info.missing_dep_callees.is_empty() {
                        info.verdict.remove(FunctionSafety::UnsafeMissingDep);
                    }
                }
            }
            (MutationCandidateSite::Function { name }, false) => {
                // A resolved non-safe callee still blocks its caller. Only an
                // unresolved or verified-safe callee discharges this dependency.
                if callee_resolves_unsafe(&candidate.callee, module_names, function_safety) {
                    continue;
                }
                if let Some(info) = get_function_safety_mut(function_safety, &module, name.as_str())
                {
                    info.missing_dep_callees.remove(&candidate.callee);
                    if info.verdict.has(FunctionSafety::UnsafeMissingDep)
                        && info.missing_dep_callees.is_empty()
                    {
                        info.verdict.remove(FunctionSafety::UnsafeMissingDep);
                        // Other concerns such as `UnsafeIfImported` still make
                        // cross-module calls unsafe and cannot verify callers.
                        if info.verdict.is_safe() {
                            resolved_to_safe = true;
                        }
                    }
                }
            }
        }
    }
    resolved_to_safe
}

/// Whether a candidate feeds imported state into a parameter its callee mutates.
///
/// Class-level calls record the class FQN, while mutation metadata lives on
/// `__init__`/`__new__`. Those methods receive an implicit first argument that
/// is absent at the call site, hence the `arg_offset + 1` probes below.
fn candidate_mutates(
    candidate: &MutationCandidate,
    module_names: &AHashSet<ModuleName>,
    function_safety: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> bool {
    let callee_mutates = |callee: &ModuleName, arg_offset: usize| {
        lookup_callee_info(callee, module_names, function_safety).is_some_and(|info| {
            candidate.imported_args.hits_any_param(
                info.mutated_params
                    .iter()
                    .map(|param| (param.name.as_str(), param.position)),
                arg_offset,
            )
        })
    };
    if callee_mutates(&candidate.callee, candidate.arg_offset) {
        return true;
    }
    CONSTRUCTOR_METHODS.into_iter().any(|method| {
        callee_mutates(
            &candidate.callee.append_str(method),
            candidate.arg_offset + 1,
        )
    })
}

/// Whether a callee resolves to a verdict that must continue blocking its caller.
/// Unresolved callees return false, matching whole-program unresolved-call handling.
fn callee_resolves_unsafe(
    callee: &ModuleName,
    module_names: &AHashSet<ModuleName>,
    function_safety: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> bool {
    lookup_callee_info(callee, module_names, function_safety)
        .is_some_and(|info| !info.verdict.is_safe())
}

/// A promotion candidate's callee, pre-split so fixpoint rounds do not repeat it.
enum ResolvedCallee {
    Qualified { module: ModuleName, local: String },
    Unqualified { name: String },
}

struct PromotionCandidate {
    module: ModuleName,
    name: String,
    /// Verdict without `UnsafeMissingDep`; stable across every fixpoint round.
    base_verdict: FunctionSafety,
    callees: Vec<ResolvedCallee>,
}

/// Split a callee at its longest known module prefix.
fn resolve_callee(func_name: &str, module_names: &AHashSet<ModuleName>) -> ResolvedCallee {
    match ModuleName::from_str(func_name)
        .iter_parents()
        .find(|(parent, _)| module_names.contains(parent))
    {
        Some((module, dot_pos)) => ResolvedCallee::Qualified {
            module,
            local: func_name[dot_pos + 1..].to_owned(),
        },
        None => ResolvedCallee::Unqualified {
            name: func_name.to_owned(),
        },
    }
}

/// Resolve to a non-blocking verdict, using the demanded-name global indices
/// only for genuinely unqualified callees.
fn resolve_callee_verdict(
    callee: &ResolvedCallee,
    function_safety: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    globally_safe: &AHashSet<String>,
    globally_if_imported: &AHashSet<String>,
) -> Option<FunctionSafety> {
    match callee {
        ResolvedCallee::Qualified { module, local } => function_safety
            .get(module)
            .and_then(|functions| functions.get(local))
            .map(|info| info.verdict)
            .and_then(maybe_non_blocking_verdict),
        ResolvedCallee::Unqualified { name } => {
            if globally_safe.contains(name.as_str()) {
                Some(FunctionSafety::Safe)
            } else if globally_if_imported.contains(name.as_str()) {
                Some(FunctionSafety::UnsafeIfImported)
            } else {
                None
            }
        }
    }
}

/// Promote `UnsafeMissingDep` functions whose callees all become non-blocking,
/// repeating until a round produces no new verdicts.
fn promote_fixpoint(
    module_names: &AHashSet<ModuleName>,
    function_safety: &mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    mut needed_unqualified: AHashSet<String>,
) -> (Vec<(ModuleName, String)>, AHashSet<String>) {
    // The promotion guard is stable: promotion only removes
    // `UnsafeMissingDep`. Precompute candidates and split their callees once.
    let candidates: Vec<PromotionCandidate> = function_safety
        .par_iter()
        .filter(|(module, _)| module_names.contains(module))
        .flat_map_iter(|(module, functions)| {
            functions.iter().filter_map(move |(name, info)| {
                if info.verdict.has(FunctionSafety::UnsafeMissingDep)
                    && !info.verdict.has(FunctionSafety::Unsafe)
                    && !info.missing_dep_callees.is_empty()
                {
                    Some(PromotionCandidate {
                        module: *module,
                        name: name.clone(),
                        base_verdict: info.verdict.without(FunctionSafety::UnsafeMissingDep),
                        callees: info
                            .missing_dep_callees
                            .iter()
                            .map(|callee| resolve_callee(callee.as_str(), module_names))
                            .collect(),
                    })
                } else {
                    None
                }
            })
        })
        .collect();

    // Only names used by unqualified callees or cached errors need a global
    // index; hashing every function name would not affect resolution.
    for candidate in &candidates {
        for callee in &candidate.callees {
            if let ResolvedCallee::Unqualified { name } = callee
                && !needed_unqualified.contains(name.as_str())
            {
                needed_unqualified.insert(name.clone());
            }
        }
    }

    let module_functions: Vec<&AHashMap<String, FunctionSafetyInfo>> = function_safety
        .iter()
        .filter(|(module, _)| module_names.contains(module))
        .map(|(_, functions)| functions)
        .collect();
    let (mut globally_safe, mut globally_if_imported) = module_functions
        .par_iter()
        .fold(
            || (AHashSet::new(), AHashSet::new()),
            |(mut safe, mut if_imported), functions| {
                for (name, info) in *functions {
                    if !needed_unqualified.contains(name.as_str()) {
                        continue;
                    }
                    match info.verdict {
                        FunctionSafety::Safe if !safe.contains(name.as_str()) => {
                            safe.insert(name.clone());
                        }
                        FunctionSafety::UnsafeIfImported
                            if !if_imported.contains(name.as_str()) =>
                        {
                            if_imported.insert(name.clone());
                        }
                        _ => {}
                    }
                }
                (safe, if_imported)
            },
        )
        .reduce(
            || (AHashSet::new(), AHashSet::new()),
            |(safe_a, imported_a), (safe_b, imported_b)| {
                (
                    union_larger(safe_a, safe_b),
                    union_larger(imported_a, imported_b),
                )
            },
        );
    drop(module_functions);

    // Reverse indices let each round revisit only dependents of the preceding
    // round's promotions rather than rescanning every candidate.
    let mut qualified_dependents: AHashMap<ModuleName, AHashMap<String, Vec<u32>>> =
        AHashMap::new();
    let mut unqualified_dependents: AHashMap<String, Vec<u32>> = AHashMap::new();
    fn watch(dependents: &mut AHashMap<String, Vec<u32>>, key: &str, index: u32) {
        match dependents.get_mut(key) {
            Some(watchers) => watchers.push(index),
            None => {
                dependents.insert(key.to_owned(), vec![index]);
            }
        }
    }
    for (index, candidate) in candidates.iter().enumerate() {
        for callee in &candidate.callees {
            match callee {
                ResolvedCallee::Qualified { module, local } => watch(
                    qualified_dependents.entry(*module).or_default(),
                    local,
                    index as u32,
                ),
                ResolvedCallee::Unqualified { name } => {
                    watch(&mut unqualified_dependents, name, index as u32)
                }
            }
        }
    }

    // Each round reads a frozen start-of-round state in parallel, then commits
    // all promotions together. This matches a full synchronized rescan.
    let mut promoted = Vec::new();
    let mut promoted_flags = vec![false; candidates.len()];
    let mut queued = vec![true; candidates.len()];
    let mut dirty: Vec<u32> = (0..candidates.len() as u32).collect();
    while !dirty.is_empty() {
        let current = std::mem::take(&mut dirty);
        for &index in &current {
            queued[index as usize] = false;
        }

        // Frozen phase: no promotion in this round observes another from the same round.
        let frozen: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> = function_safety;
        let to_promote: Vec<(u32, FunctionSafety)> = current
            .par_iter()
            .filter_map(|&index| {
                if promoted_flags[index as usize] {
                    return None;
                }
                let candidate = &candidates[index as usize];
                let mut target = candidate.base_verdict;
                for callee in &candidate.callees {
                    target.insert(resolve_callee_verdict(
                        callee,
                        frozen,
                        &globally_safe,
                        &globally_if_imported,
                    )?);
                }
                Some((index, target))
            })
            .collect();
        if to_promote.is_empty() {
            break;
        }

        // Apply phase: commit verdicts and seed demanded-name indices for the next round.
        for &(index, target) in &to_promote {
            promoted_flags[index as usize] = true;
            let candidate = &candidates[index as usize];
            if let Some(info) =
                get_function_safety_mut(function_safety, &candidate.module, &candidate.name)
            {
                info.verdict = target;
                if needed_unqualified.contains(candidate.name.as_str()) {
                    if target.is_safe() {
                        globally_safe.insert(candidate.name.clone());
                    } else if target == FunctionSafety::UnsafeIfImported {
                        // `UnsafeIfImported` is non-blocking only in its defining module;
                        // never seed it into the globally-safe index.
                        globally_if_imported.insert(candidate.name.clone());
                    }
                }
                promoted.push((candidate.module, candidate.name.clone()));
            }
        }

        // Enqueue only dependents of functions promoted in this round.
        let mut enqueue = |index: u32| {
            if !promoted_flags[index as usize] && !queued[index as usize] {
                queued[index as usize] = true;
                dirty.push(index);
            }
        };
        for &(index, _) in &to_promote {
            let candidate = &candidates[index as usize];
            if let Some(dependents) = qualified_dependents
                .get(&candidate.module)
                .and_then(|by_name| by_name.get(candidate.name.as_str()))
            {
                dependents.iter().for_each(|&dependent| enqueue(dependent));
            }
            if let Some(dependents) = unqualified_dependents.get(candidate.name.as_str()) {
                dependents.iter().for_each(|&dependent| enqueue(dependent));
            }
        }
    }
    (promoted, globally_safe)
}

/// Keep only verdicts that do not hard-block missing-dependency promotion.
fn maybe_non_blocking_verdict(verdict: FunctionSafety) -> Option<FunctionSafety> {
    match verdict {
        value @ (FunctionSafety::Safe | FunctionSafety::UnsafeIfImported) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::ImportedArgs;
    use crate::module_safety::MutatedParam;
    use crate::module_safety::ParamPosition;

    #[test]
    fn resolution_indexes_new_module_scope_errors_before_promotion() {
        let caller = ModuleName::from_str("caller");
        let dependency = ModuleName::from_str("dependency");
        let mut mutator = FunctionSafetyInfo::new(FunctionSafety::Safe);
        mutator.mutated_params.push(MutatedParam {
            name: ModuleName::from_str("value"),
            position: ParamPosition::Positional(0),
        });
        let mut function_safety = AHashMap::from_iter([
            (
                caller,
                AHashMap::from_iter([(
                    "helper".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                )]),
            ),
            (
                dependency,
                AHashMap::from_iter([("mutate".to_owned(), mutator)]),
            ),
        ]);
        let module_names = AHashSet::from_iter([caller, dependency]);
        let candidate = MutationCandidate {
            callee: ModuleName::from_str("dependency.mutate"),
            site: MutationCandidateSite::ModuleScope {
                call: ModuleName::from_str("helper"),
            },
            arg_offset: 0,
            imported_args: ImportedArgs {
                unsafe_arg_indices: 1,
                ..Default::default()
            },
        };
        let mut errors = Vec::new();

        let outcome = resolve_program(
            &module_names,
            &mut function_safety,
            std::iter::once((caller, std::slice::from_ref(&candidate))),
            AHashSet::new(),
            |module, metadata| errors.push((module, metadata)),
        );

        assert_eq!(errors, vec![(caller, "helper".to_owned())]);
        assert!(outcome.globally_safe.contains("helper"));
    }
}
