/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_text_size::TextRange;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;

use crate::config::AnalysisConfig;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::exports::ExportType;
use crate::exports::Exports;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::imports::resolve_to_known_module;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::ModuleSafety;
use crate::module_safety::SafetyResult;
use crate::project::SafetyMap;
use crate::project::SideEffectMap;
use crate::pyrefly::sys_info::PythonVersion;
use crate::source_map::SourceMap;
use crate::source_map::Sources;
use crate::traits::ModuleNameExt;

/// Cached analysis results for a single Python library.
/// Contains all information needed to merge with other libraries
/// in a map-reduce analysis pipeline.
#[derive(Serialize, Deserialize)]
pub struct LibraryCache {
    pub modules: Vec<CachedModule>,
    pub exports: CachedExports,
}

/// Cached analysis for a single module within a library.
#[derive(Serialize, Deserialize)]
pub struct CachedModule {
    pub name: ModuleName,
    pub safety: CachedSafety,
    /// Resolved imports (edges in the import graph).
    pub imports: AHashSet<ModuleName>,
    /// Imports that could not be resolved to modules in the source DB.
    pub missing_imports: AHashSet<ModuleName>,
    /// `from X import Y` where X is in the library but X.Y is not.
    /// May be a submodule in another library or an attribute of X.
    pub ambiguous_imports: AHashSet<ModuleName>,
    /// Module-level imports never accessed in any scope (side-effect imports).
    pub side_effect_imports: AHashSet<ModuleName>,
    /// Per-function safety info from call graph analysis.
    /// Keys are function-local names (e.g., "helper" for `mod.helper`).
    pub function_safety: HashMap<String, FunctionSafetyInfo>,
}

/// Safety analysis result for a cached module.
#[derive(Serialize, Deserialize)]
pub enum CachedSafety {
    Ok(CachedModuleSafety),
    AnalysisError { message: String },
}

/// Detailed safety information for a module.
#[derive(Default, Serialize, Deserialize)]
pub struct CachedModuleSafety {
    pub errors: Vec<CachedError>,
    pub force_imports_eager_overrides: Vec<CachedError>,
    pub implicit_imports: Vec<ModuleName>,
}

/// A serializable safety error (without source location).
#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct CachedError {
    pub kind: ErrorKind,
    pub metadata: String,
}

/// Cached export information for a library.
#[derive(Serialize, Deserialize)]
pub struct CachedExports {
    pub definitions: Vec<(ModuleName, ExportType)>,
    pub re_exports: Vec<CachedReExport>,
    pub all: Vec<(ModuleName, Vec<String>)>,
    pub return_types: Vec<(ModuleName, ModuleName)>,
}

/// A cached re-export entry (module.attr -> source_module.source_attr).
#[derive(Serialize, Deserialize)]
pub struct CachedReExport {
    pub exported_module: ModuleName,
    pub exported_attr: String,
    pub imported_module: ModuleName,
    pub imported_attr: String,
}

/// A module's import edges from the graph, partitioned by resolution status.
struct GraphEdgeSets {
    imports: AHashSet<ModuleName>,
    missing_imports: AHashSet<ModuleName>,
    ambiguous_imports: AHashSet<ModuleName>,
}

fn graph_edge_sets(graph: &ImportGraph, name: &ModuleName) -> GraphEdgeSets {
    let imports = graph.get_imports(name).copied().collect();
    let missing_imports = graph
        .get_missing_imports(name)
        .map(|m| m.iter().copied().collect())
        .unwrap_or_default();
    let ambiguous_imports = graph
        .get_ambiguous_imports(name)
        .map(|m| m.iter().copied().collect())
        .unwrap_or_default();
    GraphEdgeSets {
        imports,
        missing_imports,
        ambiguous_imports,
    }
}

impl LibraryCache {
    pub fn empty() -> Self {
        LibraryCache {
            modules: Vec::new(),
            exports: CachedExports {
                definitions: Vec::new(),
                re_exports: Vec::new(),
                all: Vec::new(),
                return_types: Vec::new(),
            },
        }
    }

    /// Build a cache from the analysis pipeline results.
    pub fn build(
        safety_map: &SafetyMap,
        import_graph: &ImportGraph,
        exports: &Exports,
        side_effect_imports: &SideEffectMap,
    ) -> Self {
        let mut modules: Vec<CachedModule> = safety_map
            .par_iter()
            .map(|entry| {
                let name = *entry.key();
                let safety_result = entry.value();

                let GraphEdgeSets {
                    imports,
                    missing_imports,
                    ambiguous_imports,
                } = graph_edge_sets(import_graph, &name);

                let se_imports: AHashSet<ModuleName> = side_effect_imports
                    .get(&name)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();

                let function_safety = match safety_result {
                    SafetyResult::Ok(ms) => ms.function_safety.clone(),
                    _ => HashMap::new(),
                };

                let safety = CachedSafety::from_safety_result(safety_result);

                CachedModule {
                    name,
                    safety,
                    imports,
                    missing_imports,
                    ambiguous_imports,
                    side_effect_imports: se_imports,
                    function_safety,
                }
            })
            .collect();

        modules.sort_by_key(|m| m.name);

        let exports = CachedExports::from_exports(exports);

        LibraryCache { modules, exports }
    }

    /// Write the cache to a binary file using postcard.
    pub fn write_to_file(&self, path: &Path) -> anyhow::Result<()> {
        let bytes = postcard::to_allocvec(self)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Read a cache from a binary file using postcard.
    pub fn read_from_file(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)?;
        Ok(postcard::from_bytes(&bytes)?)
    }

    /// Merge dependency caches into this cache.
    /// When the same module appears in multiple caches (a .py file can belong
    /// to more than one python_library), module data is merged:
    /// - imports / side_effect_imports: union
    /// - missing_imports: intersection (only truly missing if unresolved everywhere)
    /// - safety: most conservative (most errors)
    pub fn merge_dep_caches(&mut self, dep_caches: Vec<LibraryCache>) {
        let extra_modules: usize = dep_caches.iter().map(|d| d.modules.len()).sum();
        self.modules.reserve(extra_modules);

        for dep in dep_caches {
            self.modules.extend(dep.modules);
            self.exports.merge(dep.exports);
        }
        self.modules.sort_by_key(|m| m.name);
        self.merge_duplicate_modules();
        self.exports.sort_and_dedup();
    }

    /// Merge consecutive modules with the same name (assumes sorted by name).
    fn merge_duplicate_modules(&mut self) {
        if self.modules.len() < 2 {
            return;
        }

        let mut write = 0;
        for read in 1..self.modules.len() {
            if self.modules[write].name == self.modules[read].name {
                let name = self.modules[read].name;
                let other = std::mem::replace(&mut self.modules[read], CachedModule::empty(name));
                self.modules[write].merge(other);
            } else {
                write += 1;
                if write != read {
                    self.modules.swap(write, read);
                }
            }
        }
        self.modules.truncate(write + 1);
    }

    /// Resolve ambiguous imports: `from X import Y` where X was in the library
    /// but X.Y was not. If X.Y resolves to a module in the merged set, it's a
    /// submodule — add it as a real import edge.
    /// Returns a map of module → newly resolved targets for downstream error clearing.
    fn resolve_ambiguous_imports(
        &mut self,
        module_names: &AHashSet<ModuleName>,
    ) -> HashMap<ModuleName, AHashSet<ModuleName>> {
        let mut resolved: HashMap<ModuleName, AHashSet<ModuleName>> = HashMap::new();
        for module in &mut self.modules {
            for ambiguous in module.ambiguous_imports.drain() {
                if let Some(target) = resolve_to_known_module(&ambiguous, module_names) {
                    module.imports.insert(target);
                    resolved.entry(module.name).or_default().insert(target);
                }
            }
        }
        resolved
    }

    /// Iteratively clear false errors: promoting the functions of a module
    /// whose missing imports are all resolved to `Safe` can make a caller
    /// error-free, which in turn promotes its functions. Repeat until a round
    /// promotes nothing or clears nothing.
    fn upgrade_missing_dep_functions(
        &mut self,
        module_names: &AHashSet<ModuleName>,
        func_safety_by_module: &mut HashMap<ModuleName, HashMap<String, FunctionSafetyInfo>>,
    ) {
        // Index of globally safe function names for O(1) unqualified lookups.
        // Only include functions from modules in `module_names` to match
        // `is_call_verified_safe`'s unqualified fallback.
        let mut globally_safe_funcs: AHashSet<String> = func_safety_by_module
            .iter()
            .filter(|(module, _)| module_names.contains(module))
            .flat_map(|(_, fs)| fs.iter())
            .filter(|(_, info)| info.verdict == FunctionSafety::Safe)
            .map(|(name, _)| name.clone())
            .collect();

        // Promote to a fixpoint: one promotion can unblock a caller next round.
        let mut num_promoted = 0;
        while self.promote_resolved_module_functions(
            module_names,
            func_safety_by_module,
            &mut globally_safe_funcs,
        ) {
            num_promoted += 1;
        }
        // Clear errors only as a consequence of a promotion (else stay conservative).
        if num_promoted > 0 {
            self.clear_verified_errors(module_names, func_safety_by_module, &globally_safe_funcs);
        }
        debug!("{} promotion iterations were made", num_promoted);
    }

    /// Promote an `UnsafeMissingDep` verdict to `Safe` only when every callee
    /// that caused it now resolves to a `Safe` function, so a missing dep that
    /// resolves to an *unsafe* function keeps the caller unsafe. Promoted names
    /// are recorded in `globally_safe_funcs`. Returns whether any verdict changed.
    fn promote_resolved_module_functions(
        &self,
        module_names: &AHashSet<ModuleName>,
        func_safety_by_module: &mut HashMap<ModuleName, HashMap<String, FunctionSafetyInfo>>,
        globally_safe_funcs: &mut AHashSet<String>,
    ) -> bool {
        let to_promote: Vec<(ModuleName, String)> = {
            // Reborrow as shared for parallel access.
            let func_safety_by_module = &*func_safety_by_module;
            let globally_safe_funcs = &*globally_safe_funcs;

            self.modules
                .par_iter()
                .filter_map(|module| {
                    func_safety_by_module
                        .get(&module.name)
                        .map(|fs| (module.name, fs))
                })
                .flat_map_iter(|(name, fs)| {
                    fs.iter()
                        .filter(move |(_, info)| {
                            can_promote_missing_dep_function(
                                info,
                                module_names,
                                func_safety_by_module,
                                globally_safe_funcs,
                            )
                        })
                        .map(move |(func_name, _)| (name, func_name.clone()))
                })
                .collect()
        };

        let promoted = !to_promote.is_empty();
        for (module_name, func_name) in to_promote {
            if let Some(info) = func_safety_by_module
                .get_mut(&module_name)
                .and_then(|fs| fs.get_mut(&func_name))
            {
                info.verdict = FunctionSafety::Safe;
                globally_safe_funcs.insert(func_name);
            }
        }
        promoted
    }

    /// Drop errors that the current per-function verdicts now verify as safe,
    /// using the global safe-function index for O(1) unqualified lookups.
    /// Returns whether any error was removed.
    fn clear_verified_errors(
        &mut self,
        module_names: &AHashSet<ModuleName>,
        func_safety_by_module: &HashMap<ModuleName, HashMap<String, FunctionSafetyInfo>>,
        globally_safe_funcs: &AHashSet<String>,
    ) -> bool {
        self.modules
            .par_iter_mut()
            .map(|module| {
                let CachedSafety::Ok(ref mut safety) = module.safety else {
                    return false;
                };
                retain_unverified_errors(safety, |func_name| {
                    is_call_verified_safe_indexed(
                        func_name,
                        module_names,
                        func_safety_by_module,
                        globally_safe_funcs,
                    )
                })
            })
            .reduce(|| false, |any_cleared, cleared| any_cleared || cleared)
    }

    /// Resolve missing imports against the merged cache and selectively clear
    /// false errors using per-function safety verdicts.
    pub fn resolve_cross_library_errors(&mut self) {
        let module_names: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let ambiguous_resolved = self.resolve_ambiguous_imports(&module_names);

        self.propagate_re_export_safety();

        let mut func_safety_by_module: HashMap<ModuleName, HashMap<String, FunctionSafetyInfo>> =
            self.modules
                .iter_mut()
                .map(|m| (m.name, std::mem::take(&mut m.function_safety)))
                .collect();

        self.modules.par_iter_mut().for_each(|module| {
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                resolve_implicit_imports(&mut safety.implicit_imports, &module_names);
            }

            let from_ambiguous = ambiguous_resolved.get(&module.name);

            if module.missing_imports.is_empty() && from_ambiguous.is_none() {
                return;
            }

            let mut still_missing: AHashSet<ModuleName> =
                AHashSet::with_capacity(module.missing_imports.len());
            let mut resolved_modules: AHashSet<ModuleName> =
                AHashSet::with_capacity(module.missing_imports.len());

            if let Some(from_ambiguous) = from_ambiguous {
                resolved_modules.extend(from_ambiguous.iter().copied());
            }

            for missing in module.missing_imports.drain() {
                if let Some(resolved) = resolve_to_known_module(&missing, &module_names) {
                    module.imports.insert(resolved);
                    resolved_modules.insert(resolved);
                } else {
                    still_missing.insert(missing);
                }
            }

            module.missing_imports = still_missing;

            if let CachedSafety::Ok(ref mut safety) = module.safety {
                retain_unverified_errors(safety, |func_name| {
                    is_call_verified_safe(func_name, &resolved_modules, &func_safety_by_module)
                });
            }
        });

        self.upgrade_missing_dep_functions(&module_names, &mut func_safety_by_module);

        for module in &mut self.modules {
            if let Some(fs) = func_safety_by_module.remove(&module.name) {
                module.function_safety = fs;
            }
        }
    }

    /// Propagate function_safety entries through re-exports.
    /// If module B re-exports `foo` from module C, and C has
    /// function_safety["foo"] = Safe, then B should also get that entry.
    #[doc(hidden)]
    pub fn propagate_re_export_safety(&mut self) {
        let module_index: HashMap<ModuleName, usize> = self
            .modules
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name, i))
            .collect();

        loop {
            let mut changed = false;
            for re in &self.exports.re_exports {
                let source_safety = module_index.get(&re.imported_module).and_then(|&idx| {
                    self.modules[idx]
                        .function_safety
                        .get(&re.imported_attr)
                        .cloned()
                });

                if let Some(safety) = source_safety {
                    if let Some(&idx) = module_index.get(&re.exported_module) {
                        match self.modules[idx]
                            .function_safety
                            .entry(re.exported_attr.clone())
                        {
                            std::collections::hash_map::Entry::Vacant(e) => {
                                e.insert(safety);
                                changed = true;
                            }
                            std::collections::hash_map::Entry::Occupied(mut e)
                                if safety.verdict < e.get().verdict =>
                            {
                                e.insert(safety);
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Reconstruct a SafetyMap from cached module data.
    pub fn to_safety_map(&self) -> SafetyMap {
        let map = SafetyMap::with_capacity(self.modules.len());
        for module in &self.modules {
            map.insert(module.name, module.safety.to_safety_result());
        }
        map
    }

    /// Inject the bundled stdlib stubs as graph-only nodes so the merged graph
    /// matches the e2e graph: per-library caches drop stub-only modules, losing
    /// the typeshed import cycle. Skips names a real library already provides.
    /// Returns the injected names so the caller can keep them out of the safety map.
    pub fn inject_bundled_stub_graph(
        &mut self,
        python_version: PythonVersion,
    ) -> AHashSet<ModuleName> {
        let sources =
            Sources::new_with_version(SourceMap::default(), PathBuf::new(), python_version);
        let config = AnalysisConfig::with_python_version(python_version, None);
        let graph = ImportGraph::make(&sources, &config);

        let existing: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let mut added = AHashSet::new();

        for name in graph.graph.node_names() {
            let name = *name;
            if existing.contains(&name) {
                continue;
            }
            let GraphEdgeSets {
                imports,
                missing_imports,
                ambiguous_imports,
            } = graph_edge_sets(&graph, &name);
            self.modules.push(CachedModule {
                imports,
                missing_imports,
                ambiguous_imports,
                ..CachedModule::empty(name)
            });
            added.insert(name);
        }
        added
    }

    /// Reconstruct an ImportGraph from cached module import edges.
    pub fn to_import_graph(&self) -> ImportGraph {
        let mut graph = ImportGraph::new();
        for module in &self.modules {
            graph.graph.add_node(&module.name);
        }
        for module in &self.modules {
            for imported in &module.imports {
                graph.graph.add_edge(&module.name, imported);
            }
            for missing in &module.missing_imports {
                graph.add_missing(&module.name, *missing);
            }
        }
        graph
    }

    /// Reconstruct a SideEffectMap from cached module data.
    pub fn to_side_effect_map(&self) -> SideEffectMap {
        let mut map = SideEffectMap::with_capacity(self.modules.len());
        for m in &self.modules {
            if !m.side_effect_imports.is_empty() {
                map.insert(m.name, m.side_effect_imports.iter().copied().collect());
            }
        }
        map
    }
}

/// Drop errors on `safety` that `is_verified_safe` confirms are safe, leaving
/// the rest. Returns whether any error was removed.
fn retain_unverified_errors(
    safety: &mut CachedModuleSafety,
    mut is_verified_safe: impl FnMut(&str) -> bool,
) -> bool {
    let before = safety.errors.len();
    safety.errors.retain(|e| {
        if !e.kind.could_be_caused_by_missing_import() {
            return true;
        }
        // `e.metadata` is the callee name recorded on the effect (see
        // `SafetyError::new_from_effect`), which may render the call with a
        // trailing `()`. `function_safety` is keyed by the bare function name,
        // so strip the `()` before looking the verdict up.
        !is_verified_safe(e.metadata.trim_end_matches("()"))
    });
    safety.errors.len() < before
}

fn lookup_in_safety_map(local_name: &str, fs: &HashMap<String, FunctionSafetyInfo>) -> bool {
    let is_safe = |key: &str| {
        fs.get(key)
            .is_some_and(|info| info.verdict == FunctionSafety::Safe)
    };
    if is_safe(local_name) {
        return true;
    }
    local_name
        .split_once('.')
        .is_some_and(|(prefix, _)| is_safe(prefix))
}

/// Check if a function call can be verified as safe using cached per-function
/// safety verdicts from the resolved modules.
///
/// For qualified names like "mod.sub.func", tries each parent as the module
/// prefix (longest first) and looks up the remainder in that module's
/// function_safety map.
/// For unqualified names like "helper", checks all resolved modules.
/// Returns true only if the function is found and verified Safe.
#[doc(hidden)]
pub fn is_call_verified_safe(
    func_name: &str,
    resolved_modules: &AHashSet<ModuleName>,
    func_safety_by_module: &HashMap<ModuleName, HashMap<String, FunctionSafetyInfo>>,
) -> bool {
    let fqn = ModuleName::from_str(func_name);
    for (parent, dot_pos) in fqn.iter_parents() {
        if resolved_modules.contains(&parent) {
            let local_name = &func_name[dot_pos + 1..];
            return func_safety_by_module
                .get(&parent)
                .is_some_and(|fs| lookup_in_safety_map(local_name, fs));
        }
    }

    resolved_modules
        .iter()
        .filter_map(|r| func_safety_by_module.get(r))
        .filter_map(|fs| fs.get(func_name))
        .any(|info| info.verdict == FunctionSafety::Safe)
}

fn can_promote_missing_dep_function(
    info: &FunctionSafetyInfo,
    module_names: &AHashSet<ModuleName>,
    func_safety_by_module: &HashMap<ModuleName, HashMap<String, FunctionSafetyInfo>>,
    globally_safe_funcs: &AHashSet<String>,
) -> bool {
    info.verdict == FunctionSafety::UnsafeMissingDep
        // Promote only with positive evidence: a non-empty callee set,
        // all now verified safe. No record (e.g. a re-export-propagated
        // verdict) stays conservatively unsafe.
        && !info.missing_dep_callees.is_empty()
        && info.missing_dep_callees.iter().all(|callee| {
            is_call_verified_safe_indexed(
                callee.as_str(),
                module_names,
                func_safety_by_module,
                globally_safe_funcs,
            )
        })
}

/// Like `is_call_verified_safe` but uses a pre-built index for the unqualified
/// name fallback, turning it from O(modules) to O(1).
fn is_call_verified_safe_indexed(
    func_name: &str,
    resolved_modules: &AHashSet<ModuleName>,
    func_safety_by_module: &HashMap<ModuleName, HashMap<String, FunctionSafetyInfo>>,
    globally_safe_funcs: &AHashSet<String>,
) -> bool {
    let fqn = ModuleName::from_str(func_name);
    for (parent, dot_pos) in fqn.iter_parents() {
        if resolved_modules.contains(&parent) {
            let local_name = &func_name[dot_pos + 1..];
            return func_safety_by_module
                .get(&parent)
                .is_some_and(|fs| lookup_in_safety_map(local_name, fs));
        }
    }

    globally_safe_funcs.contains(func_name)
}

#[doc(hidden)]
pub fn resolve_implicit_imports(
    implicit_imports: &mut Vec<ModuleName>,
    module_names: &AHashSet<ModuleName>,
) {
    let mut seen = AHashSet::with_capacity(implicit_imports.len());
    implicit_imports.retain_mut(|imp| {
        if let Some(resolved) = resolve_to_known_module(imp, module_names) {
            *imp = resolved;
            return seen.insert(resolved);
        }
        seen.insert(*imp)
    });
}

impl CachedModule {
    fn empty(name: ModuleName) -> Self {
        CachedModule {
            name,
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: AHashSet::new(),
            missing_imports: AHashSet::new(),
            ambiguous_imports: AHashSet::new(),
            side_effect_imports: AHashSet::new(),
            function_safety: HashMap::new(),
        }
    }

    pub fn is_safe(&self) -> bool {
        matches!(&self.safety, CachedSafety::Ok(s) if s.is_safe())
    }

    /// Merge another CachedModule (same name) into this one.
    fn merge(&mut self, other: CachedModule) {
        self.imports.extend(other.imports);
        self.missing_imports
            .retain(|m| other.missing_imports.contains(m));
        self.ambiguous_imports.extend(other.ambiguous_imports);
        self.side_effect_imports.extend(other.side_effect_imports);
        self.safety.merge(other.safety);
        for (name, info) in other.function_safety {
            self.function_safety
                .entry(name)
                .and_modify(|existing| existing.merge(info.clone()))
                .or_insert(info);
        }
    }
}

impl CachedSafety {
    /// Merge another safety result, keeping the more conservative outcome.
    /// AnalysisError always wins. Between two Ok results, keep the union of errors.
    fn merge(&mut self, other: CachedSafety) {
        match (&mut *self, other) {
            // AnalysisError is the most conservative — keep it
            (CachedSafety::AnalysisError { .. }, _) => {}
            (_, other @ CachedSafety::AnalysisError { .. }) => *self = other,
            // Both Ok: merge errors and overrides
            (CachedSafety::Ok(this), CachedSafety::Ok(other)) => {
                merge_errors(&mut this.errors, other.errors);
                merge_errors(
                    &mut this.force_imports_eager_overrides,
                    other.force_imports_eager_overrides,
                );

                this.implicit_imports.extend(other.implicit_imports);
                this.implicit_imports.sort();
                this.implicit_imports.dedup();
            }
        }
    }

    /// Convert back to a SafetyResult for pipeline reconstruction.
    pub fn to_safety_result(&self) -> SafetyResult {
        match self {
            CachedSafety::Ok(safety) => {
                let mut module_safety = ModuleSafety::new();
                for error in &safety.errors {
                    module_safety.add_error(error.to_safety_error());
                }
                for override_err in &safety.force_imports_eager_overrides {
                    module_safety.add_force_import_override(override_err.to_safety_error());
                }
                module_safety.implicit_imports = safety.implicit_imports.clone();
                SafetyResult::Ok(module_safety)
            }
            CachedSafety::AnalysisError { message } => {
                SafetyResult::AnalysisError(anyhow::anyhow!("{}", message))
            }
        }
    }

    fn from_safety_result(result: &SafetyResult) -> Self {
        match result {
            SafetyResult::Ok(safety) => CachedSafety::Ok(CachedModuleSafety {
                errors: safety
                    .errors
                    .iter()
                    .map(CachedError::from_safety_error)
                    .collect(),
                force_imports_eager_overrides: safety
                    .force_imports_eager_overrides
                    .iter()
                    .map(CachedError::from_safety_error)
                    .collect(),
                implicit_imports: {
                    let mut v = safety.implicit_imports.clone();
                    v.sort();
                    v
                },
            }),
            SafetyResult::AnalysisError(e) => CachedSafety::AnalysisError {
                message: e.to_string(),
            },
        }
    }
}

impl CachedModuleSafety {
    pub fn is_safe(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn should_load_imports_eagerly(&self) -> bool {
        !self.force_imports_eager_overrides.is_empty()
    }
}

fn merge_errors(target: &mut Vec<CachedError>, other: Vec<CachedError>) {
    target.extend(other);
    target.sort();
    target.dedup();
}

impl CachedError {
    fn from_safety_error(error: &SafetyError) -> Self {
        CachedError {
            kind: error.kind,
            metadata: error.metadata.as_str().to_string(),
        }
    }

    fn to_safety_error(&self) -> SafetyError {
        SafetyError::new(self.kind, self.metadata.clone(), TextRange::default())
    }
}

impl CachedExports {
    fn from_exports(exports: &Exports) -> Self {
        let definitions: Vec<(ModuleName, ExportType)> = exports
            .get_exports()
            .map(|(name, export)| (*name, export.typ))
            .collect();

        let re_exports: Vec<CachedReExport> = exports
            .get_re_exports()
            .map(|(exported, (imported, _range))| CachedReExport {
                exported_module: exported.module,
                exported_attr: exported.attr.to_string(),
                imported_module: imported.module,
                imported_attr: imported.attr.to_string(),
            })
            .collect();

        let all: Vec<(ModuleName, Vec<String>)> = exports
            .iter_all()
            .map(|(name, names)| (*name, names.iter().map(|n| n.to_string()).collect()))
            .collect();

        let return_types: Vec<(ModuleName, ModuleName)> =
            exports.iter_return_types().map(|(k, v)| (*k, *v)).collect();

        let mut result = CachedExports {
            definitions,
            re_exports,
            all,
            return_types,
        };
        result.sort_and_dedup();
        result
    }

    fn sort_and_dedup(&mut self) {
        self.definitions.sort_by_key(|(name, _)| *name);
        self.definitions.dedup_by_key(|(name, _)| *name);

        self.re_exports.sort_by(|a, b| {
            (&a.exported_module, &a.exported_attr).cmp(&(&b.exported_module, &b.exported_attr))
        });
        self.re_exports.dedup_by(|a, b| {
            a.exported_module == b.exported_module && a.exported_attr == b.exported_attr
        });

        self.all.sort_by_key(|(name, _)| *name);
        self.all.dedup_by_key(|(name, _)| *name);

        self.return_types.sort_by_key(|(k, _)| *k);
        self.return_types.dedup_by_key(|(k, _)| *k);
    }

    fn merge(&mut self, other: CachedExports) {
        self.definitions.extend(other.definitions);
        self.re_exports.extend(other.re_exports);
        self.all.extend(other.all);
        self.return_types.extend(other.return_types);
    }
}

#[cfg(test)]
mod tests {
    use rayon::ThreadPoolBuilder;

    use super::*;

    #[test]
    fn clear_verified_errors_processes_every_module() {
        let module_a = ModuleName::from_str("test.module_a");
        let module_b = ModuleName::from_str("test.module_b");

        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: module_a,
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "helper()".to_owned(),
                        }],
                        force_imports_eager_overrides: Vec::new(),
                        implicit_imports: Vec::new(),
                    }),
                    imports: AHashSet::new(),
                    missing_imports: AHashSet::new(),
                    ambiguous_imports: AHashSet::new(),
                    side_effect_imports: AHashSet::new(),
                    function_safety: HashMap::new(),
                },
                CachedModule {
                    name: module_b,
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "helper()".to_owned(),
                        }],
                        force_imports_eager_overrides: Vec::new(),
                        implicit_imports: Vec::new(),
                    }),
                    imports: AHashSet::new(),
                    missing_imports: AHashSet::new(),
                    ambiguous_imports: AHashSet::new(),
                    side_effect_imports: AHashSet::new(),
                    function_safety: HashMap::new(),
                },
            ],
            exports: CachedExports {
                definitions: Vec::new(),
                re_exports: Vec::new(),
                all: Vec::new(),
                return_types: Vec::new(),
            },
        };

        let module_names: AHashSet<ModuleName> = [module_a, module_b].into_iter().collect();
        let func_safety_by_module = HashMap::from([
            (
                module_a,
                HashMap::from([(
                    "helper".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                )]),
            ),
            (
                module_b,
                HashMap::from([(
                    "helper".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                )]),
            ),
        ]);
        let globally_safe_funcs: AHashSet<String> = ["helper".to_owned()].into_iter().collect();

        let cleared = ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("should build test thread pool")
            .install(|| {
                cache.clear_verified_errors(
                    &module_names,
                    &func_safety_by_module,
                    &globally_safe_funcs,
                )
            });

        assert!(
            cleared,
            "expected at least one verified error to be removed"
        );
        assert!(
            cache.modules.iter().all(CachedModule::is_safe),
            "all modules should have their verified errors cleared",
        );
    }
}
