/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Construction of immutable project facts and derived mutation data.

use std::mem;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::class::Class;
use crate::class::ClassTable;
use crate::effects::Effect;
use crate::effects::EffectKind;
use crate::effects::EffectTable;
use crate::exports::Exports;
use crate::hasher::AHashMap;
use crate::hasher::HashMapExt;
use crate::project::info::ProjectInfo;
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
}
