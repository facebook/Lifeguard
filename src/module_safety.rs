/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;

use ahash::AHashSet;
use pyrefly_python::module_name::ModuleName;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::SafetyError;

/// Safety verdict for a single function from call graph analysis.
///
/// Variants are ordered by conservatism: `Safe` < `UnsafeIfImported` < `Unsafe`.
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
    /// Always unsafe to call.
    Unsafe,
}

#[derive(Debug)]
pub struct ModuleSafety {
    /// Errors that alter how a module should be imported (lazy/eager)
    pub errors: Vec<SafetyError>,
    /// Errors that alter if a module should eagerly import its imports
    pub force_imports_eager_overrides: Vec<SafetyError>,
    pub implicit_imports: Vec<ModuleName>,
    /// Per-function safety verdicts from call graph analysis.
    /// Keys are function-local names (e.g., "helper" for `mod.helper`).
    pub function_safety: HashMap<String, FunctionSafety>,
}

impl ModuleSafety {
    pub fn new() -> Self {
        Self {
            errors: Vec::new(),
            force_imports_eager_overrides: Vec::new(),
            implicit_imports: Vec::new(),
            function_safety: HashMap::new(),
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
}
