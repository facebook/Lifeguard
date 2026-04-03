/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Perform analysis of an entire project

use std::collections::HashMap;
use std::mem;
use std::sync::LazyLock;

use ahash::AHashMap;
use ahash::AHashSet;
use anyhow::Result;
use anyhow::anyhow;
use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::analyzer;
use crate::analyzer::AnalyzedModule;
use crate::class::Class;
use crate::class::ClassTable;
use crate::class::FieldKind;
use crate::effects::Effect;
use crate::effects::EffectData;
use crate::effects::EffectKind;
use crate::effects::EffectTable;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::exports::Exports;
use crate::imports::ImportGraph;
use crate::module_effects::ModuleImportsMap;
use crate::module_safety::ModuleSafety;
use crate::module_safety::SafetyResult;
use crate::pyrefly::sys_info::SysInfo;
use crate::source_map::AstResult;
use crate::source_map::ModuleProvider;
use crate::stubs::Stubs;
use crate::tracing::time;
use crate::traits::ModuleNameExt;

pub type AnalysisMap = HashMap<ModuleName, AnalyzedModule, ahash::RandomState>;
pub type SafetyMap = DashMap<ModuleName, SafetyResult>;
pub type SideEffectMap = AHashMap<ModuleName, AHashSet<ModuleName>>;
type ScopeImportsMap = AHashMap<ModuleName, AHashMap<ModuleName, AHashSet<ModuleName>>>;

/// Shared immutable context for per-module analysis.
struct AnalysisContext<'a> {
    exports: &'a Exports,
    import_graph: &'a ImportGraph,
    stubs: &'a Stubs,
    sys_info: &'a SysInfo,
}

/// Shared immutable context for computing implicit imports.
struct ImplicitImportContext<'a> {
    analysis_map: &'a AnalysisMap,
    additional_called_imports: &'a ScopeImportsMap,
    init_module_map: &'a HashMap<ModuleName, ModuleName>,
    import_graph: &'a ImportGraph,
}

// Merge effects from all modules into a single effect table
//
// If a function contains nested scopes, add all effects from within the function body to the
// function's scope as well. We do this because in general it is very hard to tell when nested
// scopes escape the function, so we assume that any effect within the body of the function is
// possible to trigger when the function is called.
//
// The major use case for this is decorators defined like
//     def dec:
//       def f:
//         ...  # code with side effects here
//
//       return f
//
// where we want to track the decorator as having side effects even though none are present
// in its direct scope.
fn merge_all_effects(analysis_map: &AnalysisMap) -> EffectTable {
    // Pre-allocate DashMap with estimated capacity (roughly 2 scopes per module)
    let num_modules = analysis_map.len();
    let concurrent_table: DashMap<ModuleName, Vec<Effect>> =
        DashMap::with_capacity(num_modules * 2);

    // Process analysis_map in parallel
    analysis_map.par_iter().for_each(|(_, v)| {
        // Merge module effects
        for (scope, effects) in v.module_effects.effects.iter() {
            let effs = effects.iter().cloned();
            concurrent_table.entry(*scope).or_default().extend(effs);
        }

        // Add nested scope effects to parent functions
        for (scope, effects) in v.module_effects.effects.iter() {
            if let Some(parent) = v.definitions.enclosing_functions.get(scope) {
                let effs = effects.iter().cloned();
                concurrent_table.entry(*parent).or_default().extend(effs);
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
                .functions
                .iter()
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

fn get_all_safe_re_exports(effect_table: &EffectTable, re_exports: &mut AHashSet<ModuleName>) {
    let unsafe_re_exports = effect_table
        .values()
        .flatten()
        .filter(|eff| eff.kind == EffectKind::ImportedVarReassignment);

    for unsafe_re_export in unsafe_re_exports {
        re_exports.remove(&unsafe_re_export.name);
    }
}

// Cached function safety
#[derive(Debug, Clone, Copy, PartialEq)]
enum FunctionSafety {
    // Always safe to call
    Safe,
    // Always unsafe to call
    Unsafe,
    // Unsafe if called from a different module
    UnsafeIfImported,
}

// Collects whole-project analysis output, as well as any global state that is required while
// traversing the analysis map. Uses DashMap for concurrent access from multiple threads.
struct GlobalAnalysisState {
    safety_map: SafetyMap,
    function_safety: DashMap<ModuleName, FunctionSafety>,
}

impl GlobalAnalysisState {
    fn new() -> Self {
        Self {
            safety_map: SafetyMap::new(),
            function_safety: DashMap::new(),
        }
    }

    /// Pre-populate the safety_map with empty ModuleSafety for each module
    fn init_safety_map(&self, analysis_map: &AnalysisMap) {
        for mod_name in analysis_map.keys() {
            self.safety_map
                .insert(*mod_name, SafetyResult::Ok(ModuleSafety::new()));
        }
    }

    fn into_safety_map(self) -> SafetyMap {
        self.safety_map
    }

    fn add_error_to_module(&self, mod_name: &ModuleName, err: SafetyError) {
        let mut entry = self
            .safety_map
            .entry(*mod_name)
            .or_insert_with(|| SafetyResult::Ok(ModuleSafety::new()));
        if let SafetyResult::Ok(module_safety) = entry.value_mut() {
            module_safety.add_error(err);
        }
    }

    fn add_force_imports_eager_override_to_module(&self, mod_name: &ModuleName, err: SafetyError) {
        let mut entry = self
            .safety_map
            .entry(*mod_name)
            .or_insert_with(|| SafetyResult::Ok(ModuleSafety::new()));
        if let SafetyResult::Ok(module_safety) = entry.value_mut() {
            module_safety.add_force_import_override(err);
        }
    }

    fn add_implicit_imports_to_module(
        &self,
        mod_name: &ModuleName,
        implicit_imports: &AHashSet<ModuleName>,
    ) {
        let mut entry = self
            .safety_map
            .entry(*mod_name)
            .or_insert_with(|| SafetyResult::Ok(ModuleSafety::new()));
        if let SafetyResult::Ok(module_safety) = entry.value_mut() {
            module_safety.add_implicit_imports(implicit_imports);
        }
    }

    fn mark_safe(&self, func: &ModuleName) {
        self.function_safety.insert(*func, FunctionSafety::Safe);
    }

    fn mark_unsafe(&self, func: &ModuleName) {
        self.function_safety.insert(*func, FunctionSafety::Unsafe);
    }

    fn mark_unsafe_if_imported(&self, func: &ModuleName) {
        self.function_safety
            .insert(*func, FunctionSafety::UnsafeIfImported);
    }
}

/// Compute side-effect imports: module-level imports never accessed in any scope.
/// These are imports that exist solely for their side effects (e.g., decorator registration).
fn compute_side_effect_imports(analysis_map: &AnalysisMap) -> SideEffectMap {
    let results: Vec<_> = analysis_map
        .par_iter()
        .map(|(module_name, output)| {
            let Some(module_pending) = output.module_effects.pending_imports.get(module_name)
            else {
                return (*module_name, AHashSet::new());
            };

            let side_effects: AHashSet<ModuleName> = module_pending
                .difference(&output.module_effects.all_called_import_names)
                .copied()
                .collect();

            (*module_name, side_effects)
        })
        .collect();

    results.into_iter().collect()
}

/// Run the full analysis pipeline
pub fn run_analysis(
    sources: &impl ModuleProvider,
    exports: &Exports,
    import_graph: &ImportGraph,
    sys_info: &SysInfo,
) -> (SafetyMap, SideEffectMap) {
    let analysis_map = analyze_all(sources, exports, import_graph, sys_info);
    let side_effect_imports = time("  Computing side-effect imports", || {
        compute_side_effect_imports(&analysis_map)
    });
    let info = time("  Building project info", || {
        ProjectInfo::new(analysis_map, exports)
    });
    let safety_map = time("  Collecting errors", || info.collect_errors_from_project());
    time("  Filtering out stubs", || {
        filter_out_stubs(&safety_map, sources)
    });
    (safety_map, side_effect_imports)
}

/// Filter out stubs from the safety map
fn filter_out_stubs(safety_map: &SafetyMap, sources: &impl ModuleProvider) {
    for name in sources.module_names_iter() {
        if sources.is_stub(name) {
            safety_map.remove(name);
        }
    }
}

fn get_parent_module_imports(
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

fn get_imports_in_function_module(
    curr_import: &ModuleName,
    analysis_map: &AnalysisMap,
) -> AHashSet<ModuleName> {
    let mut function_pending_import = AHashSet::new();
    let mut function_called_import = AHashSet::new();
    let mut other_module_level_import = AHashSet::new();

    let mut additional_called_imports = AHashSet::new();

    // For curr_import = "foo.bar.baz.func", check for module in order: foo.bar.baz, foo.bar, foo
    for parent_name in curr_import.iter_parents() {
        if let Some(output) = analysis_map.get(&parent_name) {
            let module_pending_imports = &output.module_effects.pending_imports;
            // Check for imports in parent_name module
            if let Some(s) = module_pending_imports.get(&parent_name) {
                other_module_level_import = s.clone();
            }
            if let Some(s) = module_pending_imports.get(curr_import) {
                function_pending_import = s.clone();
            }
            if let Some(s) = output.module_effects.called_imports.get(curr_import) {
                function_called_import = s.clone();
            }
            break;
        }
    }
    for import in &function_called_import {
        if function_pending_import.contains(import) || other_module_level_import.contains(import) {
            additional_called_imports.insert(*import);
        }
    }

    for import in &function_pending_import {
        additional_called_imports.insert(*import);
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
    for parent_name in curr_import.iter_parents() {
        if all_pending_imports.contains(&parent_name) {
            return true;
        }
    }
    false
}

fn get_import_as_modules(
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

fn get_called_function_imports(
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
        .unwrap_or(&AHashSet::new())
        .clone();

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

fn get_additional_called_imports(analysis_map: &AnalysisMap) -> ScopeImportsMap {
    let set_binding = AHashSet::new();

    // Each module will have its own additional_called_imports, which we merge at the end
    let results: Vec<ScopeImportsMap> = analysis_map
        .par_iter()
        .map(|(curr_module, output)| {
            let mut additional_called_imports = AHashMap::new();
            let pending_imports_map = &output.module_effects.pending_imports;
            let called_imports_map = &output.module_effects.called_imports;
            let called_functions = &output.module_effects.called_functions;
            let module_level_import = pending_imports_map.get(curr_module).unwrap_or(&set_binding);

            for (scope, imports) in called_imports_map {
                for curr_import in imports {
                    if !module_level_import.contains(curr_import)
                        && !pending_imports_map
                            .get(scope)
                            .is_some_and(|imports| imports.contains(curr_import))
                        && called_functions.contains(curr_import)
                    {
                        additional_called_imports.insert(
                            *scope,
                            get_imports_in_function_module(curr_import, analysis_map),
                        );
                    }
                }
            }
            let mut map = AHashMap::new();
            map.insert(*curr_module, additional_called_imports);
            map
        })
        .collect();

    // Merge all per-module additional_called_imports into one
    results.into_iter().flatten().collect()
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

fn compute_implicit_imports_for_module(
    curr_module: &ModuleName,
    ctx: &ImplicitImportContext,
) -> Vec<ModuleName> {
    let set_binding = AHashSet::new();
    let map_binding = AHashMap::new();

    let output = ctx.analysis_map.get(curr_module).unwrap();
    let pending_imports_map = &output.module_effects.pending_imports;
    let called_imports_map = &output.module_effects.called_imports;
    let called_functions = &output.module_effects.called_functions;
    let module_level_import = pending_imports_map.get(curr_module).unwrap_or(&set_binding);

    let all_pending_imports = &output.module_effects.all_pending_import_names;

    // Set of all imports that are called within any imported module.
    let called_in_imported_set: AHashSet<ModuleName> = all_pending_imports
        .iter()
        .filter_map(|pending_module| {
            let pending_module_name = ctx
                .init_module_map
                .get(pending_module)
                .unwrap_or(pending_module);
            ctx.additional_called_imports
                .get(pending_module_name)
                .and_then(|m| m.get(pending_module_name))
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
        get_import_as_modules(
            pending_module,
            top_level_imports,
            module_pending_imports,
            &mut import_as_map,
        );

        // Accumulate called_function_imports
        all_called_fn_imports.extend(get_called_function_imports(
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
            if module_level_import.contains(curr_import)
                || pending_imports_map
                    .get(scope)
                    .is_some_and(|imports| imports.contains(curr_import))
            {
                non_implicit_imports.insert(*curr_import);
                non_implicit_imports
                    .extend(get_parent_module_imports(curr_import, ctx.analysis_map));
            } else if called_functions.contains(curr_import) {
                // mark function as loaded
                non_implicit_imports.insert(*curr_import);
                non_implicit_imports.extend(get_imports_in_function_module(
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

fn get_implicit_imports(analysis_map: &mut AnalysisMap, import_graph: &ImportGraph) {
    let init_module_map = build_init_module_map(analysis_map);

    // we need this so we know when a module is loaded through an imported function call
    // we can't modify analysis_map again so using a global map
    let additional_called_imports = get_additional_called_imports(analysis_map);

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

fn analyze_module(
    mod_name: ModuleName,
    ast_result: &AstResult,
    ctx: &AnalysisContext,
) -> Option<(ModuleName, AnalyzedModule)> {
    let module = ast_result.as_parsed().ok()?;
    let output = analyzer::analyze(
        module,
        ctx.exports,
        ctx.import_graph,
        ctx.stubs,
        ctx.sys_info,
    );
    Some((mod_name, output))
}

/// Analyze all modules and build an analysis map
pub fn analyze_all(
    sources: &impl ModuleProvider,
    exports: &Exports,
    import_graph: &ImportGraph,
    sys_info: &SysInfo,
) -> AnalysisMap {
    let ctx = AnalysisContext {
        exports,
        import_graph,
        stubs: sources.stubs(),
        sys_info,
    };
    let mut analysis_map = time("  Building analysis map", || {
        sources
            .module_names_par_iter()
            .filter_map(|mod_name| {
                let ast_result = sources.parse(mod_name)?;
                analyze_module(*mod_name, &ast_result, &ctx)
            })
            .collect()
    });
    time("  Getting implicit imports", || {
        get_implicit_imports(&mut analysis_map, import_graph)
    });
    analysis_map
}

#[derive(Debug)]
struct Call<'a> {
    caller_module: &'a ModuleName,
    effect: &'a Effect,
    func: ModuleName,
    stack: CallStack,
}

#[derive(Debug, Default)]
struct CallStack {
    entries: Vec<ModuleName>,
    seen: AHashSet<ModuleName>,
}

impl CallStack {
    fn new(initial: ModuleName) -> Self {
        let mut seen = AHashSet::with_capacity(16);
        seen.insert(initial);
        Self {
            entries: vec![initial],
            seen,
        }
    }

    fn contains(&self, name: &ModuleName) -> bool {
        self.seen.contains(name)
    }

    fn push(&mut self, name: ModuleName) {
        self.seen.insert(name);
        self.entries.push(name);
    }

    fn pop(&mut self) {
        if let Some(name) = self.entries.pop() {
            // Make sure we have filtered out recursive stacks
            debug_assert!(!self.entries.contains(&name));
            self.seen.remove(&name);
        }
    }
}

// Immutable global information derived from the project
struct ProjectInfo {
    analysis_map: AnalysisMap,
    effect_table: EffectTable,
    classes: ClassTable,
    // Mappings of functions to the containing module
    functions: AHashMap<ModuleName, ModuleName>,
    re_exports: AHashSet<ModuleName>,
    // Mapping of all methods called on imported objects
    methods: AHashMap<ModuleName, ModuleName>,
}

impl ProjectInfo {
    pub fn new(mut analysis_map: AnalysisMap, exports: &Exports) -> Self {
        let effect_table = time("    Merging all effects", || {
            merge_all_effects(&analysis_map)
        });
        let (functions, methods) = time("    Merging all functions and methods", || {
            merge_all_functions_and_methods(&analysis_map)
        });
        let classes = time("    Merging all classes", || {
            merge_all_classes(&mut analysis_map)
        });
        let re_exports = time("    Getting re-exports", || {
            let mut re_exports = exports
                .get_re_exports()
                .map(|(name, _)| name.as_module_name())
                .collect();
            get_all_safe_re_exports(&effect_table, &mut re_exports);
            re_exports
        });
        Self {
            analysis_map,
            effect_table,
            classes,
            functions,
            re_exports,
            methods,
        }
    }

    pub fn contains_callable(&self, name: &ModuleName) -> bool {
        if self.functions.contains_key(name) || self.classes.contains(name) {
            return true;
        }
        let call_name = self.methods.get(name).copied().unwrap_or(*name);
        if self.functions.contains_key(&call_name) || self.classes.contains(&call_name) {
            true
        } else {
            self.re_exports.contains(&call_name)
        }
    }

    pub fn collect_errors_from_project(&self) -> SafetyMap {
        let state = GlobalAnalysisState::new();
        state.init_safety_map(&self.analysis_map);

        self.analysis_map.par_iter().for_each(|(mod_name, result)| {
            let defs = &result.definitions;
            for scope in &defs.eager_scopes {
                if let Err(e) = self.collect_errors_from_scope(mod_name, scope, &state) {
                    state
                        .safety_map
                        .insert(*mod_name, SafetyResult::AnalysisError(e));
                    return;
                }
            }
            if let Err(e) = self.check_load_imports_eagerly(mod_name, result, &state) {
                state
                    .safety_map
                    .insert(*mod_name, SafetyResult::AnalysisError(e));
                return;
            }
            if let Err(e) = self.collect_implicit_imports(mod_name, result, &state) {
                state
                    .safety_map
                    .insert(*mod_name, SafetyResult::AnalysisError(e));
            }

            // Mark re-exported submodules as failing: when a module re-exports from
            // its own submodule via __all__, the submodule must be marked unsafe so
            // it is excluded from the LAZY_ELIGIBLE dict. Otherwise CPython's lazy import
            // can resolve the name to the submodule object instead of the attribute.
            for effs in result.module_effects.effects.values() {
                for eff in effs {
                    if eff.kind == EffectKind::SubmoduleReExport {
                        state.add_error_to_module(
                            &eff.name,
                            SafetyError::new_from_effect(ErrorKind::SubmoduleReExport, eff),
                        );
                    }
                }
            }
        });

        state.into_safety_map()
    }

    fn check_load_imports_eagerly(
        &self,
        mod_name: &ModuleName,
        result: &AnalyzedModule,
        state: &GlobalAnalysisState,
    ) -> Result<()> {
        // Find effects that trigger adding the module to the load_imports_eagerly set.
        for effs in result.module_effects.effects.values() {
            for e in effs
                .iter()
                .filter(|e| e.kind.requires_eager_loading_imports())
            {
                let err = SafetyError::from_effect(e).ok_or(anyhow!("Unhandled effect {:?}", e))?;
                state.add_force_imports_eager_override_to_module(mod_name, err);
            }
        }
        Ok(())
    }

    fn collect_implicit_imports(
        &self,
        mod_name: &ModuleName,
        result: &AnalyzedModule,
        state: &GlobalAnalysisState,
    ) -> Result<()> {
        state.add_implicit_imports_to_module(mod_name, &result.implicit_imports);

        Ok(())
    }

    fn collect_errors_from_scope(
        &self,
        mod_name: &ModuleName,
        scope: &ModuleName,
        state: &GlobalAnalysisState,
    ) -> Result<()> {
        let Some(effs) = self.effect_table.get(scope) else {
            return Ok(());
        };
        for eff in effs {
            if let Some(err) = SafetyError::from_effect(eff) {
                state.add_error_to_module(mod_name, err);
            } else if eff.kind.is_runnable() {
                let mut call = Call {
                    caller_module: mod_name,
                    effect: eff,
                    func: eff.name,
                    stack: CallStack::new(*scope),
                };
                self.check_call_safety(&mut call, state, true)?;
            } else if eff.kind == EffectKind::ImportedTypeAttr {
                // Check if this is a property access
                if let Some((typ, attr)) = eff.name.split_attr() {
                    if let Some(field) = self
                        .classes
                        .lookup(&typ)
                        .and_then(|cls| cls.get_field(&attr))
                    {
                        if field.kind == FieldKind::Property {
                            let mut call = Call {
                                caller_module: mod_name,
                                effect: eff,
                                func: eff.name,
                                stack: CallStack::new(*scope),
                            };
                            self.check_call_safety(&mut call, state, true)?;
                        }
                    }
                }
            } else if eff.kind == EffectKind::ImportedVarMutation {
                // We only want to capture this effect as an error if it is
                // produced at global scope
                state.add_error_to_module(
                    mod_name,
                    SafetyError::new_from_effect(ErrorKind::ImportedModuleAssignment, eff),
                );
            }
        }
        Ok(())
    }

    fn can_resolve_call(&self, call: &Call) -> bool {
        self.contains_callable(&call.func)
    }

    fn check_unknown_call(&self, call: &Call) -> Result<SafetyError> {
        match call.effect.kind {
            EffectKind::ImportedFunctionCall | EffectKind::FunctionCall => {
                // This is a call with a name, but we don't have a resolved function or class name
                // corresponding to it. Mark it unsafe.
                let err = SafetyError::new_from_effect(ErrorKind::UnknownFunctionCall, call.effect);
                Ok(err)
            }
            EffectKind::MethodCall | EffectKind::ParamMethodCall => {
                let err = SafetyError::new_from_effect(ErrorKind::UnknownMethodCall, call.effect);
                Ok(err)
            }
            EffectKind::ImportedDecoratorCall | EffectKind::DecoratorCall => {
                let err =
                    SafetyError::new_from_effect(ErrorKind::UnknownDecoratorCall, call.effect);
                Ok(err)
            }
            _ => {
                // We should not reach this function with any other type of call
                Err(anyhow!("Unexpected call type {:?}", call))
            }
        }
    }

    fn check_call_safety(
        &self,
        call: &mut Call,
        state: &GlobalAnalysisState,
        publish_safety_error: bool,
    ) -> Result<bool> {
        if !self.can_resolve_call(call) {
            if publish_safety_error {
                let err = self.check_unknown_call(call)?;
                state.add_error_to_module(call.caller_module, err);
            }
            return Ok(false);
        }

        if !self.check_call(call, state)? {
            if publish_safety_error {
                let err = SafetyError::from_unsafe_call(call.effect)?;
                state.add_error_to_module(call.caller_module, err);
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn check_call(&self, call: &mut Call, state: &GlobalAnalysisState) -> Result<bool> {
        if self.classes.contains(&call.func) {
            // This is a class constructor
            self.check_constructor_call(call, state)
        } else {
            self.check_call_body(call, state)
        }
    }

    fn check_constructor_call(&self, call: &mut Call, state: &GlobalAnalysisState) -> Result<bool> {
        let mut ret = true;
        let cls_name = call.func;
        // Check the metaclass
        let cls = self.classes.lookup(&cls_name).unwrap();
        if let Some(mcls) = cls.metaclass {
            call.func = mcls.append_str("__new__");
            ret &= self.check_call_body(call, state)?;
            call.func = mcls.append_str("__init__");
            ret &= self.check_call_body(call, state)?;
        }
        // Check __init__
        // TODO: Look up __init__ in the MRO
        call.func = cls_name.append_str("__init__");
        ret &= self.check_call_body(call, state)?;
        // Check __post_init__ (called by dataclass-generated __init__)
        call.func = cls_name.append_str("__post_init__");
        ret &= self.check_call_body(call, state)?;
        Ok(ret)
    }

    fn check_call_params(&self, call: &Call, effs: &[Effect], state: &GlobalAnalysisState) {
        let EffectData::Call(ref call_data) = call.effect.data else {
            return;
        };
        if !call_data.has_unsafe_args() {
            return;
        }

        let func = &call.func;
        let func_module = self.functions.get(func).copied().unwrap_or(*func);
        let defs = self.analysis_map.get(&func_module).map(|m| &m.definitions);

        for eff in effs {
            if eff.kind != EffectKind::ParamMethodCall {
                continue;
            }
            let param_name = eff.name.as_str();

            // Check positional args: does the mutated param's index match an unsafe arg?
            if let Some(param_idx) = defs.and_then(|d| d.get_param_index(func, param_name)) {
                if call_data.has_unsafe_arg_index(param_idx) {
                    let err =
                        SafetyError::new_from_effect(ErrorKind::ImportedVarArgument, call.effect);
                    state.add_error_to_module(call.caller_module, err);
                    continue;
                }
            }

            // Check keyword args: does the mutated param's name match an unsafe keyword?
            if call_data.has_unsafe_keywords() {
                if call_data.has_unsafe_keyword(param_name) {
                    let err =
                        SafetyError::new_from_effect(ErrorKind::ImportedVarArgument, call.effect);
                    state.add_error_to_module(call.caller_module, err);
                }
                if call_data.has_precise_keyword_tracking() {
                    continue;
                }
            }

            // Fallback: no positional or keyword match resolved, use coarse matching
            if !call_data.has_any_tracked_args() {
                let err = SafetyError::new_from_effect(ErrorKind::ImportedVarArgument, call.effect);
                state.add_error_to_module(call.caller_module, err);
            }
        }
    }

    fn check_call_body(&self, call: &mut Call, state: &GlobalAnalysisState) -> Result<bool> {
        let func = call.func;
        // We need to mark errors in the module containing the called function, not the caller's
        // module.
        let Some(call_module) = self.functions.get(&func) else {
            // This function is not in the function table so we cannot find effects for it.
            return Ok(true);
        };
        let Some(effs) = self.effect_table.get(&func) else {
            // This function has no effects and is therefore safe
            state.mark_safe(&func);
            return Ok(true);
        };

        // Call param mutation is orthogonal to function safety; we always need to check for it
        // even if the function safety is cached.
        self.check_call_params(call, effs, state);

        let is_cross_module_call = *call.caller_module != *call_module;

        if let Some(safe) = state.function_safety.get(&func).map(|r| *r) {
            let ret = match safe {
                FunctionSafety::Safe => true,
                FunctionSafety::Unsafe => false,
                FunctionSafety::UnsafeIfImported => !is_cross_module_call,
            };
            return Ok(ret);
        }

        let mut ret = true;
        for eff in effs {
            if SafetyError::from_effect(eff).is_some() {
                // We have an effect that translates unconditionally to an error, so mark the
                // function unsafe
                state.mark_unsafe(&func);
                ret = false;
            } else if eff.kind.is_runnable() {
                if call.stack.contains(&eff.name) {
                    // We have a recursive function call; mark it unsafe
                    state.mark_unsafe(&func);
                    ret = false;
                } else {
                    call.stack.push(eff.name);
                    let mut child_call = Call {
                        caller_module: call.caller_module,
                        effect: eff,
                        func: eff.name,
                        stack: std::mem::take(&mut call.stack),
                    };
                    if !self.check_call_safety(&mut child_call, state, false)? {
                        // This function has called an unsafe function; mark it unsafe.
                        state.mark_unsafe(&func);
                        // Do not return at the first error because we might miss some transitive
                        // calls.
                        ret = false;
                    }
                    call.stack = child_call.stack;
                    call.stack.pop();
                }
            } else {
                match eff.kind {
                    EffectKind::ImportedVarMutation => {
                        // We mark this call as unsafe but do not add an error
                        // as this is not call is not happening at global scope.
                        // If this callable is called, we will add the error there.
                        state.mark_unsafe(&func);
                        ret = false;
                    }
                    EffectKind::GlobalVarAssign | EffectKind::GlobalVarMutation => {
                        // This function is attempting to mutate a global variable, and so is only safe
                        // if being called from its own module.
                        // NOTE: unsafe-if-imported should not overwrite unsafe, so we check the
                        // cached state first.
                        let is_already_unsafe = state
                            .function_safety
                            .get(&func)
                            .is_some_and(|v| *v == FunctionSafety::Unsafe);
                        if !is_already_unsafe {
                            state.mark_unsafe_if_imported(&func);
                            if is_cross_module_call {
                                ret = false;
                            }
                        }
                    }
                    _ => (),
                }
            }
        }

        // We haven't detected any unsafe behaviour
        if state.function_safety.get(&func).is_none() {
            state.mark_safe(&func);
        }
        Ok(ret)
    }
}
