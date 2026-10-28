/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Shared concurrent storage for module diagnostics and callable safety verdicts.

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::errors::SafetyError;
use crate::hasher::AHashSet;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::ModuleSafety;
use crate::module_safety::PropertyCandidate;
use crate::module_safety::SafetyResult;
use crate::module_safety::SubclassCandidate;
use crate::names::enclosing_module_str;
use crate::project::AnalysisMap;
use crate::project::ExecutionMode;
use crate::project::SafetyMap;
use crate::project::info::ProjectInfo;

// Collects whole-project analysis output, as well as any global state that is required while
// traversing the analysis map. Uses DashMap for concurrent access from multiple threads.
pub(super) struct GlobalAnalysisState {
    pub(super) safety_map: SafetyMap,
    pub(super) function_safety: DashMap<ModuleName, FunctionSafetyInfo>,
}

impl GlobalAnalysisState {
    pub(super) fn new() -> Self {
        Self {
            safety_map: SafetyMap::new(),
            function_safety: DashMap::new(),
        }
    }

    /// Pre-populate the safety_map with empty ModuleSafety for each module
    pub(super) fn init_safety_map(&self, analysis_map: &AnalysisMap) {
        for mod_name in analysis_map.keys() {
            self.safety_map
                .insert(*mod_name, SafetyResult::Ok(ModuleSafety::new()));
        }
    }

    /// Decompose FQN-keyed function safety verdicts into per-module local-name
    /// maps and embed them in the corresponding ModuleSafety entries.
    pub(super) fn into_safety_map(self, mode: ExecutionMode, project: &ProjectInfo) -> SafetyMap {
        let Self {
            safety_map,
            function_safety,
        } = self;
        if mode == ExecutionMode::Incremental {
            let modules: Vec<ModuleName> = safety_map.iter().map(|entry| *entry.key()).collect();
            let module_strs: AHashSet<&str> = modules.iter().map(|m| m.as_str()).collect();

            // Consumed rather than iterated by reference: every verdict is moved
            // into the module that owns it, so none of them is cloned.
            function_safety.into_par_iter().for_each(|(fqn, mut info)| {
                let Some((module, local_name)) =
                    enclosing_module_str(fqn.as_str(), |p| module_strs.contains(p))
                else {
                    return;
                };
                if let Some(mut safety_entry) = safety_map.get_mut(&module) {
                    if let SafetyResult::Ok(module_safety) = safety_entry.value_mut() {
                        if let Some(mutated) = project.resolve_cached_mutated_params_for(&fqn) {
                            info.mutated_params = mutated;
                        }
                        module_safety
                            .function_safety
                            .insert(local_name.to_string(), info);
                    }
                }
            });
        }
        safety_map
    }

    pub(super) fn add_error_to_module(&self, mod_name: &ModuleName, err: SafetyError) {
        self.update_module_safety(mod_name, |safety| safety.add_error(err));
    }

    pub(super) fn add_property_candidate_to_module(
        &self,
        mod_name: &ModuleName,
        candidate: PropertyCandidate,
    ) {
        self.update_module_safety(mod_name, |safety| {
            safety.property_candidates.push(candidate)
        });
    }

    pub(super) fn add_subclass_candidate_to_module(
        &self,
        mod_name: &ModuleName,
        candidate: SubclassCandidate,
    ) {
        self.update_module_safety(mod_name, |safety| {
            safety.subclass_candidates.push(candidate)
        });
    }

    pub(super) fn add_force_imports_eager_override_to_module(
        &self,
        mod_name: &ModuleName,
        err: SafetyError,
    ) {
        self.update_module_safety(mod_name, |safety| safety.add_force_import_override(err));
    }

    pub(super) fn add_implicit_imports_to_module(
        &self,
        mod_name: &ModuleName,
        implicit_imports: &AHashSet<ModuleName>,
    ) {
        self.update_module_safety(mod_name, |safety| {
            safety.add_implicit_imports(implicit_imports)
        });
    }

    fn update_module_safety(&self, mod_name: &ModuleName, update: impl FnOnce(&mut ModuleSafety)) {
        let mut entry = self
            .safety_map
            .entry(*mod_name)
            .or_insert_with(|| SafetyResult::Ok(ModuleSafety::new()));
        if let SafetyResult::Ok(safety) = entry.value_mut() {
            update(safety);
        }
    }

    /// Replace `func`'s verdict outright, discarding anything recorded before.
    pub(super) fn set_function_safety(&self, func: &ModuleName, info: FunctionSafetyInfo) {
        self.function_safety.insert(*func, info);
    }

    pub(super) fn mark_safe(&self, func: &ModuleName) {
        self.set_function_safety(func, FunctionSafetyInfo::new(FunctionSafety::Safe));
    }

    pub(super) fn mark_unsafe(&self, func: &ModuleName) {
        self.set_function_safety(func, FunctionSafetyInfo::new(FunctionSafety::Unsafe));
    }

    pub(super) fn mark_unsafe_missing_dep(&self, func: &ModuleName, callee: &ModuleName) {
        self.function_safety
            .entry(*func)
            .and_modify(|info| {
                info.verdict.insert(FunctionSafety::UnsafeMissingDep);
                info.missing_dep_callees.insert(*callee);
            })
            .or_insert_with(|| FunctionSafetyInfo::unsafe_missing_dep(*callee));
    }

    pub(super) fn is_unsafe(&self, func: &ModuleName) -> bool {
        self.function_safety
            .get(func)
            .is_some_and(|info| info.verdict.has(FunctionSafety::Unsafe))
    }

    pub(super) fn mark_unsafe_if_imported(&self, func: &ModuleName) {
        self.function_safety
            .entry(*func)
            .and_modify(|info| {
                info.verdict.insert(FunctionSafety::UnsafeIfImported);
            })
            .or_insert_with(|| FunctionSafetyInfo::new(FunctionSafety::UnsafeIfImported));
    }
}
