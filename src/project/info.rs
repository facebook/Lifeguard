/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Immutable project facts and callable resolution through re-exports and the MRO.

use std::collections::HashMap;

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::cache::CONSTRUCTOR_METHODS;
use crate::cache::ConstructorCallees;
use crate::cache::constructor_mask_bits;
use crate::class::ClassTable;
use crate::effects::EffectTable;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::mro::c3_linearize;
use crate::names::enclosing_module;
use crate::project::types::AnalysisMap;
use crate::traits::ModuleNameExt;

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

/// What `type.__call__` runs on the metaclass when a `class` statement creates a class.
const METACLASS_CREATION_METHODS: [&str; 2] = ["__new__", "__init__"];

/// The functions a constructor call to `cls_name` may dispatch to, in the
/// order `check_constructor_call` checks them: `CONSTRUCTOR_METHODS` on the
/// metaclass (if the class has one), then the same set on the class itself.
/// Mirrors the candidate set `ConstructorCallees` masks, so the direct and
/// cache-backed paths dispatch to the same callees.
/// TODO: Look up `__init__` in the MRO.
pub(super) fn constructor_method_names(
    cls_name: ModuleName,
    classes: &ClassTable,
) -> impl Iterator<Item = ModuleName> {
    let metaclass = classes.lookup(&cls_name).and_then(|cls| cls.metaclass);
    metaclass
        .into_iter()
        .flat_map(|mcls| CONSTRUCTOR_METHODS.map(|method| mcls.append_str(method)))
        .chain(CONSTRUCTOR_METHODS.map(|method| cls_name.append_str(method)))
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

impl ProjectInfo {
    pub(super) fn contains_callable(&self, name: &ModuleName) -> bool {
        if self.functions.contains_key(name) || self.classes.contains(name) {
            return true;
        }
        let call_name = self.methods.get(name).copied().unwrap_or(*name);
        if self.functions.contains_key(&call_name) || self.classes.contains(&call_name) {
            true
        } else {
            self.re_exports.contains_key(&call_name)
        }
    }

    pub(super) fn resolve_callable(&self, name: &ModuleName) -> Option<ModuleName> {
        if self.functions.contains_key(name) || self.classes.contains(name) {
            return Some(*name);
        }
        let call_name = self.methods.get(name).copied().unwrap_or(*name);
        if self.functions.contains_key(&call_name) || self.classes.contains(&call_name) {
            return Some(call_name);
        }
        if let Some(target) = self.re_exports.get(&call_name) {
            // A re-exported name has no effects of its own; its definition does.
            return Some(*target);
        }
        let (class, method) = name.split_attr()?;
        if !self.classes.contains(&class) {
            return None;
        }
        self.resolve_inherited(&class, method.as_str())
    }

    /// The nearest ancestor's definition of `method`, skipping `class`'s own.
    fn resolve_inherited(&self, class: &ModuleName, method: &str) -> Option<ModuleName> {
        // Classes without bases inherit nothing, and c3_linearize allocates a
        // memo per call, so skipping them keeps this off the hot path.
        if !self.class_bases.contains_key(class) {
            return None;
        }
        c3_linearize(&self.class_bases, class)
            .into_iter()
            .skip(1)
            .map(|base| base.append_str(method))
            .find(|candidate| self.contains_callable(candidate))
    }

    /// The definition an implicit dispatch above `name`'s class runs: `name` is
    /// a method on the class the statement is written in, and lookup starts
    /// above it. Zero-argument `super()` and class creation both dispatch this
    /// way — the latter because `type.__new__` calls
    /// `super().__init_subclass__`.
    pub(super) fn resolve_dispatch_above(&self, name: &ModuleName) -> Option<ModuleName> {
        let (class, method) = name.split_attr()?;
        self.resolve_inherited(&class, method.as_str())
    }

    /// The metaclass a `class` statement calls: the first `metaclass=` declared
    /// along its MRO, which starts with the class itself.
    fn creation_metaclass(&self, class: &ModuleName) -> Option<ModuleName> {
        // skip linearizing a class without bases, as `resolve_inherited` does
        if !self.class_bases.contains_key(class) {
            return self.classes.lookup(class)?.metaclass;
        }
        c3_linearize(&self.class_bases, class)
            .iter()
            .find_map(|cls| self.classes.lookup(cls)?.metaclass)
    }

    /// The metaclass methods a `class` statement runs. `name` is the
    /// `<class>.__init_subclass__` a `ClassCreation` effect is recorded under.
    pub(super) fn metaclass_creation_callees(&self, name: &ModuleName) -> Vec<ModuleName> {
        let Some((class, _)) = name.split_attr() else {
            return Vec::new();
        };
        self.creation_metaclass(&class)
            .into_iter()
            .flat_map(|metaclass| {
                METACLASS_CREATION_METHODS
                    .iter()
                    .filter_map(move |method| self.resolve_callable(&metaclass.append_str(method)))
            })
            .collect()
    }

    /// Every class paired with the functions its constructor call dispatches to,
    /// keeping only those that resolve to something this analysis can see. An
    /// entry with no resolvable callee is dropped: reduce must fall back to its
    /// own lookup there rather than read the empty list as "nothing runs".
    pub(super) fn resolved_constructor_callees(&self) -> Vec<(ModuleName, ConstructorCallees)> {
        self.classes
            .par_keys()
            .filter_map(|cls_name| {
                let recorded = self.resolve_constructor_callees(*cls_name);
                (!recorded.is_empty()).then_some((*cls_name, recorded))
            })
            .collect()
    }

    /// Which of `cls_name`'s candidate constructor callees resolve to something
    /// this analysis can see. The metaclass is recorded only when one of its
    /// methods resolved, so an entry never carries a name none of its bits use.
    fn resolve_constructor_callees(&self, cls_name: ModuleName) -> ConstructorCallees {
        let metaclass = self.classes.lookup(&cls_name).and_then(|cls| cls.metaclass);
        let metaclass_mask = metaclass.map_or(0, |mcls| {
            constructor_mask_bits(mcls, 0, &CONSTRUCTOR_METHODS, |m| self.contains_callable(m))
        });
        let own_mask = constructor_mask_bits(
            cls_name,
            CONSTRUCTOR_METHODS.len(),
            &CONSTRUCTOR_METHODS,
            |m| self.contains_callable(m),
        );
        ConstructorCallees {
            metaclass: (metaclass_mask != 0).then_some(metaclass).flatten(),
            mask: metaclass_mask | own_mask,
            extra: self.inherited_constructor_callees(cls_name),
        }
    }

    /// The constructor callees the mask cannot name: the ones a class inherits,
    /// whose owner is neither the class nor its metaclass. `resolve_callable`
    /// walks the MRO, so this names the ancestor that actually defines the
    /// method rather than the first base that mentions it.
    fn inherited_constructor_callees(&self, cls_name: ModuleName) -> Vec<ModuleName> {
        CONSTRUCTOR_METHODS
            .iter()
            .filter_map(|method| {
                let own = cls_name.append_str(method);
                let resolved = self.resolve_callable(&own)?;
                (resolved != own).then_some(resolved)
            })
            .collect()
    }

    pub(super) fn constructor_methods(
        &self,
        cls_name: ModuleName,
    ) -> impl Iterator<Item = ModuleName> {
        constructor_method_names(cls_name, &self.classes)
    }
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
