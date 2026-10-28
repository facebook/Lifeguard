/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Imports loaded implicitly or solely for their side effects.

use std::collections::HashMap;
use std::sync::LazyLock;

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::module_effects::ModuleEffects;
use crate::module_effects::ModuleImportsMap;
use crate::project::AnalysisMap;
use crate::project::SideEffectMap;
use crate::traits::ModuleNameExt;

/// Per module, the extra imports transitively loaded by functions it calls.
type AdditionalCalledImports = AHashMap<ModuleName, AHashSet<ModuleName>>;

/// Shared immutable context for computing implicit imports.
struct ImplicitImportContext<'a> {
    analysis_map: &'a AnalysisMap,
    additional_called_imports: &'a AdditionalCalledImports,
    init_module_map: &'a HashMap<ModuleName, ModuleName>,
    import_graph: &'a ImportGraph,
}

/// Compute side-effect imports: module-level imports never accessed in any scope.
/// These are imports that exist solely for their side effects (e.g., decorator registration).
/// Lazy imports that are never accessed have no side effects, so they are excluded.
pub(super) fn compute_side_effect_imports(analysis_map: &AnalysisMap) -> SideEffectMap {
    let results: Vec<_> = analysis_map
        .par_iter()
        .map(|(module_name, output)| {
            let Some(module_pending) = output.module_effects.pending_imports.get(module_name)
            else {
                return (*module_name, AHashSet::new());
            };

            let side_effects: AHashSet<ModuleName> = module_pending
                .difference(&output.module_effects.all_called_import_names)
                .filter(|m| output.module_effects.eager_imports.contains(m))
                .copied()
                .collect();

            (*module_name, side_effects)
        })
        .collect();

    results.into_iter().collect()
}

fn collect_parent_module_imports(
    curr_import: &ModuleName,
    analysis_map: &AnalysisMap,
) -> AHashSet<ModuleName> {
    let Some(output) = analysis_map.get(curr_import) else {
        return AHashSet::new();
    };
    let called_map = &output.module_effects.called_imports;
    let Some(imports_to_load) = called_map.get(curr_import) else {
        return AHashSet::new();
    };
    if let Some(parent_pending_import) = output.module_effects.pending_imports.get(curr_import) {
        parent_pending_import
            .intersection(imports_to_load)
            .copied()
            .collect()
    } else {
        AHashSet::new()
    }
}

fn collect_imports_in_function_module(
    curr_import: &ModuleName,
    analysis_map: &AnalysisMap,
) -> AHashSet<ModuleName> {
    let mut additional_called_imports = AHashSet::new();

    // Use the first parent module present in the analysis map. For
    // curr_import = "foo.bar.baz.func", parents are checked longest-first:
    // foo.bar.baz, foo.bar, foo.
    let Some((parent_name, output)) = curr_import
        .iter_parents()
        .find_map(|(parent, _)| analysis_map.get(&parent).map(|output| (parent, output)))
    else {
        return additional_called_imports;
    };

    // All three sets come from this single matched module, so read them by
    // reference rather than cloning.
    let module_pending_imports = &output.module_effects.pending_imports;
    let other_module_level_import = module_pending_imports.get(&parent_name);
    let function_pending_import = module_pending_imports.get(curr_import);
    let function_called_import = output.module_effects.called_imports.get(curr_import);

    if let Some(function_called_import) = function_called_import {
        for import in function_called_import {
            let in_pending = function_pending_import.is_some_and(|s| s.contains(import));
            let in_module = other_module_level_import.is_some_and(|s| s.contains(import));
            if in_pending || in_module {
                additional_called_imports.insert(*import);
            }
        }
    }
    if let Some(function_pending_import) = function_pending_import {
        additional_called_imports.extend(function_pending_import.iter().copied());
    }

    additional_called_imports
}

fn is_called_attribute_loaded(
    curr_import: &ModuleName,
    all_pending_imports: &AHashSet<ModuleName>,
    import_graph: &ImportGraph,
) -> bool {
    // Check if the called import is an attribute, if so it's not implicit
    if import_graph.contains(curr_import) {
        return false;
    }
    for (parent_name, _) in curr_import.iter_parents() {
        if all_pending_imports.contains(&parent_name) {
            return true;
        }
    }
    false
}

fn fill_import_as_map(
    pending_module: &ModuleName,
    top_level_imports: &AHashSet<ModuleName>,
    module_pending_imports: &ModuleImportsMap,
    import_as_map: &mut AHashMap<ModuleName, AHashSet<ModuleName>>,
) {
    // Check for modules that are imported as attributes
    for import in top_level_imports {
        let parts = vec![pending_module.as_str(), import.as_str()];
        let key = ModuleName::from_parts(parts);
        let entry = import_as_map.entry(key).or_default();
        entry.insert(key);
        // Accessing the alias triggers the import of the original module
        if let Some(loaded_module) = module_pending_imports.get(import) {
            // There is only one element since we create this from an "import as" statement
            if let Some(module_name) = loaded_module.iter().next() {
                entry.insert(*module_name);
            }
        }
    }
}

fn collect_called_function_imports(
    pending_module_name: &ModuleName,
    analysis_map: &AnalysisMap,
) -> AHashSet<ModuleName> {
    let Some(output) = analysis_map.get(pending_module_name) else {
        return AHashSet::new();
    };

    let mut function_called_imports = AHashSet::new();

    let module_called_imports = &output.module_effects.called_imports;

    let mut module_and_function_pending_imports: AHashSet<ModuleName> = output
        .module_effects
        .pending_imports
        .get(pending_module_name)
        .cloned()
        .unwrap_or_default();

    for function in output.module_effects.called_functions.iter() {
        if let Some(imports) = output.module_effects.pending_imports.get(function) {
            module_and_function_pending_imports.extend(imports);
        }
        // check if the function loads any imports
        if let Some(called_imports) = module_called_imports.get(function) {
            let modules_to_update = called_imports.iter().filter(|called_import| {
                module_and_function_pending_imports.contains(*called_import)
            });
            function_called_imports.extend(modules_to_update);
        }
    }

    function_called_imports
}

fn compute_additional_called_imports(analysis_map: &AnalysisMap) -> AdditionalCalledImports {
    analysis_map
        .par_iter()
        .filter_map(|(curr_module, output)| {
            let pending_imports_map = &output.module_effects.pending_imports;
            let called_imports_map = &output.module_effects.called_imports;
            let called_functions = &output.module_effects.called_functions;

            // Only the module-level scope's entry is ever consumed (as the
            // `[module][module]` diagonal), so compute just that scope. Union the
            // imports loaded by every qualifying called function, not just the last.
            let mut additional = AHashSet::new();
            for curr_import in called_imports_map.get(curr_module)? {
                if !pending_imports_map
                    .get(curr_module)
                    .is_some_and(|imports| imports.contains(curr_import))
                    && called_functions.contains(curr_import)
                {
                    additional.extend(collect_imports_in_function_module(
                        curr_import,
                        analysis_map,
                    ));
                }
            }
            (!additional.is_empty()).then_some((*curr_module, additional))
        })
        .collect()
}

fn build_init_module_map(analysis_map: &AnalysisMap) -> HashMap<ModuleName, ModuleName> {
    // Pre-compute __init__ module mappings to avoid repeated string formatting
    analysis_map
        .par_iter()
        .filter_map(|(module, _)| {
            let module_str = module.as_str();
            module_str.ends_with("/__init__").then(|| {
                let base_module_str = module_str.strip_suffix("/__init__").unwrap_or(module_str);
                let base_module = ModuleName::from_str(base_module_str);
                (base_module, *module)
            })
        })
        .collect()
}

/// Submodules pre-loaded into `sys.modules` during Python startup.
/// These are always available at runtime, so accessing them is never an implicit import.
static PYTHON_STARTUP_SUBMODULES: LazyLock<AHashSet<ModuleName>> = LazyLock::new(|| {
    ["encodings.aliases", "encodings.utf_8", "os.path"]
        .iter()
        .map(|s| ModuleName::from_str(s))
        .collect()
});

/// Whether an import statement in `scope` names `module`, directly or through an alias.
fn scope_imports_module(effects: &ModuleEffects, scope: &ModuleName, module: &ModuleName) -> bool {
    [&effects.pending_imports, &effects.aliased_imports]
        .iter()
        .any(|imports| {
            imports
                .get(scope)
                .is_some_and(|names| names.contains(module))
        })
}

fn compute_implicit_imports_for_module(
    curr_module: &ModuleName,
    ctx: &ImplicitImportContext,
) -> Vec<ModuleName> {
    let set_binding = AHashSet::new();
    let map_binding = AHashMap::new();

    let output = ctx.analysis_map.get(curr_module).unwrap();
    let called_imports_map = &output.module_effects.called_imports;
    let called_functions = &output.module_effects.called_functions;

    let all_pending_imports = &output.module_effects.all_pending_import_names;

    // Set of all imports that are called within any imported module.
    let called_in_imported_set: AHashSet<ModuleName> = all_pending_imports
        .iter()
        .filter_map(|pending_module| {
            let pending_module_name = ctx
                .init_module_map
                .get(pending_module)
                .unwrap_or(pending_module);
            ctx.additional_called_imports.get(pending_module_name)
        })
        .flatten()
        .copied()
        .collect();

    // Map from "pending_module.top_level_import" -> set of loaded modules.
    let mut import_as_map: AHashMap<ModuleName, AHashSet<ModuleName>> = AHashMap::new();

    // Union of all called_function_imports across pending modules.
    let mut all_called_fn_imports: AHashSet<ModuleName> = AHashSet::new();

    for pending_module in all_pending_imports {
        let pending_module_name = ctx
            .init_module_map
            .get(pending_module)
            .unwrap_or(pending_module);
        let module_pending_imports = ctx
            .analysis_map
            .get(pending_module_name)
            .map(|output| &output.module_effects.pending_imports)
            .unwrap_or(&map_binding);
        let top_level_imports = module_pending_imports
            .get(pending_module_name)
            .unwrap_or(&set_binding);

        // Build import_as_map entries
        fill_import_as_map(
            pending_module,
            top_level_imports,
            module_pending_imports,
            &mut import_as_map,
        );

        // Accumulate called_function_imports
        all_called_fn_imports.extend(collect_called_function_imports(
            pending_module_name,
            ctx.analysis_map,
        ));
    }

    // collect all the imports we want to mark as non-implicit at the end
    let mut non_implicit_imports = AHashSet::new();
    let mut has_unresolved_imports = false;
    for (scope, imports) in called_imports_map {
        for curr_import in imports {
            // if the import statement exists in the scope of where the import is
            // called or in the module level then it's loaded
            if scope_imports_module(&output.module_effects, curr_module, curr_import)
                || scope_imports_module(&output.module_effects, scope, curr_import)
            {
                non_implicit_imports.insert(*curr_import);
                non_implicit_imports
                    .extend(collect_parent_module_imports(curr_import, ctx.analysis_map));
            } else if called_functions.contains(curr_import) {
                // mark function as loaded
                non_implicit_imports.insert(*curr_import);
                non_implicit_imports.extend(collect_imports_in_function_module(
                    curr_import,
                    ctx.analysis_map,
                ));
            } else {
                has_unresolved_imports = true;

                if called_in_imported_set.contains(curr_import) {
                    non_implicit_imports.insert(*curr_import);
                    continue;
                }

                // Check if any parent of curr_import is a pending module.
                if is_called_attribute_loaded(curr_import, all_pending_imports, ctx.import_graph) {
                    non_implicit_imports.insert(*curr_import);
                }

                // Import-as module lookup.
                if let Some(loaded) = import_as_map.get(curr_import) {
                    non_implicit_imports.extend(loaded.iter());
                }
            }
        }
    }

    // Add called_function_imports if any import reached the unresolved branch.
    if has_unresolved_imports {
        non_implicit_imports.extend(all_called_fn_imports.iter());
    }

    called_imports_map
        .values()
        .flatten()
        .filter(|imp| {
            !non_implicit_imports.contains(imp) && !PYTHON_STARTUP_SUBMODULES.contains(imp)
        })
        .copied()
        .collect()
}

pub(super) fn compute_implicit_imports(analysis_map: &mut AnalysisMap, import_graph: &ImportGraph) {
    let init_module_map = build_init_module_map(analysis_map);

    // we need this so we know when a module is loaded through an imported function call
    // we can't modify analysis_map again so using a global map
    let additional_called_imports = compute_additional_called_imports(analysis_map);

    let ctx = ImplicitImportContext {
        analysis_map,
        additional_called_imports: &additional_called_imports,
        init_module_map: &init_module_map,
        import_graph,
    };

    // Collect implicit imports for each module in parallel
    let implicit_imports_per_module: Vec<(ModuleName, Vec<ModuleName>)> = analysis_map
        .par_iter()
        .map(|(curr_module, _)| {
            let implicit_imports = compute_implicit_imports_for_module(curr_module, &ctx);
            (*curr_module, implicit_imports)
        })
        .collect();

    // Now, sequentially add the collected implicit imports to the output
    for (curr_module, implicit_imports) in implicit_imports_per_module {
        if let Some(output) = analysis_map.get_mut(&curr_module) {
            output.implicit_imports.extend(implicit_imports);
        }
    }
}
