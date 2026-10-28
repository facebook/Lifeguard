/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Shared analysis inputs, outputs, and execution mode.

use std::collections::HashMap;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;

use crate::analyzer::AnalyzedModule;
use crate::cache::ConstructorCallees;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::module_safety::SafetyResult;

pub type AnalysisMap = HashMap<ModuleName, AnalyzedModule, ahash::RandomState>;
pub type SafetyMap = DashMap<ModuleName, SafetyResult>;
pub type SideEffectMap = AHashMap<ModuleName, AHashSet<ModuleName>>;
pub type ParseErrors = DashMap<ModuleName, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Runs a map/reduce pipeline with per-library results cached in the map phase,
    /// so that we can perform an incremental analysis when something changes.
    Incremental,
    /// Single-pass whole-program analysis, does not cache anything.
    WholeProgram,
}

/// Collected output from the analysis pipeline.
pub struct AnalysisOutput {
    pub safety_map: SafetyMap,
    pub side_effect_imports: SideEffectMap,
    pub parse_errors: ParseErrors,
    /// Class FQN -> base-class FQNs, for reduce-time MRO resolution of inherited
    /// `Class.method` calls.
    pub class_bases: Vec<(ModuleName, Vec<ModuleName>)>,
    /// Class FQN -> the functions a constructor call to it dispatches to,
    /// resolved here where the class table is complete.
    pub constructor_callees: Vec<(ModuleName, ConstructorCallees)>,
    /// Class FQN -> property field names, for resolving property candidates.
    pub class_properties: Vec<(ModuleName, Vec<String>)>,
}
