/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Eager-scope diagnostics and whole-program safety resolution.

use anyhow::Result;
use dashmap::DashMap;
use itertools::Itertools;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::analyzer::AnalyzedModule;
use crate::class::FieldKind;
use crate::effects::EffectKind;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::hasher::union_larger;
use crate::imports::ImportGraph;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::MutationCandidateSite;
use crate::module_safety::PropertyCandidate;
use crate::module_safety::SafetyResult;
use crate::module_safety::SubclassCandidate;
use crate::project::ExecutionMode;
use crate::project::SafetyMap;
use crate::project::calls::Call;
use crate::project::calls::CallStack;
use crate::project::info::ProjectInfo;
use crate::project::info::resolve_enclosing_module;
use crate::project::state::GlobalAnalysisState;
use crate::resolution::NameSearch;
use crate::resolution::WalkedBases;
use crate::resolution::get_function_safety;
use crate::resolution::resolve_program;
use crate::traits::ModuleNameExt;

impl ProjectInfo {
    /// The incremental path defers this to the reduce, where every walked base is known.
    pub(super) fn apply_subclass_registrations(
        &self,
        safety_map: &SafetyMap,
        import_graph: &ImportGraph,
    ) {
        let mut walked = WalkedBases::default();
        for entry in safety_map.iter() {
            if let Some(safety) = entry.value().as_safety() {
                walked.record(*entry.key(), &safety.force_imports_eager_overrides);
            }
        }
        if walked.is_empty() {
            return;
        }
        safety_map.par_iter_mut().for_each(|mut entry| {
            let module = *entry.key();
            let SafetyResult::Ok(safety) = entry.value_mut() else {
                return;
            };
            let candidates = std::mem::take(&mut safety.subclass_candidates);
            safety.errors.extend(walked.registration_errors(
                module,
                &candidates,
                &self.class_bases,
                |walker, imported| import_graph.get_imports(walker).any(|m| m == imported),
            ));
        });
    }

    /// Single-pass (whole-program) resolution of recoverable `UnsafeMissingDep`
    /// verdicts, mirroring the incremental reduce in-process. Runs after the
    /// function-safety precompute and before the module-scope error pass, so that
    /// pass publishes errors against resolved verdicts (no error clearing needed).
    pub(super) fn resolve_whole_program(
        &self,
        state: &GlobalAnalysisState,
        import_graph: &ImportGraph,
    ) {
        // Build the module -> local-name -> info view the shared resolution helpers
        // expect. Assembled in parallel (fold per thread, then merge); a flat
        // fqn-keyed map would build faster but can't reproduce the helpers' callee
        // resolution (class-prefix proxy + unqualified fallback + module gating).
        // Keep the full `FunctionSafetyInfo`: mutation-candidate resolution may
        // still consult `mutated_params` when a callee resolves through this view.
        let by_module: DashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            DashMap::with_capacity(self.analysis_map.len());
        state.function_safety.par_iter().for_each(|entry| {
            let fqn = entry.key();
            let resolved =
                resolve_enclosing_module(fqn, |name| self.analysis_map.contains_key(name));
            if let Some((module, local)) = resolved {
                by_module
                    .entry(module)
                    .or_default()
                    .insert(local.to_owned(), entry.value().clone());
            }
        });
        let mut view: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            by_module.into_iter().collect();

        let module_names: AHashSet<ModuleName> = self.analysis_map.keys().copied().collect();

        let candidates = self.collect_mutation_candidates(import_graph);

        // Functions whose verdict the resolution may change: mutation-candidate call
        // sites plus everything the promotion fixpoint promotes. Only these are
        // written back (re-deriving the fqn), avoiding a full-table rewrite.
        // Call sites outnumber the functions they name by an order of magnitude, so
        // they are deduplicated as `Copy` pairs before any name is allocated.
        let sites: AHashSet<(ModuleName, ModuleName)> = candidates
            .par_iter()
            .flat_map_iter(|(module, module_candidates)| {
                module_candidates
                    .iter()
                    .filter_map(move |candidate| match &candidate.site {
                        MutationCandidateSite::Function { name } => Some((*module, *name)),
                        MutationCandidateSite::ModuleScope { .. } => None,
                    })
            })
            .fold(AHashSet::new, |mut acc, site| {
                acc.insert(site);
                acc
            })
            .reduce(AHashSet::new, union_larger);
        let mut changed: AHashSet<(ModuleName, String)> = sites
            .into_iter()
            .map(|(module, name)| (module, name.as_str().to_owned()))
            .collect();

        // Whole-program: every candidate callee is unresolved, so none are confirmed —
        // each drops from its caller's missing-dep set, promoting callers left with no
        // missing dep to Safe. Module-scope mutations cannot be confirmed (sink unused);
        // module-scope imported-arg mutations are caught by the module-scope pass.
        let outcome = resolve_program(
            &module_names,
            &mut view,
            candidates
                .iter()
                .map(|(module, candidates)| (*module, candidates.as_slice())),
            AHashSet::new(),
            NameSearch::Intern,
            // Never reached, which is why the index handed to promotion above can
            // be empty: `resolve_program` only indexes a name when a module-scope
            // candidate is *confirmed*, and none can be here. Asserted rather than
            // gated so that the assumption fails loudly if it stops holding.
            |module, metadata, _range| {
                debug_assert!(
                    false,
                    "whole-program pass confirmed a module-scope mutation ({}.{}), but no \
                     candidate callee should resolve here",
                    module.as_str(),
                    metadata,
                );
            },
        );
        changed.extend(outcome.promoted);

        // Write back only changed verdicts so the module-scope pass reads them.
        changed.par_iter().for_each(|(module, local)| {
            let fqn = ModuleName::from_str(&format!("{}.{}", module.as_str(), local));
            if let Some(mut entry) = state.function_safety.get_mut(&fqn) {
                if let Some(info) = get_function_safety(&view, module, local) {
                    entry.verdict = info.verdict;
                }
            }
        });
    }

    pub(super) fn check_load_imports_eagerly(
        &self,
        mod_name: &ModuleName,
        result: &AnalyzedModule,
        state: &GlobalAnalysisState,
    ) {
        // Find effects that trigger adding the module to the load_imports_eagerly set.
        for effs in result.module_effects.effects.values() {
            for e in effs.iter() {
                let Some(kind) = SafetyError::error_kind_for(e.kind) else {
                    continue;
                };
                if !kind.requires_eager_loading_imports() {
                    continue;
                }
                let err = SafetyError::new_from_effect(kind, e);
                state.add_force_imports_eager_override_to_module(mod_name, err);
            }
        }
        if let Some((names, range)) = &result.module_effects.names_only_in_stub {
            let names = names.iter().join(", ");
            let err = SafetyError::new(ErrorKind::NamesOnlyInStub, names, *range);
            state.add_force_imports_eager_override_to_module(mod_name, err);
        }
    }

    pub(super) fn collect_implicit_imports(
        &self,
        mod_name: &ModuleName,
        result: &AnalyzedModule,
        state: &GlobalAnalysisState,
    ) -> Result<()> {
        state.add_implicit_imports_to_module(mod_name, &result.implicit_imports);

        Ok(())
    }

    pub(super) fn collect_errors_from_scope(
        &self,
        mod_name: &ModuleName,
        scope: &ModuleName,
        mode: ExecutionMode,
        state: &GlobalAnalysisState,
    ) -> Result<()> {
        let Some(effs) = self.effect_table.get(scope) else {
            return Ok(());
        };
        for eff in effs {
            if let Some(err) = SafetyError::from_effect(eff) {
                state.add_error_to_module(mod_name, err);
            } else if eff.kind.is_runnable() {
                // Only eager scopes reach here, so this is a class created at import
                if eff.kind == EffectKind::ClassCreation
                    && let Some((class, _)) = eff.name.split_attr()
                {
                    state.add_subclass_candidate_to_module(
                        mod_name,
                        SubclassCandidate {
                            class,
                            range: eff.range,
                            from_main_guard: eff.from_main_guard,
                        },
                    );
                }
                let mut call = Call {
                    caller_module: mod_name,
                    effect: eff,
                    func: eff.name,
                    stack: CallStack::new(*scope),
                    is_module_scope: true,
                };
                self.check_call_safety(&mut call, state, true)?;
            } else if eff.kind == EffectKind::ImportedTypeAttr {
                // Check if this is a property access
                if let Some((typ, attr)) = eff.name.split_attr() {
                    if let Some(field) = self
                        .classes
                        .lookup(&typ)
                        .and_then(|cls| cls.get_field(&attr))
                    {
                        if field.kind == FieldKind::Property {
                            let mut call = Call {
                                caller_module: mod_name,
                                effect: eff,
                                func: eff.name,
                                stack: CallStack::new(*scope),
                                is_module_scope: true,
                            };
                            self.check_call_safety(&mut call, state, true)?;
                        }
                    }
                }
            } else if eff.kind == EffectKind::UnconfirmedTypeAttr {
                // The map phase cannot tell if this access runs a property getter,
                // so it is recorded for the reduce. The whole-program path never needs
                // it: it can see every class already.
                if mode == ExecutionMode::Incremental {
                    state.add_property_candidate_to_module(
                        mod_name,
                        PropertyCandidate {
                            attribute: eff.name,
                            range: eff.range,
                            from_main_guard: eff.from_main_guard,
                        },
                    );
                }
            } else if eff.kind == EffectKind::ImportedVarMutation {
                // We only want to capture this effect as an error if it is
                // produced at global scope
                state.add_error_to_module(
                    mod_name,
                    SafetyError::new_from_effect(ErrorKind::ImportedModuleAssignment, eff),
                );
            }
        }
        Ok(())
    }
}
