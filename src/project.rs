/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Whole-project analysis, from per-module effects to safety verdicts.
//!
//! - [`analysis`] owns parallel module analysis and the pipeline entry points.
//! - [`implicit_imports`] computes implicit and side-effect imports.
//! - [`info`] merges immutable project facts and resolves callable names.
//! - [`call_graph`] orders safety precomputation by callee dependencies.
//! - [`calls`] checks call-site safety and traverses callable effects.
//! - [`mutations`] propagates parameter mutations and records deferred candidates.
//! - [`safety`] collects module diagnostics and resolves whole-program verdicts.
//! - [`state`] stores shared concurrent diagnostics and callable verdicts.

mod analysis;
mod builder;
mod call_graph;
mod calls;
mod implicit_imports;
mod info;
mod mutations;
mod safety;
mod state;
mod types;

pub use crate::module_safety::FunctionSafety;
pub use crate::project::analysis::analyze_all;
pub use crate::project::analysis::run_analysis;
pub use crate::project::types::AnalysisMap;
pub use crate::project::types::AnalysisOutput;
pub use crate::project::types::ExecutionMode;
pub use crate::project::types::ParseErrors;
pub use crate::project::types::SafetyMap;
pub use crate::project::types::SideEffectMap;
