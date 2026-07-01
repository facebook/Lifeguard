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
use std::sync::OnceLock;
use std::sync::mpsc;
use std::sync::mpsc::Sender;

use anyhow::Result;
use anyhow::anyhow;
use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_text_size::TextRange;
use tracing::warn;

use crate::analyzer;
use crate::analyzer::AnalyzedModule;
use crate::class::Class;
use crate::class::ClassTable;
use crate::class::FieldKind;
use crate::config::AnalysisConfig;
use crate::csr_graph::CsrGraph;
use crate::effects::ArgSlot;
use crate::effects::CallData;
use crate::effects::Effect;
use crate::effects::EffectData;
use crate::effects::EffectKind;
use crate::effects::EffectTable;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::exports::Exports;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::module_effects::ModuleImportsMap;
use crate::module_info::DefinitionTable;
use crate::module_parser::ParsedModule;
pub use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::ModuleSafety;
use crate::module_safety::SafetyResult;
use crate::source_map::AstResult;
use crate::source_map::ModuleProvider;
use crate::stubs::Stubs;
use crate::tracing::time;
use crate::traits::ModuleNameExt;

pub type AnalysisMap = HashMap<ModuleName, AnalyzedModule, ahash::RandomState>;
pub type SafetyMap = DashMap<ModuleName, SafetyResult>;
pub type SideEffectMap = AHashMap<ModuleName, AHashSet<ModuleName>>;
pub type ParseErrors = DashMap<ModuleName, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CachingMode {
    Enabled,
    Disabled,
}
type ScopeImportsMap = AHashMap<ModuleName, AHashMap<ModuleName, AHashSet<ModuleName>>>;

/// Shared immutable context for per-module analysis.
struct AnalysisContext<'a> {
    exports: &'a Exports,
    import_graph: &'a ImportGraph,
    stubs: &'a Stubs,
    config: &'a AnalysisConfig,
}

/// Shared immutable context for computing implicit imports.
struct ImplicitImportContext<'a> {
    analysis_map: &'a AnalysisMap,
    additional_called_imports: &'a ScopeImportsMap,
    init_module_map: &'a HashMap<ModuleName, ModuleName>,
    import_graph: &'a ImportGraph,
}

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

        // Bubble eager scope (class body) effects to their enclosing function.
        // Nested function effects are NOT bubbled — the call graph handles them.
        for (scope, effects) in v.module_effects.effects.iter() {
            if let Some(parent) = v.definitions.enclosing_functions.get(scope) {
                if !v.definitions.functions.contains(scope) {
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
                    if v.definitions.functions.contains(child)
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

fn collect_re_exports(exports: &Exports, effect_table: &EffectTable) -> AHashSet<ModuleName> {
    // Re-export names are interned via as_module_name(), which is the expensive
    // part on large projects; do it in parallel, then build the set.
    let names: Vec<ModuleName> = exports
        .par_re_exports()
        .map(|(name, _)| name.as_module_name())
        .collect();
    let mut re_exports: AHashSet<ModuleName> = names.into_iter().collect();
    get_all_safe_re_exports(effect_table, &mut re_exports);
    re_exports
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

/// Collected output from the analysis pipeline.
pub struct AnalysisOutput {
    pub safety_map: SafetyMap,
    pub side_effect_imports: SideEffectMap,
    pub parse_errors: ParseErrors,
}

// Collects whole-project analysis output, as well as any global state that is required while
// traversing the analysis map. Uses DashMap for concurrent access from multiple threads.
struct GlobalAnalysisState {
    safety_map: SafetyMap,
    function_safety: DashMap<ModuleName, FunctionSafetyInfo>,
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

    /// Decompose FQN-keyed function safety verdicts into per-module local-name
    /// maps and embed them in the corresponding ModuleSafety entries.
    fn into_safety_map(self, caching: CachingMode) -> SafetyMap {
        if caching == CachingMode::Enabled {
            self.function_safety.par_iter().for_each(|entry| {
                let fqn = entry.key();
                for (parent, dot_pos) in fqn.iter_parents() {
                    if let Some(mut safety_entry) = self.safety_map.get_mut(&parent) {
                        if let SafetyResult::Ok(module_safety) = safety_entry.value_mut() {
                            let local_name = &fqn.as_str()[dot_pos + 1..];
                            module_safety
                                .function_safety
                                .insert(local_name.to_string(), entry.value().clone());
                        }
                        break;
                    }
                }
            });
        }
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
        self.function_safety
            .insert(*func, FunctionSafetyInfo::new(FunctionSafety::Safe));
    }

    fn mark_unsafe(&self, func: &ModuleName) {
        self.function_safety
            .insert(*func, FunctionSafetyInfo::new(FunctionSafety::Unsafe));
    }

    fn mark_unsafe_missing_dep(&self, func: &ModuleName, callee: &ModuleName) {
        self.function_safety
            .entry(*func)
            .and_modify(|info| {
                info.verdict = info.verdict.max(FunctionSafety::UnsafeMissingDep);
                info.missing_dep_callees.insert(*callee);
            })
            .or_insert_with(|| FunctionSafetyInfo::unsafe_missing_dep(*callee));
    }

    fn is_unsafe(&self, func: &ModuleName) -> bool {
        self.function_safety
            .get(func)
            .is_some_and(|info| info.verdict == FunctionSafety::Unsafe)
    }

    fn mark_unsafe_if_imported(&self, func: &ModuleName) {
        self.function_safety
            .entry(*func)
            .and_modify(|info| {
                info.verdict = info.verdict.max(FunctionSafety::UnsafeIfImported);
            })
            .or_insert_with(|| FunctionSafetyInfo::new(FunctionSafety::UnsafeIfImported));
    }
}

/// Compute side-effect imports: module-level imports never accessed in any scope.
/// These are imports that exist solely for their side effects (e.g., decorator registration).
/// Lazy imports that are never accessed have no side effects, so they are excluded.
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
                .filter(|m| output.module_effects.eager_imports.contains(m))
                .copied()
                .collect();

            (*module_name, side_effects)
        })
        .collect();

    results.into_iter().collect()
}

/// Run the full analysis pipeline.
pub fn run_analysis(
    sources: &impl ModuleProvider,
    exports: &Exports,
    import_graph: &ImportGraph,
    config: &AnalysisConfig,
    caching: CachingMode,
) -> AnalysisOutput {
    let (analysis_map, parse_errors) = analyze_all(sources, exports, import_graph, config);
    let side_effect_imports = time("  Computing side-effect imports", || {
        compute_side_effect_imports(&analysis_map)
    });
    let info = time("  Building project info", || {
        ProjectInfo::new(analysis_map, exports)
    });
    let safety_map = time("  Collecting errors", || {
        info.collect_errors_from_project(caching)
    });
    time("  Filtering out stubs", || {
        filter_out_stubs(&safety_map, sources)
    });

    // Deallocating ProjectInfo takes seconds on large projects. Hand it to a
    // dedicated background thread so the dealloc is non-blocking
    drop_in_background(info);

    AnalysisOutput {
        safety_map,
        side_effect_imports,
        parse_errors,
    }
}

/// Dedicated thread for deallocating project info
fn drop_in_background<T: Send + 'static>(value: T) {
    static DROPPER: OnceLock<Sender<Box<dyn FnOnce() + Send>>> = OnceLock::new();
    let sender = DROPPER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name("lifeguard-dealloc".to_owned())
            .spawn(move || rx.into_iter().for_each(|dropper| dropper()))
            .expect("failed to spawn background dropper thread");
        tx
    });
    // If the designated thread exits unexpectedly, drop inline rather than leak.
    if let Err(err) = sender.send(Box::new(move || drop(value))) {
        warn!("background deallocation thread unavailable; dropping inline");
        let dropper = err.0;
        dropper();
    }
}

/// Filter out stubs without sources from the safety map
fn filter_out_stubs(safety_map: &SafetyMap, sources: &impl ModuleProvider) {
    for name in sources.module_names_iter() {
        if sources.is_stub(name) && !sources.overrides_source(name) {
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
    for (parent_name, _) in curr_import.iter_parents() {
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
    for (parent_name, _) in curr_import.iter_parents() {
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
    module: &ParsedModule,
    ctx: &AnalysisContext,
) -> (ModuleName, AnalyzedModule) {
    let output = analyzer::analyze(module, ctx.exports, ctx.import_graph, ctx.stubs, ctx.config);
    (mod_name, output)
}

/// Analyze all modules and build an analysis map.
/// Parse errors are collected and returned separately.
pub fn analyze_all(
    sources: &impl ModuleProvider,
    exports: &Exports,
    import_graph: &ImportGraph,
    config: &AnalysisConfig,
) -> (AnalysisMap, ParseErrors) {
    let ctx = AnalysisContext {
        exports,
        import_graph,
        stubs: sources.stubs(),
        config,
    };

    let parse_errors = ParseErrors::new();

    let mut analysis_map: AnalysisMap = time("  Building analysis map", || {
        sources
            .module_names_par_iter()
            .filter_map(|mod_name| {
                let ast_result = sources.parse(mod_name)?;
                match ast_result {
                    AstResult::ParserError(e) => {
                        parse_errors.insert(*mod_name, e.to_string());
                        None
                    }
                    AstResult::Ok(ref module) => Some(analyze_module(*mod_name, module, &ctx)),
                }
            })
            .collect()
    });

    time("  Getting implicit imports", || {
        get_implicit_imports(&mut analysis_map, import_graph)
    });
    (analysis_map, parse_errors)
}

#[derive(Debug)]
struct Call<'a> {
    caller_module: &'a ModuleName,
    effect: &'a Effect,
    func: ModuleName,
    stack: CallStack,
    // Distinct from check_call_safety's publish_safety_error: that flag is true only at the
    // top-level call (false for recursive calls to avoid double-counting), while is_module_scope
    // stays true through the entire chain so check_call_params catches transitive param mutation.
    is_module_scope: bool,
}

impl<'a> Call<'a> {
    fn clone_with_name(&self, func: ModuleName) -> Self {
        Self {
            caller_module: self.caller_module,
            effect: self.effect,
            func,
            stack: self.stack.clone(),
            is_module_scope: self.is_module_scope,
        }
    }
}

#[derive(Debug, Default, Clone)]
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

/// The functions a constructor call to `cls_name` may dispatch to, in the
/// order `check_constructor_call` checks them: the metaclass `__new__` and
/// `__init__` (if the class has a metaclass), then `__init__` and
/// `__post_init__` on the class itself (the latter is called by the
/// dataclass-generated `__init__`).
/// TODO: Look up `__init__` in the MRO.
fn constructor_method_names(
    cls_name: ModuleName,
    classes: &ClassTable,
) -> impl Iterator<Item = ModuleName> {
    let metaclass = classes.lookup(&cls_name).and_then(|cls| cls.metaclass);
    metaclass
        .into_iter()
        .flat_map(|mcls| [mcls.append_str("__new__"), mcls.append_str("__init__")])
        .chain([
            cls_name.append_str("__init__"),
            cls_name.append_str("__post_init__"),
        ])
}

/// Number of implicit leading parameters (the receiver) for a method call.
/// Returns `None` when the method's kind is unknown (e.g. class could not be resolved).
fn method_receiver_offset(method: &ModuleName, classes: &ClassTable, bound: bool) -> Option<usize> {
    let (class, leaf) = method.split_attr()?;
    Some(match classes.lookup(&class)?.get_field(&leaf)?.kind {
        FieldKind::StaticMethod => 0,
        FieldKind::ClassMethod => 1,
        FieldKind::InstanceMethod => usize::from(bound),
        _ => return None,
    })
}

/// Yield each `(callee, arg_offset)` a call binds to: the function(s) whose
/// parameters the call's explicit arguments map to, paired with the number of
/// implicit leading parameters (the receiver) to skip.
fn iter_callees<'a>(
    eff: &'a Effect,
    classes: &'a ClassTable,
) -> impl Iterator<Item = (ModuleName, usize)> + 'a {
    let is_method = matches!(
        eff.kind,
        EffectKind::MethodCall | EffectKind::UnboundMethodCall
    );
    let is_constructor = !is_method && classes.contains(&eff.name);

    // Constructor calls dispatch to each constructor method (offset 1); empty
    // for non-constructor calls.
    let constructors = is_constructor
        .then(|| constructor_method_names(eff.name, classes))
        .into_iter()
        .flatten()
        .map(|method| (method, 1));

    // Method / plain function calls resolve to a single callee (`eff.name`) at
    // one or two offsets; empty for constructor calls.
    let (offset, extra_offset) = match eff.kind {
        _ if is_constructor => (None, None),
        EffectKind::MethodCall => match method_receiver_offset(&eff.name, classes, true) {
            Some(offset) => (Some(offset), None),
            // Unknown kind (e.g. a builtin / third-party class): assume an
            // implicit receiver, as bound calls usually have one.
            None => (Some(1), None),
        },
        EffectKind::UnboundMethodCall => match method_receiver_offset(&eff.name, classes, false) {
            Some(offset) => (Some(offset), None),
            // Unknown kind: the receiver may be explicit (0) or implicit (1), so
            // consider both to avoid missing a mutated parameter.
            None => (Some(0), Some(1)),
        },
        _ => (Some(0), None),
    };
    let name = eff.name;
    let single = offset
        .into_iter()
        .chain(extra_offset)
        .map(move |offset| (name, offset));

    constructors.chain(single)
}

/// Per function, the set of parameters that are method-mutated either directly or transitively via
/// parameter forwarding.
///
/// TODO(T237092592):
/// - Cross-library forwarding is not tracked
/// - Unknown callee (no EffectTable entry) similarly drops the edge, inconsistent with other places
///   where unknown => unsafe
/// - `**kwargs` forwarding of a parameter is not tracked precisely; only named
///   keyword arguments are added to edges.
fn compute_mutated_params(
    effect_table: &EffectTable,
    functions: &AHashMap<ModuleName, ModuleName>,
    classes: &ClassTable,
    analysis_map: &AnalysisMap,
) -> AHashMap<ModuleName, AHashSet<ModuleName>> {
    // Compute a fixpoint over the call graph. The "seed" is the base set of functions that directly
    // mutate their parameters, and an "edge" is a transitive mutation from callee->caller (reversed
    // from the call graph since mutations flow in the opposite direction to calls).

    // (function, mutated parameter)
    type SeedMutation = (ModuleName, ModuleName);
    // ((callee, target parameter), (caller, caller parameter))
    type Edge = ((ModuleName, ModuleName), SeedMutation);

    // Scan every scope's effects in parallel
    let (all_seeds, all_edges): (Vec<SeedMutation>, Vec<Edge>) = effect_table
        .par_iter()
        .fold(
            || (Vec::new(), Vec::new()),
            |(mut seeds, mut edges), (scope, effs)| {
                for eff in effs {
                    // Seed: parameters mutated directly in this function.
                    if eff.kind == EffectKind::ParamMethodCall {
                        seeds.push((*scope, eff.name));
                        continue;
                    }
                    // Parameter-forwarding edges recorded on call effects.
                    let EffectData::Call(ref call_data) = eff.data else {
                        continue;
                    };
                    let forwarded = call_data.forwarded_params();
                    if forwarded.is_empty() {
                        continue;
                    }
                    // A call may bind to several callees (a constructor's
                    // __init__/__new__); resolve each owner's parameter names once.
                    for (owner, arg_offset) in iter_callees(eff, classes) {
                        let owner_param_names = functions
                            .get(&owner)
                            .and_then(|module| analysis_map.get(module))
                            .and_then(|m| m.definitions.param_names.get(&owner));
                        for (slot, param) in forwarded {
                            match slot {
                                ArgSlot::Keyword(name) => {
                                    edges.push(((owner, *name), (*scope, *param)));
                                }
                                ArgSlot::Positional(idx) => {
                                    if let Some(target) = owner_param_names
                                        .and_then(|names| names.get(idx + arg_offset))
                                        .map(ModuleName::from_name)
                                    {
                                        edges.push(((owner, target), (*scope, *param)));
                                    }
                                }
                                ArgSlot::StarExpansion(start) => {
                                    // `*param` unpacks into the callee's positional
                                    // parameters from the star's position onward, so
                                    // forward to each parameter at or after that slot
                                    // (offset by the receiver). Leading positional args
                                    // before the star fill earlier params, which `*param`
                                    // cannot reach.
                                    if let Some(names) = owner_param_names {
                                        edges.extend(names.iter().skip(start + arg_offset).map(
                                            |name| {
                                                (
                                                    (owner, ModuleName::from_name(name)),
                                                    (*scope, *param),
                                                )
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                (seeds, edges)
            },
        )
        .reduce(
            || (Vec::new(), Vec::new()),
            |(mut seeds, mut edges), (s, e)| {
                seeds.extend(s);
                edges.extend(e);
                (seeds, edges)
            },
        );

    // Merge into `mutated` (seeds the fixpoint) and `dependents`, the reverse
    // index: (owner `g`, parameter `q`) -> callers whose parameter becomes mutated
    // once `q` is known mutated.
    let mut mutated: AHashMap<ModuleName, AHashSet<ModuleName>> =
        AHashMap::with_capacity(all_seeds.len());
    let mut dependents: AHashMap<(ModuleName, ModuleName), Vec<SeedMutation>> =
        AHashMap::with_capacity(all_edges.len());
    let mut worklist: Vec<SeedMutation> = Vec::new();
    for (scope, param) in all_seeds {
        if mutated.entry(scope).or_default().insert(param) {
            worklist.push((scope, param));
        }
    }
    for (key, dep) in all_edges {
        dependents.entry(key).or_default().push(dep);
    }

    // Fixpoint: when (g, q) becomes mutated, each caller forwarding a parameter
    // into q has that parameter become mutated too.
    while let Some((g, q)) = worklist.pop() {
        let Some(deps) = dependents.get(&(g, q)) else {
            continue;
        };
        for &(f, p) in deps {
            if mutated.entry(f).or_default().insert(p) {
                worklist.push((f, p));
            }
        }
    }

    mutated
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
    // Reverse mapping: parent function → nested function scopes.
    // Used by check_call_body to check nested function effects for decorator calls.
    nested_functions: AHashMap<ModuleName, Vec<ModuleName>>,
    // Per-function set of parameter names that are method-mutated, directly or by
    // forwarding to another function's mutated parameter (transitive closure).
    // Used to detect passing imported state to a mutated parameter, even through
    // intermediate forwarding functions.
    mutated_params: AHashMap<ModuleName, AHashSet<ModuleName>>,
}

impl ProjectInfo {
    pub fn new(mut analysis_map: AnalysisMap, exports: &Exports) -> Self {
        let (effect_table, (functions, methods)) = time("    Merging effects + functions", || {
            rayon::join(
                || merge_all_effects(&analysis_map),
                || merge_all_functions_and_methods(&analysis_map),
            )
        });
        let classes = time("    Merging all classes", || {
            merge_all_classes(&mut analysis_map)
        });
        let ((re_exports, nested_functions), mutated_params) = rayon::join(
            || {
                time("    Getting re-exports + nested fns", || {
                    rayon::join(
                        || collect_re_exports(exports, &effect_table),
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
            functions,
            re_exports,
            methods,
            nested_functions,
            mutated_params,
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

    pub fn collect_errors_from_project(&self, caching: CachingMode) -> SafetyMap {
        let state = GlobalAnalysisState::new();
        state.init_safety_map(&self.analysis_map);

        // Determinism fix: compute all function/constructor safety verdicts up
        // front, BEFORE the module-scope error pass, so that pass only ever reads
        // a complete, order-independent verdict cache.
        if caching == CachingMode::Enabled {
            time("    Marking recursive functions", || {
                self.mark_recursive_functions_unsafe(&state)
            });
            time("    Precompute constructor safety", || {
                self.precompute_constructor_safety(&state)
            });
            time("    Precompute function safety", || {
                self.precompute_function_safety(&state)
            });
        }

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
        });

        state.into_safety_map(caching)
    }

    /// Deterministically mark every function/class that participates in a call cycle as `Unsafe`,
    /// before the memoized call-graph traversal runs.
    ///
    /// Marking the whole cycle up front is the order-free equivalent of "recursive calls are
    /// unsafe", and leaves the remaining call graph acyclic so memoized verdicts become independent
    /// of visitation order.
    fn mark_recursive_functions_unsafe(&self, state: &GlobalAnalysisState) {
        let class_names: Vec<ModuleName> = self.classes.par_keys().copied().collect();
        let n_nodes = self.functions.len() + class_names.len();
        let mut indexes: AHashMap<ModuleName, u32> = AHashMap::with_capacity(n_nodes);
        let mut names: Vec<ModuleName> = Vec::with_capacity(n_nodes);
        for name in self.functions.keys().chain(class_names.iter()) {
            indexes.entry(*name).or_insert_with(|| {
                names.push(*name);
                (names.len() - 1) as u32
            });
        }

        // Collect call-graph edges in parallel. Edge order does not affect the SCC
        // result, so the nondeterministic cross-thread merge order is fine.
        let indexes = &indexes;
        // Runnable-call edges out of each function scope (function/method/
        // decorator/constructor calls).
        let mut edges: Vec<(u32, u32)> = self
            .functions
            .par_iter()
            .flat_map_iter(|(func, _)| {
                let from = indexes[func];
                self.effect_table
                    .get(func)
                    .into_iter()
                    .flatten()
                    .filter(|e| e.kind.is_runnable())
                    .filter_map(move |e| indexes.get(&e.name).map(|&to| (from, to)))
            })
            .collect();
        // Constructor dispatch edges, mirroring check_constructor_call.
        let ctor_edges: Vec<(u32, u32)> = class_names
            .par_iter()
            .flat_map_iter(|cls_name| {
                let from = indexes[cls_name];
                self.constructor_methods(*cls_name)
                    .filter_map(move |m| indexes.get(&m).map(|&to| (from, to)))
            })
            .collect();
        edges.extend(ctor_edges);

        let in_cycle = CsrGraph::from_edges(n_nodes, &edges).nodes_in_cycles();

        for (i, is_cyclic) in in_cycle.iter().enumerate() {
            if *is_cyclic {
                state.mark_unsafe(&names[i]);
            }
        }
    }

    fn precompute_constructor_safety(&self, state: &GlobalAnalysisState) {
        let dummy_range = TextRange::default();
        self.classes.par_keys().for_each(|cls_name| {
            if state.function_safety.contains_key(cls_name) {
                return;
            }
            let effect = Effect::new(EffectKind::FunctionCall, *cls_name, dummy_range);
            // cls_name as caller_module makes UnsafeIfImported → Unsafe (conservative for cache).
            let call = Call {
                caller_module: cls_name,
                effect: &effect,
                func: *cls_name,
                stack: CallStack::default(),
                is_module_scope: false,
            };
            match self.check_constructor_call(&call, state) {
                Ok(true) => state.mark_safe(cls_name),
                Ok(false) | Err(_) => state.mark_unsafe(cls_name),
            }
        });
    }

    fn precompute_function_safety(&self, state: &GlobalAnalysisState) {
        let dummy_range = TextRange::default();
        self.functions
            .par_iter()
            .for_each(|(func_name, func_module)| {
                if state.function_safety.contains_key(func_name) {
                    return;
                }
                let effect = Effect::new(EffectKind::FunctionCall, *func_name, dummy_range);
                let mut call = Call {
                    caller_module: func_module,
                    effect: &effect,
                    func: *func_name,
                    stack: CallStack::default(),
                    is_module_scope: false,
                };
                if let Err(e) = self.check_call_body(&mut call, state) {
                    tracing::warn!("precompute_function_safety: {}: {}", func_name.as_str(), e);
                    state.mark_unsafe(func_name);
                }
                if !state.function_safety.contains_key(func_name) {
                    state.mark_safe(func_name);
                }
            });
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
                    is_module_scope: true,
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
                                is_module_scope: true,
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

    fn can_resolve_call(&self, call: &Call, state: &GlobalAnalysisState) -> bool {
        self.contains_callable(&call.func) || state.function_safety.contains_key(&call.func)
    }

    /// Mark `func` unsafe because one of its callees failed its safety check.
    /// A resolvable callee is intrinsically unsafe, so `func` is hard `Unsafe`.
    /// An unresolvable callee may just be a missing cross-library dep, so `func`
    /// gets the recoverable `UnsafeMissingDep` (which cross-library resolution
    /// can later upgrade) — but never downgrade an already hard-`Unsafe` verdict.
    fn mark_caller_unsafe_for_failed_callee(
        &self,
        func: &ModuleName,
        callee: &Call,
        state: &GlobalAnalysisState,
    ) {
        if self.can_resolve_call(callee, state) {
            state.mark_unsafe(func);
        } else if !state.is_unsafe(func) {
            state.mark_unsafe_missing_dep(func, &callee.func);
        }
    }

    fn check_unknown_call(&self, call: &Call) -> Result<SafetyError> {
        match call.effect.kind {
            EffectKind::ImportedFunctionCall | EffectKind::FunctionCall => {
                // This is a call with a name, but we don't have a resolved function or class name
                // corresponding to it. Mark it unsafe.
                let err = SafetyError::new_from_effect(ErrorKind::UnknownFunctionCall, call.effect);
                Ok(err)
            }
            EffectKind::MethodCall
            | EffectKind::UnboundMethodCall
            | EffectKind::ParamMethodCall => {
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
        if !self.can_resolve_call(call, state) {
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
        let mut ret = if self.classes.contains(&call.func) {
            // This is a class constructor
            self.check_constructor_call(call, state)?
        } else {
            self.check_call_body(call, state)?
        };
        // A parameterized decorator @deco(args) also calls the function deco returns,
        // so its nested functions' effects run at import time too. This is checked at
        // the call site, not folded into check_call_body's cached verdict: that verdict
        // must stay independent of the call kind that first reached the function.
        if self.is_parameterized_decorator_call(call) {
            ret &= self.check_decorator_nested_functions(call, state)?;
        }
        Ok(ret)
    }

    /// Check the nested functions of a parameterized decorator call without
    /// mutating the cached safety verdict of the decorator function itself.
    fn check_decorator_nested_functions(
        &self,
        call: &mut Call,
        state: &GlobalAnalysisState,
    ) -> Result<bool> {
        let Some(children) = self.nested_functions.get(&call.func) else {
            return Ok(true);
        };
        let mut ret = true;
        // Only the decorator factory's immediate children run at decoration time.
        for child in children {
            // Use a FunctionCall effect for the child so we don't re-enter the
            // parameterized decorator path (which would cause infinite recursion).
            let child_effect = Effect::new(EffectKind::FunctionCall, *child, call.effect.range);
            let mut child_call = Call {
                caller_module: call.caller_module,
                effect: &child_effect,
                func: *child,
                stack: std::mem::take(&mut call.stack),
                is_module_scope: call.is_module_scope,
            };
            let child_ret = self.check_call_body(&mut child_call, state)?;
            call.stack = child_call.stack;
            ret &= child_ret;
        }
        Ok(ret)
    }

    fn constructor_methods(&self, cls_name: ModuleName) -> impl Iterator<Item = ModuleName> {
        constructor_method_names(cls_name, &self.classes)
    }

    fn check_constructor_call(&self, call: &Call, state: &GlobalAnalysisState) -> Result<bool> {
        let mut ret = true;
        for method in self.constructor_methods(call.func) {
            let mut method_call = call.clone_with_name(method);
            ret &= self.check_call_body(&mut method_call, state)?;
        }
        Ok(ret)
    }

    /// Whether `call_data` passes an imported variable to the callee's parameter
    /// `param_name`: either the imported arg is at that parameter's position or
    /// keyword, or arg tracking was imprecise so we conservatively assume it is.
    ///
    /// `arg_offset` is the number of implicit leading parameters (the receiver)
    /// to skip, so that explicit positional argument `i` matches parameter
    /// `i + arg_offset` (e.g. 1 for a bound method or constructor's `self`).
    fn mutated_param_receives_imported_arg(
        call_data: &CallData,
        callee: &ModuleName,
        param_name: &str,
        defs: Option<&DefinitionTable>,
        arg_offset: usize,
    ) -> bool {
        // Positional args: does the mutated param's index (minus the receiver
        // offset) match an unsafe arg? With the *args lower bound recorded in
        // has_unsafe_arg_index, a resolved index yields an exact answer.
        let mut positional_unresolved = true;
        if let Some(param_idx) = defs.and_then(|d| d.get_param_index(callee, param_name)) {
            if let Some(arg_idx) = param_idx.checked_sub(arg_offset) {
                positional_unresolved = false;
                if call_data.has_unsafe_arg_index(arg_idx) {
                    return true;
                }
            }
        }

        // Keyword args: does the mutated param's name match an unsafe keyword?
        if call_data.has_unsafe_keywords() {
            if call_data.has_unsafe_keyword(param_name) {
                return true;
            }
            // Only rule this parameter out when BOTH keyword and positional
            // tracking are precise; an imprecise *args expansion could still reach
            // a positional parameter we couldn't pinpoint.
            if call_data.has_precise_keyword_tracking() && call_data.has_precise_arg_tracking() {
                return false;
            }
        }

        // No positional/keyword match could be pinpointed. Match conservatively
        // only when the positional index couldn't be resolved and positional
        // tracking was imprecise (an unsafe *args expansion at an unknown slot,
        // mirroring the **kwargs handling above), or there is an unsafe arg we
        // couldn't track at all. A resolved index is already answered exactly by
        // has_unsafe_arg_index above, so it must not be re-flagged here.
        (positional_unresolved && !call_data.has_precise_arg_tracking())
            || !call_data.has_any_tracked_args()
    }

    /// Whether `call_data` passes an imported variable into a (transitively)
    /// mutated parameter of `callee`, with explicit args starting at `arg_offset`.
    /// (Helper function for `call_mutates_imported_arg()` below.)
    fn callee_mutates_imported_arg(
        &self,
        call_data: &CallData,
        callee: &ModuleName,
        arg_offset: usize,
    ) -> bool {
        let Some(mutated) = self.mutated_params.get(callee) else {
            return false;
        };
        let callee_module = self.functions.get(callee).copied().unwrap_or(*callee);
        let defs = self
            .analysis_map
            .get(&callee_module)
            .map(|m| &m.definitions);
        mutated.iter().any(|param| {
            Self::mutated_param_receives_imported_arg(
                call_data,
                callee,
                param.as_str(),
                defs,
                arg_offset,
            )
        })
    }

    /// Whether `call_effect` passes an imported variable to a parameter the callee
    /// (transitively) mutates, i.e. running this call mutates imported state.
    fn call_mutates_imported_arg(&self, call_effect: &Effect) -> bool {
        let EffectData::Call(ref call_data) = call_effect.data else {
            return false;
        };
        if !call_data.has_unsafe_args() {
            return false;
        }
        iter_callees(call_effect, &self.classes)
            .any(|(callee, offset)| self.callee_mutates_imported_arg(call_data, &callee, offset))
    }

    fn check_call_params(&self, call: &Call, state: &GlobalAnalysisState) {
        if self.call_mutates_imported_arg(call.effect) {
            let err = SafetyError::new_from_effect(ErrorKind::ImportedVarArgument, call.effect);
            state.add_error_to_module(call.caller_module, err);
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
        let is_cross_module_call = *call.caller_module != *call_module;

        let effs = self.effect_table.get(&func);

        if call.is_module_scope {
            self.check_call_params(call, state);
        }

        if let Some(verdict) = state.function_safety.get(&func).map(|info| info.verdict) {
            let ret = match verdict {
                FunctionSafety::Safe => true,
                FunctionSafety::Unsafe | FunctionSafety::UnsafeMissingDep => false,
                FunctionSafety::UnsafeIfImported => !is_cross_module_call,
            };
            return Ok(ret);
        }

        let mut ret = true;

        if let Some(effs) = effs {
            for eff in effs {
                if SafetyError::from_effect(eff).is_some() {
                    // We have an effect that translates unconditionally to an error, so mark the
                    // function unsafe
                    state.mark_unsafe(&func);
                    ret = false;
                } else if eff.kind.is_runnable() {
                    // If we pass an imported variable to a function that mutates it
                    // (directly or transitively), mark the current function as unsafe.
                    if self.call_mutates_imported_arg(eff) {
                        state.mark_unsafe(&func);
                        ret = false;
                    }
                    if call.stack.contains(&eff.name) {
                        // We have a recursive function call; mark it unsafe
                        state.mark_unsafe(&func);
                        ret = false;
                    } else {
                        call.stack.push(eff.name);
                        let child_caller_module = if !call.is_module_scope {
                            // When precomputing function safety, the child module is the immediate
                            // caller, so that we can inherit the "unsafe if imported" status
                            // independent of the original entry point.
                            call_module
                        } else {
                            // When checking module safety, thread the entry module through all
                            // calls, so that we can resolve the "unsafe if imported" effects from
                            // the previous pass with the correct value of "if imported".
                            call.caller_module
                        };
                        let mut child_call = Call {
                            caller_module: child_caller_module,
                            effect: eff,
                            func: eff.name,
                            stack: std::mem::take(&mut call.stack),
                            is_module_scope: call.is_module_scope,
                        };
                        if !self.check_call_safety(&mut child_call, state, false)? {
                            self.mark_caller_unsafe_for_failed_callee(&func, &child_call, state);
                            ret = false;
                        } else if state
                            .function_safety
                            .get(&child_call.func)
                            .is_some_and(|i| i.verdict == FunctionSafety::UnsafeIfImported)
                            && self.functions.get(&child_call.func) == Some(call_module)
                        {
                            // propagate `UnsafeIfImported` to the caller
                            state.mark_unsafe_if_imported(&func);
                            if is_cross_module_call {
                                ret = false;
                            }
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
                            let is_already_unsafe =
                                state.function_safety.get(&func).is_some_and(|info| {
                                    info.verdict >= FunctionSafety::UnsafeMissingDep
                                });
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
        }

        // We haven't detected any unsafe behaviour
        if state.function_safety.get(&func).is_none() {
            state.mark_safe(&func);
        }
        Ok(ret)
    }

    fn is_parameterized_decorator_call(&self, call: &Call) -> bool {
        matches!(
            call.effect.kind,
            EffectKind::DecoratorCall | EffectKind::ImportedDecoratorCall
        ) && matches!(call.effect.data, EffectData::Call(_))
    }
}
