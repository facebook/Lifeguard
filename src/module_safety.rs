/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;

use pyrefly_python::module_name::ModuleName;
use serde::Deserialize;
use serde::Serialize;

use crate::effects::ImportedArgs;
use crate::errors::SafetyError;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;

/// Safety verdict for a single function from call graph analysis.
///
/// Variants are ordered by conservatism:
/// `Safe` < `UnsafeIfImported` < `UnsafeMissingDep` < `Unsafe`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub enum FunctionSafety {
    /// Always safe to call.
    Safe,
    /// Safe within its own module, unsafe when called cross-module.
    UnsafeIfImported,
    /// Unsafe only because a transitive call target could not be resolved
    /// (missing dep). May be upgraded to Safe after cross-library resolution.
    UnsafeMissingDep,
    /// Always unsafe to call (intrinsic side effects).
    Unsafe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutatedParam {
    pub name: ModuleName,
    pub index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSafetyInfo {
    pub verdict: FunctionSafety,
    /// The missing cross-library callees that caused an `UnsafeMissingDep`
    /// verdict. Promotion to `Safe` requires every callee to resolve safe.
    pub missing_dep_callees: AHashSet<ModuleName>,
    /// Parameters this function (transitively) mutates, with their positional
    /// index in the signature or `None` for keyword-only matching.
    pub mutated_params: Vec<MutatedParam>,
}

impl FunctionSafetyInfo {
    pub fn new(verdict: FunctionSafety) -> Self {
        Self {
            verdict,
            missing_dep_callees: AHashSet::new(),
            mutated_params: Vec::new(),
        }
    }

    pub fn unsafe_missing_dep(callee: ModuleName) -> Self {
        Self {
            verdict: FunctionSafety::UnsafeMissingDep,
            missing_dep_callees: [callee].into_iter().collect(),
            mutated_params: Vec::new(),
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.verdict = self.verdict.max(other.verdict);
        self.missing_dep_callees.extend(other.missing_dep_callees);
        self.extend_mutated_params(other.mutated_params);
    }

    fn extend_mutated_params(&mut self, params: impl IntoIterator<Item = MutatedParam>) {
        for param in params {
            if !self
                .mutated_params
                .iter()
                .any(|existing| existing.name == param.name)
            {
                self.mutated_params.push(param);
            }
        }
    }
}

/// Where a cross-library mutation candidate occurs, and how the reduce step
/// applies it when the candidate is confirmed. The two cases are mutually exclusive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationCandidateSite {
    /// A call at module scope. On confirmation, an `ImportedVarArgument` error is added to the
    /// module. Carries the callee as written at the call site for the error metadata (the resolved
    /// `callee` may differ, e.g. a constructor's `__init__`).
    ModuleScope { call: ModuleName },
    /// A call inside a function (its module-local name). On confirmation, that function's verdict
    /// is upgraded to `Unsafe`, which propagates to its callers.
    Function { name: ModuleName },
}

/// A module-scope or in-function call that passes an imported object to a callee
/// that is unresolved in this library (a cross-library candidate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationCandidate {
    /// Resolved FQN of the callee (e.g. `setup.configure`).
    pub callee: ModuleName,
    /// Where the call occurs, and how a confirmed mutation candidate is applied.
    pub site: MutationCandidateSite,
    /// Receiver offset (implicit `self`/`cls`) for positional-arg matching.
    pub arg_offset: usize,
    /// The imported arguments passed at the call.
    pub imported_args: ImportedArgs,
}

#[derive(Debug)]
pub struct ModuleSafety {
    /// Errors that alter how a module should be imported (lazy/eager)
    pub errors: Vec<SafetyError>,
    /// Errors that alter if a module should eagerly import its imports
    pub force_imports_eager_overrides: Vec<SafetyError>,
    pub implicit_imports: Vec<ModuleName>,
    /// Per-function safety info from call graph analysis.
    /// Keys are function-local names (e.g., "helper" for `mod.helper`).
    pub function_safety: HashMap<String, FunctionSafetyInfo>,
    /// Calls passing imported objects to cross-library-unresolved callees.
    pub mutation_candidates: Vec<MutationCandidate>,
}

impl ModuleSafety {
    pub fn new() -> Self {
        Self {
            errors: Vec::new(),
            force_imports_eager_overrides: Vec::new(),
            implicit_imports: Vec::new(),
            function_safety: HashMap::new(),
            mutation_candidates: Vec::new(),
        }
    }

    pub fn is_safe(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn has_implicit_imports(&self) -> bool {
        !self.implicit_imports.is_empty()
    }

    pub fn should_load_imports_eagerly(&self) -> bool {
        !self.force_imports_eager_overrides.is_empty()
    }

    pub fn add_error(&mut self, error: SafetyError) {
        self.errors.push(error);
    }

    pub fn add_force_import_override(&mut self, error: SafetyError) {
        assert!(error.kind.requires_eager_loading_imports());
        self.force_imports_eager_overrides.push(error);
    }

    pub fn add_implicit_imports(&mut self, implicit_imports: &AHashSet<ModuleName>) {
        self.implicit_imports.extend(implicit_imports);
    }
}

#[derive(Debug)]
pub enum SafetyResult {
    Ok(ModuleSafety),
    AnalysisError(anyhow::Error),
}

impl SafetyResult {
    pub fn as_safety(&self) -> Option<&ModuleSafety> {
        match self {
            SafetyResult::Ok(safety) => Some(safety),
            _ => None,
        }
    }

    pub fn as_safety_mut(&mut self) -> Option<&mut ModuleSafety> {
        match self {
            SafetyResult::Ok(safety) => Some(safety),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ruff_text_size::TextRange;

    use super::*;
    use crate::errors::ErrorKind;

    fn make_error(kind: ErrorKind) -> SafetyError {
        SafetyError::new(kind, "test".to_owned(), TextRange::default())
    }

    #[test]
    fn new_module_safety_is_safe() {
        let safety = ModuleSafety::new();
        assert!(safety.is_safe(), "fresh ModuleSafety should be safe");
        assert!(
            !safety.has_implicit_imports(),
            "fresh ModuleSafety should have no implicit imports"
        );
        assert!(
            !safety.should_load_imports_eagerly(),
            "fresh ModuleSafety should not load imports eagerly"
        );
    }

    #[test]
    fn add_error_makes_unsafe() {
        let mut safety = ModuleSafety::new();
        safety.add_error(make_error(ErrorKind::UnsafeFunctionCall));
        assert!(!safety.is_safe(), "should be unsafe after adding an error");
        assert_eq!(safety.errors.len(), 1);
    }

    #[test]
    fn add_force_import_override_with_valid_kind() {
        let mut safety = ModuleSafety::new();
        safety.add_force_import_override(make_error(ErrorKind::ExecCall));
        assert!(
            safety.should_load_imports_eagerly(),
            "should load imports eagerly after adding ExecCall override"
        );
        assert_eq!(safety.force_imports_eager_overrides.len(), 1);
    }

    #[test]
    #[should_panic(expected = "requires_eager_loading_imports")]
    fn add_force_import_override_panics_with_wrong_kind() {
        let mut safety = ModuleSafety::new();
        safety.add_force_import_override(make_error(ErrorKind::UnsafeFunctionCall));
    }

    #[test]
    fn add_implicit_imports_sets_flag() {
        let mut safety = ModuleSafety::new();
        let mut implicits = AHashSet::new();
        implicits.insert(ModuleName::from_str("foo.bar"));
        safety.add_implicit_imports(&implicits);
        assert!(
            safety.has_implicit_imports(),
            "should have implicit imports after adding"
        );
        assert_eq!(safety.implicit_imports.len(), 1);
    }

    #[test]
    fn add_implicit_imports_empty_set() {
        let mut safety = ModuleSafety::new();
        safety.add_implicit_imports(&AHashSet::new());
        assert!(
            !safety.has_implicit_imports(),
            "should have no implicit imports after adding empty set"
        );
    }

    #[test]
    fn safety_result_ok_as_safety() {
        let result = SafetyResult::Ok(ModuleSafety::new());
        assert!(
            result.as_safety().is_some(),
            "Ok variant should return Some from as_safety"
        );
    }

    #[test]
    fn safety_result_ok_as_safety_mut() {
        let mut result = SafetyResult::Ok(ModuleSafety::new());
        let safety = result.as_safety_mut().unwrap();
        safety.add_error(make_error(ErrorKind::ExecCall));
        assert!(
            !result.as_safety().unwrap().is_safe(),
            "mutation through as_safety_mut should be visible"
        );
    }

    #[test]
    fn safety_result_analysis_error_as_safety_returns_none() {
        let result = SafetyResult::AnalysisError(anyhow::anyhow!("parse failure"));
        assert!(
            result.as_safety().is_none(),
            "AnalysisError should return None from as_safety"
        );
    }

    #[test]
    fn safety_result_analysis_error_as_safety_mut_returns_none() {
        let mut result = SafetyResult::AnalysisError(anyhow::anyhow!("parse failure"));
        assert!(
            result.as_safety_mut().is_none(),
            "AnalysisError should return None from as_safety_mut"
        );
    }

    #[test]
    fn multiple_errors_accumulate() {
        let mut safety = ModuleSafety::new();
        safety.add_error(make_error(ErrorKind::UnsafeFunctionCall));
        safety.add_error(make_error(ErrorKind::UnhandledException));
        safety.add_force_import_override(make_error(ErrorKind::CustomFinalizer));
        safety.add_force_import_override(make_error(ErrorKind::SysModulesAccess));
        assert_eq!(safety.errors.len(), 2);
        assert_eq!(safety.force_imports_eager_overrides.len(), 2);
    }

    #[test]
    fn all_eager_loading_kinds_accepted() {
        let mut safety = ModuleSafety::new();
        safety.add_force_import_override(make_error(ErrorKind::CustomFinalizer));
        safety.add_force_import_override(make_error(ErrorKind::ExecCall));
        safety.add_force_import_override(make_error(ErrorKind::SysModulesAccess));
        assert_eq!(safety.force_imports_eager_overrides.len(), 3);
    }

    #[test]
    fn function_safety_merge_dedupes_mutated_params() {
        let mut info = FunctionSafetyInfo::new(FunctionSafety::Safe);
        info.mutated_params.push(MutatedParam {
            name: ModuleName::from_str("arg"),
            index: Some(0),
        });

        let mut other = FunctionSafetyInfo::new(FunctionSafety::UnsafeIfImported);
        other.mutated_params.push(MutatedParam {
            name: ModuleName::from_str("arg"),
            index: Some(0),
        });
        other.mutated_params.push(MutatedParam {
            name: ModuleName::from_str("other"),
            index: Some(1),
        });

        info.merge(other);

        assert_eq!(info.verdict, FunctionSafety::UnsafeIfImported);
        assert_eq!(
            info.mutated_params,
            vec![
                MutatedParam {
                    name: ModuleName::from_str("arg"),
                    index: Some(0),
                },
                MutatedParam {
                    name: ModuleName::from_str("other"),
                    index: Some(1),
                },
            ]
        );
    }
}
