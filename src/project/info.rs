/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Immutable project facts and callable resolution through re-exports and the MRO.

use std::collections::HashMap;
use std::mem;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::cache::CONSTRUCTOR_METHODS;
use crate::cache::ConstructorCallees;
use crate::cache::constructor_mask_bits;
use crate::class::Class;
use crate::class::ClassTable;
use crate::effects::Effect;
use crate::effects::EffectKind;
use crate::effects::EffectTable;
use crate::exports::Exports;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::mro::c3_linearize;
use crate::names::enclosing_module;
use crate::project::mutations::compute_mutated_params;
use crate::project::types::AnalysisMap;
use crate::tracing::time;
use crate::traits::ModuleNameExt;

// Merge effects from all modules into a single effect table.
//
// Class bodies within functions are eager: they execute when the enclosing function runs.
// We bubble their effects to the nearest enclosing function scope so that calling the
// function correctly surfaces those effects.
//
// Nested function effects are NOT bubbled here. For regular function calls, the call
// graph handles them: if a nested function is called within its parent, the FunctionCall
// effect triggers check_call_body() to examine the nested function's effects. For
// decorator calls, check_call_body separately checks nested function effects since
// decorator application calls the returned function.
fn merge_all_effects(analysis_map: &AnalysisMap) -> EffectTable {
    // One entry per scope with effects (~10 scopes/module).
    let num_modules = analysis_map.len();
    let concurrent_table: DashMap<ModuleName, Vec<Effect>> =
        DashMap::with_capacity(num_modules * 10);

    // Process analysis_map in parallel
    analysis_map.par_iter().for_each(|(_, v)| {
        // Merge module effects
        for (scope, effects) in v.module_effects.effects.iter() {
            let effs = effects.iter().cloned();
            concurrent_table.entry(*scope).or_default().extend(effs);
        }

        // Bubble eager scope (class body) effects to their enclosing function.
        // Nested function effects are NOT bubbled — the call graph handles them.
        for (scope, effects) in v.module_effects.effects.iter() {
            if let Some(parent) = v.definitions.enclosing_functions.get(scope) {
                if !v.definitions.is_function_scope(scope) {
                    let effs = effects.iter().cloned();
                    concurrent_table.entry(*parent).or_default().extend(effs);
                }
            }
        }
    });

    // Convert DashMap back to EffectTable with pre-allocated AHashMap
    let mut table: AHashMap<ModuleName, Vec<Effect>> =
        AHashMap::with_capacity(concurrent_table.len());
    for (k, v) in concurrent_table.into_iter() {
        table.insert(k, v);
    }
    EffectTable::new(table)
}

fn merge_all_classes(analysis_map: &mut AnalysisMap) -> ClassTable {
    // Move all per-module class tables out of the analysis map (cheap pointer swaps)
    let class_tables: Vec<ClassTable> = analysis_map
        .values_mut()
        .map(|v| mem::replace(&mut v.classes, ClassTable::empty()))
        .collect();

    // Merge in parallel, pre-allocating ~5 classes per module based on profiling
    let concurrent_table: DashMap<ModuleName, Class> =
        DashMap::with_capacity(analysis_map.len() * 5);
    class_tables.into_par_iter().for_each(|ct| {
        for (name, class) in ct.into_inner() {
            concurrent_table.insert(name, class);
        }
    });

    ClassTable::new(concurrent_table.into_iter().collect())
}

fn build_nested_functions_map(analysis_map: &AnalysisMap) -> AHashMap<ModuleName, Vec<ModuleName>> {
    // Build per-thread maps in parallel, then merge. The consumer ANDs over all
    // children, so the merged child order does not affect results.
    analysis_map
        .par_iter()
        .fold(
            AHashMap::new,
            |mut map: AHashMap<ModuleName, Vec<ModuleName>>, (_, v)| {
                for (child, parent) in &v.definitions.enclosing_functions {
                    // Keep only immediate children; deeper wrappers run later.
                    if v.definitions.is_function_scope(child)
                        && child.parent().as_ref() == Some(parent)
                    {
                        map.entry(*parent).or_default().push(*child);
                    }
                }
                map
            },
        )
        .reduce(AHashMap::new, |mut acc, map| {
            for (parent, mut children) in map {
                acc.entry(parent).or_default().append(&mut children);
            }
            acc
        })
}

fn merge_all_functions_and_methods(
    analysis_map: &AnalysisMap,
) -> (
    AHashMap<ModuleName, ModuleName>,
    AHashMap<ModuleName, ModuleName>,
) {
    let func_pairs: Vec<(ModuleName, ModuleName)> = analysis_map
        .par_iter()
        .flat_map_iter(|(mod_name, v)| {
            v.definitions
                .function_scopes()
                .map(|func| (*func, *mod_name))
        })
        .collect();

    let method_pairs: Vec<(ModuleName, ModuleName)> = analysis_map
        .par_iter()
        .flat_map_iter(|(_, v)| {
            v.module_effects
                .indirectly_called_methods
                .iter()
                .map(|(def, source)| (*def, *source))
        })
        .collect();

    (
        func_pairs.into_iter().collect(),
        method_pairs.into_iter().collect(),
    )
}

fn collect_re_exports(
    exports: &Exports,
    effect_table: &EffectTable,
    functions: &AHashMap<ModuleName, ModuleName>,
    classes: &ClassTable,
) -> AHashMap<ModuleName, ModuleName> {
    // Re-export names are interned via as_module_name(), which is the expensive
    // part on large projects; do it in parallel, then build the map.
    let entries: Vec<(ModuleName, ModuleName)> = exports
        .par_re_exports()
        .map(|(module, attr, (imported, _))| {
            let name = module.append_str(attr.as_str());
            let target = exports
                .resolve_transitive(imported)
                .map(|source| source.as_module_name())
                .filter(|source| functions.contains_key(source) || classes.contains(source))
                .unwrap_or(name);
            (name, target)
        })
        .collect();
    let mut re_exports: AHashMap<ModuleName, ModuleName> = entries.into_iter().collect();
    remove_unsafe_re_exports(effect_table, &mut re_exports);
    re_exports
}

fn remove_unsafe_re_exports(
    effect_table: &EffectTable,
    re_exports: &mut AHashMap<ModuleName, ModuleName>,
) {
    let unsafe_re_exports = effect_table
        .values()
        .flatten()
        .filter(|eff| eff.kind == EffectKind::ImportedVarReassignment);

    for unsafe_re_export in unsafe_re_exports {
        re_exports.remove(&unsafe_re_export.name);
    }
}

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
    methods: AHashMap<ModuleName, ModuleName>,
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
    pub(super) fn new(mut analysis_map: AnalysisMap, exports: &Exports) -> Self {
        let (effect_table, (functions, methods)) = time("    Merging effects + functions", || {
            rayon::join(
                || merge_all_effects(&analysis_map),
                || merge_all_functions_and_methods(&analysis_map),
            )
        });
        let classes = time("    Merging all classes", || {
            merge_all_classes(&mut analysis_map)
        });
        let class_bases = time("    Indexing class bases", || {
            classes.base_edges().into_iter().collect()
        });
        let ((re_exports, nested_functions), mutated_params) = rayon::join(
            || {
                time("    Getting re-exports + nested fns", || {
                    rayon::join(
                        || collect_re_exports(exports, &effect_table, &functions, &classes),
                        || build_nested_functions_map(&analysis_map),
                    )
                })
            },
            || {
                time("    Computing mutated params", || {
                    compute_mutated_params(&effect_table, &functions, &classes, &analysis_map)
                })
            },
        );
        Self {
            analysis_map,
            effect_table,
            classes,
            class_bases,
            functions,
            re_exports,
            methods,
            nested_functions,
            mutated_params,
        }
    }

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
