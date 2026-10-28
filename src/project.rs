/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Whole-project analysis, from per-module effects to safety verdicts.
//!
//! - [`analysis`] owns parallel module analysis, orchestration, and final projection.
//! - [`types`] defines shared analysis data and execution mode.
//! - [`implicit_imports`] computes implicit and side-effect imports.
//! - [`info`] stores immutable project facts and provides enclosing-module lookup.
//! - [`builder`] constructs facts and computes derived mutation data.
//! - [`dispatch`] enumerates syntactic candidates and resolves callable/MRO dispatch.
//! - [`call_graph`] orders safety precomputation by callee dependencies.
//! - [`calls`] evaluates callable effects and call-site mutation diagnostics.
//! - [`mutations`] queries mutations, propagates parameters, and collects candidates.
//! - [`safety`] collects module diagnostics and resolves whole-program verdicts.
//! - [`state`] stores concurrent records and exposes consuming owned maps.

mod analysis;
mod builder;
mod call_graph;
mod calls;
mod dispatch;
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
