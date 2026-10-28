/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Concurrent module/callable records and updates, consumed as owned maps by analysis.

use anyhow::Error;
use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;

use crate::errors::SafetyError;
use crate::hasher::AHashSet;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::ModuleSafety;
use crate::module_safety::PropertyCandidate;
use crate::module_safety::SafetyResult;
use crate::module_safety::SubclassCandidate;
use crate::project::types::AnalysisMap;
use crate::project::types::SafetyMap;

// Concurrent records shared by evaluation, scheduling, and eager diagnostics.
pub(super) struct GlobalAnalysisState {
    safety_map: SafetyMap,
    pub(super) function_safety: DashMap<ModuleName, FunctionSafetyInfo>,
}

impl GlobalAnalysisState {
    pub(super) fn new(analysis_map: &AnalysisMap) -> Self {
        let state = Self {
            safety_map: SafetyMap::new(),
            function_safety: DashMap::new(),
        };
        state.init_safety_map(analysis_map);
        state
    }

    /// Pre-populate the safety_map with empty ModuleSafety for each module
    fn init_safety_map(&self, analysis_map: &AnalysisMap) {
        for mod_name in analysis_map.keys() {
            self.safety_map
                .insert(*mod_name, SafetyResult::Ok(ModuleSafety::new()));
        }
    }

    pub(super) fn into_parts(self) -> (SafetyMap, DashMap<ModuleName, FunctionSafetyInfo>) {
        (self.safety_map, self.function_safety)
    }

    pub(super) fn set_analysis_error(&self, mod_name: &ModuleName, error: Error) {
        self.safety_map
            .insert(*mod_name, SafetyResult::AnalysisError(error));
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
