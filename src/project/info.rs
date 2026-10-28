/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Immutable project facts and enclosing-module name lookup.

use std::collections::HashMap;

use pyrefly_python::module_name::ModuleName;

use crate::class::ClassTable;
use crate::effects::EffectTable;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::names::enclosing_module;
use crate::project::types::AnalysisMap;

/// Split a scope FQN into the module that contains it and its module-local name.
/// `is_module` identifies which names name a module. Returns `None` when no
/// ancestor is a module.
///
/// The search is over proper ancestors, so a scope that is itself a module
/// splits against its parent package: `pkg.mod` becomes `(pkg, "mod")`, consistent
/// with `SafetyResolver::split_at_module`
///
/// Callers that need to distinguish "this scope is a module" from "this scope is
/// nested in one" should ask `is_module` directly; see `mutation_candidate_scope`.
pub(super) fn resolve_enclosing_module<'a>(
    scope: &'a ModuleName,
    is_module: impl Fn(&ModuleName) -> bool,
) -> Option<(ModuleName, &'a str)> {
    enclosing_module(scope.as_str(), is_module)
}

// Immutable global information derived from the project
pub(super) struct ProjectInfo {
    pub(super) analysis_map: AnalysisMap,
    pub(super) effect_table: EffectTable,
    pub(super) classes: ClassTable,
    pub(super) class_bases: HashMap<ModuleName, Vec<ModuleName>>,
    // Mappings of functions to the containing module
    pub(super) functions: AHashMap<ModuleName, ModuleName>,
    // Re-exported name -> the function or class it resolves to, or the name itself
    // when this analysis has no such definition.
    pub(super) re_exports: AHashMap<ModuleName, ModuleName>,
    // Mapping of all methods called on imported objects
    pub(super) methods: AHashMap<ModuleName, ModuleName>,
    // Reverse mapping: parent function → nested function scopes.
    // Used by check_call_body to check nested function effects for decorator calls.
    pub(super) nested_functions: AHashMap<ModuleName, Vec<ModuleName>>,
    // Per-function set of parameter names that are method-mutated, directly or by
    // forwarding to another function's mutated parameter (transitive closure).
    // Used to detect passing imported state to a mutated parameter, even through
    // intermediate forwarding functions.
    pub(super) mutated_params: AHashMap<ModuleName, AHashSet<ModuleName>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_that_is_itself_a_module_splits_against_its_parent() {
        // `package.child` is both a module and a scope. It has to decompose the
        // way `SafetyResolver::split_at_module` will decompose it on the way back
        // out, which is against the parent package.
        let package = ModuleName::from_str("package");
        let scope = ModuleName::from_str("package.child");
        let modules = AHashSet::from_iter([package, scope]);

        assert_eq!(
            resolve_enclosing_module(&scope, |name| modules.contains(name)),
            Some((package, "child")),
        );
    }

    #[test]
    fn a_scope_with_no_module_ancestor_does_not_resolve() {
        let scope = ModuleName::from_str("orphan.scope");
        let modules = AHashSet::from_iter([ModuleName::from_str("unrelated")]);

        assert_eq!(
            resolve_enclosing_module(&scope, |name| modules.contains(name)),
            None,
        );
    }

    #[test]
    fn a_module_scope_with_no_module_parent_does_not_resolve() {
        let scope = ModuleName::from_str("package.child");
        let modules = AHashSet::from_iter([scope]);

        assert_eq!(
            resolve_enclosing_module(&scope, |name| modules.contains(name)),
            None,
        );
    }
}
