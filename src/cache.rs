/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::path::Path;

use ahash::AHashSet;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_text_size::TextRange;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;

use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::exports::ExportType;
use crate::exports::Exports;
use crate::imports::ImportGraph;
use crate::imports::resolve_to_known_module;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::ModuleSafety;
use crate::module_safety::SafetyResult;
use crate::project::SafetyMap;
use crate::project::SideEffectMap;
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

                let imports: AHashSet<ModuleName> =
                    import_graph.get_imports(&name).cloned().collect();

                let missing_imports: AHashSet<ModuleName> = import_graph
                    .get_missing_imports(&name)
                    .map(|m| m.iter().cloned().collect())
                    .unwrap_or_default();

                let ambiguous_imports: AHashSet<ModuleName> = import_graph
                    .get_ambiguous_imports(&name)
                    .map(|m| m.iter().cloned().collect())
                    .unwrap_or_default();

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
        let mut to_promote: Vec<(ModuleName, String)> = Vec::new();
        for module in &self.modules {
            let Some(fs) = func_safety_by_module.get(&module.name) else {
                continue;
            };
            for (func_name, info) in fs {
                if can_promote_missing_dep_function(
                    info,
                    module_names,
                    func_safety_by_module,
                    globally_safe_funcs,
                ) {
                    to_promote.push((module.name, func_name.clone()));
                }
            }
        }

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
        let mut cleared = false;
        for module in &mut self.modules {
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                cleared |= retain_unverified_errors(safety, |func_name| {
                    is_call_verified_safe_indexed(
                        func_name,
                        module_names,
                        func_safety_by_module,
                        globally_safe_funcs,
                    )
                });
            }
        }
        cleared
    }

    /// Resolve missing imports against the merged cache and selectively clear
    /// false errors using per-function safety verdicts.
    pub fn resolve_cross_library_errors(&mut self) {
        let module_names: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let mut ambiguous_resolved = self.resolve_ambiguous_imports(&module_names);

        self.propagate_re_export_safety();

        let mut func_safety_by_module: HashMap<ModuleName, HashMap<String, FunctionSafetyInfo>> =
            self.modules
                .iter_mut()
                .map(|m| (m.name, std::mem::take(&mut m.function_safety)))
                .collect();

        for module in &mut self.modules {
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                resolve_implicit_imports(&mut safety.implicit_imports, &module_names);
            }

            let from_ambiguous = ambiguous_resolved.remove(&module.name);

            if module.missing_imports.is_empty() && from_ambiguous.is_none() {
                continue;
            }

            let mut still_missing: AHashSet<ModuleName> =
                AHashSet::with_capacity(module.missing_imports.len());
            let mut resolved_modules: AHashSet<ModuleName> =
                AHashSet::with_capacity(module.missing_imports.len());

            if let Some(from_ambiguous) = from_ambiguous {
                resolved_modules.extend(from_ambiguous);
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
        }

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
    fn propagate_re_export_safety(&mut self) {
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
fn is_call_verified_safe(
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

fn resolve_implicit_imports(
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
    use ahash::AHashMap;
    use ahash::AHashSet;
    use dashmap::DashMap;
    use ruff_text_size::TextRange;

    use super::*;
    use crate::errors::ErrorKind;
    use crate::errors::SafetyError;
    use crate::module_safety::ModuleSafety;
    use crate::module_safety::SafetyResult;

    fn mn(s: &str) -> ModuleName {
        ModuleName::from_str(s)
    }

    fn build_cache(sources: &crate::test_lib::TestSources) -> LibraryCache {
        use crate::config::AnalysisConfig;
        use crate::imports::ImportGraph;
        use crate::project;

        let config = AnalysisConfig::default();
        let (import_graph, exports) = ImportGraph::make_with_exports(sources, &config);
        let output = project::run_analysis(
            sources,
            &exports,
            &import_graph,
            &config,
            project::CachingMode::Enabled,
        );
        LibraryCache::build(
            &output.safety_map,
            &import_graph,
            &exports,
            &output.side_effect_imports,
        )
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_cached_struct_sizes() {
        assert_eq!(std::mem::size_of::<LibraryCache>(), 120);
        assert_eq!(std::mem::size_of::<CachedModule>(), 384);
        assert_eq!(std::mem::size_of::<CachedSafety>(), 72);
        assert_eq!(std::mem::size_of::<CachedModuleSafety>(), 72);
        assert_eq!(std::mem::size_of::<CachedError>(), 32);
        assert_eq!(std::mem::size_of::<CachedExports>(), 96);
        assert_eq!(std::mem::size_of::<CachedReExport>(), 64);
    }

    #[test]
    fn test_cache_round_trip() {
        let safety_map: SafetyMap = DashMap::new();

        // Safe module
        safety_map.insert(mn("foo"), SafetyResult::Ok(ModuleSafety::new()));

        // Unsafe module
        let mut unsafe_safety = ModuleSafety::new();
        unsafe_safety.add_error(SafetyError::new(
            ErrorKind::UnsafeFunctionCall,
            "bad_func()".to_string(),
            TextRange::default(),
        ));
        safety_map.insert(mn("bar"), SafetyResult::Ok(unsafe_safety));

        let mut import_graph = ImportGraph::new();
        import_graph.graph.add_node(&mn("foo"));
        import_graph.graph.add_node(&mn("bar"));
        import_graph.graph.add_edge(&mn("foo"), &mn("bar"));

        let exports = Exports::empty();
        let side_effect_imports: SideEffectMap = AHashMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let bytes = postcard::to_allocvec(&cache).unwrap();
        let loaded: LibraryCache = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(loaded.modules.len(), 2);

        let foo = loaded.modules.iter().find(|m| m.name == mn("foo")).unwrap();
        assert!(matches!(&foo.safety, CachedSafety::Ok(s) if s.is_safe()));
        assert!(foo.imports.contains(&mn("bar")));

        let bar = loaded.modules.iter().find(|m| m.name == mn("bar")).unwrap();
        match &bar.safety {
            CachedSafety::Ok(s) => {
                assert_eq!(s.errors.len(), 1);
                assert_eq!(s.errors[0].kind, ErrorKind::UnsafeFunctionCall);
                assert_eq!(s.errors[0].metadata, "bad_func()");
            }
            _ => panic!("Expected Ok safety"),
        }
    }

    #[test]
    fn test_cache_analysis_error() {
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(
            mn("broken"),
            SafetyResult::AnalysisError(anyhow::anyhow!("parse failed")),
        );

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let side_effect_imports: SideEffectMap = AHashMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let bytes = postcard::to_allocvec(&cache).unwrap();
        let loaded: LibraryCache = postcard::from_bytes(&bytes).unwrap();

        let broken = loaded
            .modules
            .iter()
            .find(|m| m.name == mn("broken"))
            .unwrap();
        assert!(
            matches!(&broken.safety, CachedSafety::AnalysisError { message } if message == "parse failed")
        );
    }

    #[test]
    fn test_cache_serialize_deserialize_bytes() {
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(mn("test"), SafetyResult::Ok(ModuleSafety::new()));

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let side_effect_imports: SideEffectMap = AHashMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);

        let bytes = postcard::to_allocvec(&cache).unwrap();
        let loaded: LibraryCache = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(loaded.modules.len(), 1);
        assert_eq!(loaded.modules[0].name, mn("test"));
    }

    #[test]
    fn test_cache_from_pipeline() {
        use crate::test_lib::TestSources;

        let sources = TestSources::new(&[
            ("foo", "import bar\nx = bar.func()\n"),
            ("bar", "def func(): return 1\n"),
        ]);
        let cache = build_cache(&sources);

        // Both modules should be in the cache (stubs are filtered by run_analysis)
        assert_eq!(cache.modules.len(), 2);

        // Both should be safe (bar.func is safe to call)
        for m in &cache.modules {
            assert!(
                matches!(&m.safety, CachedSafety::Ok(s) if s.is_safe()),
                "Module {} should be safe",
                m.name.as_str()
            );
        }

        // foo should import bar
        let foo = cache.modules.iter().find(|m| m.name == mn("foo")).unwrap();
        assert!(foo.imports.contains(&mn("bar")));

        // Round-trip through postcard
        let bytes = postcard::to_allocvec(&cache).unwrap();
        let loaded: LibraryCache = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.modules.len(), 2);
    }

    #[test]
    fn test_constructor_call_caches_class_level_safety() {
        use crate::test_lib::TestSources;

        let cache = build_cache(&TestSources::new(&[
            (
                "defs",
                "from dataclasses import dataclass\n\
                 @dataclass\n\
                 class Safe:\n\
                 \x20   value: int = 0\n",
            ),
            ("caller", "from defs import Safe\nobj = Safe()\n"),
        ]));

        let defs_mod = cache.modules.iter().find(|m| m.name == mn("defs")).unwrap();
        assert!(
            defs_mod.function_safety.contains_key("Safe"),
            "function_safety should contain class-level entry 'Safe', got keys: {:?}",
            defs_mod.function_safety.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            defs_mod
                .function_safety
                .get("Safe")
                .map(|info| info.verdict),
            Some(FunctionSafety::Safe),
        );
    }

    #[test]
    fn test_cache_with_load_imports_eagerly() {
        let safety_map: SafetyMap = DashMap::new();
        let mut safety = ModuleSafety::new();
        safety.add_force_import_override(SafetyError::new(
            ErrorKind::ExecCall,
            "exec()".to_string(),
            TextRange::default(),
        ));
        safety_map.insert(mn("exec_mod"), SafetyResult::Ok(safety));

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let side_effect_imports: SideEffectMap = AHashMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let bytes = postcard::to_allocvec(&cache).unwrap();
        let loaded: LibraryCache = postcard::from_bytes(&bytes).unwrap();

        let exec_mod = loaded
            .modules
            .iter()
            .find(|m| m.name == mn("exec_mod"))
            .unwrap();
        match &exec_mod.safety {
            CachedSafety::Ok(s) => {
                assert!(s.is_safe());
                assert!(s.should_load_imports_eagerly());
                assert_eq!(s.force_imports_eager_overrides.len(), 1);
                assert_eq!(s.force_imports_eager_overrides[0].kind, ErrorKind::ExecCall);
            }
            _ => panic!("Expected Ok safety"),
        }
    }

    #[test]
    fn test_cache_side_effect_imports() {
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(mn("a"), SafetyResult::Ok(ModuleSafety::new()));

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();

        let mut side_effect_imports: SideEffectMap = AHashMap::new();
        let mut se = AHashSet::new();
        se.insert(mn("unused_dep"));
        side_effect_imports.insert(mn("a"), se);

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let bytes = postcard::to_allocvec(&cache).unwrap();
        let loaded: LibraryCache = postcard::from_bytes(&bytes).unwrap();

        let a = loaded.modules.iter().find(|m| m.name == mn("a")).unwrap();
        assert!(a.side_effect_imports.contains(&mn("unused_dep")));
    }

    #[test]
    fn test_cache_sorted_output() {
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(mn("z_mod"), SafetyResult::Ok(ModuleSafety::new()));
        safety_map.insert(mn("a_mod"), SafetyResult::Ok(ModuleSafety::new()));
        safety_map.insert(mn("m_mod"), SafetyResult::Ok(ModuleSafety::new()));

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let side_effect_imports: SideEffectMap = AHashMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);

        let names: Vec<&str> = cache.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a_mod", "m_mod", "z_mod"]);
    }

    #[test]
    fn test_merge_dep_caches() {
        // Build own cache with 1 module
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(mn("own"), SafetyResult::Ok(ModuleSafety::new()));

        let mut cache = LibraryCache::build(
            &safety_map,
            &ImportGraph::new(),
            &Exports::empty(),
            &AHashMap::new(),
        );
        assert_eq!(cache.modules.len(), 1);

        // Build a dep cache with 2 modules
        let dep_safety_map: SafetyMap = DashMap::new();
        dep_safety_map.insert(mn("dep_a"), SafetyResult::Ok(ModuleSafety::new()));
        let mut unsafe_safety = ModuleSafety::new();
        unsafe_safety.add_error(SafetyError::new(
            ErrorKind::UnsafeFunctionCall,
            "bad()".to_string(),
            TextRange::default(),
        ));
        dep_safety_map.insert(mn("dep_b"), SafetyResult::Ok(unsafe_safety));

        let dep_cache = LibraryCache::build(
            &dep_safety_map,
            &ImportGraph::new(),
            &Exports::empty(),
            &AHashMap::new(),
        );

        cache.merge_dep_caches(vec![dep_cache]);

        // Should now have 3 modules, sorted
        assert_eq!(cache.modules.len(), 3);
        let names: Vec<&str> = cache.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["dep_a", "dep_b", "own"]);

        // dep_b should still have its error
        let dep_b = cache
            .modules
            .iter()
            .find(|m| m.name == mn("dep_b"))
            .unwrap();
        match &dep_b.safety {
            CachedSafety::Ok(s) => {
                assert_eq!(s.errors.len(), 1);
                assert_eq!(s.errors[0].kind, ErrorKind::UnsafeFunctionCall);
            }
            _ => panic!("Expected Ok safety"),
        }
    }

    /// Simulates the sample_project scenario:
    /// - sample_lib has 5 modules (dep), analyzed alone to produce a dep cache
    /// - sample_project-library has 1 own module, analyzed separately + merged
    ///
    /// Verifies that own build + merge produces the same results as full analysis.
    #[test]
    fn test_own_build_plus_merge_matches_full_build() {
        use crate::test_lib::TestSources;

        let dep_modules: Vec<(&str, &str)> = vec![
            ("safe_module", "def greet(name): return f'Hello, {name}'\n"),
            (
                "unsafe_module",
                "import os\nresult = os.path.join('a', 'b')\ndef helper(): return result\n",
            ),
            (
                "importer",
                "from safe_module import greet\nfrom unsafe_module import helper\n",
            ),
            (
                "has_finalizer",
                "class Leaker:\n    def __del__(self):\n        pass\n",
            ),
            ("uses_exec", "exec('x = 1')\n"),
        ];
        let own_module = (
            "main",
            "from importer import greet\ndef main():\n    print(greet('world'))\n",
        );

        // --- Step 1: Analyze dep modules alone → dep cache ---
        let dep_cache = build_cache(&TestSources::new(&dep_modules));
        assert_eq!(dep_cache.modules.len(), 5);

        // --- Step 2: Analyze own module alone → own cache ---
        let mut own_cache = build_cache(&TestSources::new(&[own_module]));
        assert_eq!(own_cache.modules.len(), 1);

        // --- Step 3: Merge ---
        own_cache.merge_dep_caches(vec![dep_cache]);
        assert_eq!(own_cache.modules.len(), 6);

        // --- Step 4: Full analysis for comparison ---
        let mut all_modules = dep_modules.clone();
        all_modules.push(own_module);
        let full_cache = build_cache(&TestSources::new(&all_modules));
        assert_eq!(full_cache.modules.len(), 6);

        // Same module names
        let full_names: Vec<&str> = full_cache.modules.iter().map(|m| m.name.as_str()).collect();
        let merged_names: Vec<&str> = own_cache.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(full_names, merged_names);

        // Same safety status for every module
        for (full_mod, merged_mod) in full_cache.modules.iter().zip(own_cache.modules.iter()) {
            let full_safe = matches!(&full_mod.safety, CachedSafety::Ok(s) if s.is_safe());
            let merged_safe = matches!(&merged_mod.safety, CachedSafety::Ok(s) if s.is_safe());
            assert_eq!(
                full_safe,
                merged_safe,
                "Module {} safety mismatch: full={}, merged={}",
                full_mod.name.as_str(),
                full_safe,
                merged_safe
            );
        }
    }

    #[test]
    fn test_resolve_cross_library_constructor_call() {
        use crate::test_lib::TestSources;

        let dep_cache = build_cache(&TestSources::new(&[(
            "dep",
            "from dataclasses import dataclass\n\
             @dataclass\n\
             class MyClass:\n\
             \x20   value: int = 0\n",
        )]));

        let own_sources = TestSources::new(&[(
            "caller",
            "from dep import MyClass\n\
             instance = MyClass()\n",
        )]);
        let mut own_cache = build_cache(&own_sources);

        let caller_before = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (dep is missing)"
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller_after = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            caller_after.is_safe(),
            "caller should be safe after resolving cross-library constructor call"
        );
    }

    #[test]
    fn test_resolve_cross_library_unsafe_constructor() {
        use crate::test_lib::TestSources;

        let dep_cache = build_cache(&TestSources::new(&[
            (
                "dep",
                "import dep_state\n\
             class MyClass:\n\
             \x20   def __init__(self):\n\
             \x20       dep_state.counter = dep_state.counter + 1\n",
            ),
            ("dep_state", "counter = 0\n"),
        ]));

        let own_sources = TestSources::new(&[(
            "caller",
            "from dep import MyClass\n\
             instance = MyClass()\n",
        )]);
        let mut own_cache = build_cache(&own_sources);

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller.is_safe(),
            "caller should remain unsafe when constructor has side effects"
        );
    }

    #[test]
    fn test_resolve_cross_library_unsafe_if_imported_constructor() {
        use crate::test_lib::TestSources;

        let dep_cache = build_cache(&TestSources::new(&[(
            "defs",
            "counter = 0\n\
             class Foo:\n\
             \x20   def __init__(self):\n\
             \x20       global counter\n\
             \x20       counter += 1\n\
             obj = Foo()\n",
        )]));

        let defs_mod = dep_cache
            .modules
            .iter()
            .find(|m| m.name == mn("defs"))
            .unwrap();
        assert_ne!(
            defs_mod.function_safety.get("Foo").map(|info| info.verdict),
            Some(FunctionSafety::Safe),
            "class Foo must not be cached as Safe when __init__ mutates module globals"
        );

        let own_sources = TestSources::new(&[(
            "caller",
            "from defs import Foo\n\
             instance = Foo()\n",
        )]);
        let mut own_cache = build_cache(&own_sources);

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller.is_safe(),
            "caller should remain unsafe: Foo.__init__ mutates module globals"
        );
    }

    #[test]
    fn test_resolve_to_known_module_exact_and_parent() {
        let known: AHashSet<ModuleName> = [mn("foo"), mn("bar.baz")].into_iter().collect();

        assert_eq!(resolve_to_known_module(&mn("foo"), &known), Some(mn("foo")));
        assert_eq!(
            resolve_to_known_module(&mn("bar.baz"), &known),
            Some(mn("bar.baz"))
        );
        assert_eq!(
            resolve_to_known_module(&mn("bar.baz.Qux"), &known),
            Some(mn("bar.baz")),
        );
        assert_eq!(resolve_to_known_module(&mn("unknown"), &known), None);
    }

    #[test]
    fn test_resolve_implicit_imports_dotted_paths() {
        let known: AHashSet<ModuleName> = [mn("dep"), mn("other")].into_iter().collect();

        let mut implicits = vec![mn("dep.ClassName"), mn("other"), mn("missing.Foo")];
        resolve_implicit_imports(&mut implicits, &known);

        assert_eq!(implicits, vec![mn("dep"), mn("other"), mn("missing.Foo")]);
    }

    #[test]
    fn test_resolve_implicit_imports_deduplicates() {
        let known: AHashSet<ModuleName> = [mn("dep")].into_iter().collect();

        let mut implicits = vec![mn("dep.ClassA"), mn("dep.ClassB"), mn("dep")];
        resolve_implicit_imports(&mut implicits, &known);

        assert_eq!(implicits, vec![mn("dep")]);
    }

    #[test]
    fn test_precompute_function_safety_populates_all_functions() {
        use crate::test_lib::TestSources;

        let cache = build_cache(&TestSources::new(&[(
            "mod_a",
            "def helper(): return 1\ndef unused(): return 2\n",
        )]));

        let mod_a = cache
            .modules
            .iter()
            .find(|m| m.name == mn("mod_a"))
            .unwrap();
        assert!(
            mod_a.function_safety.contains_key("helper"),
            "helper should have a function_safety entry, got keys: {:?}",
            mod_a.function_safety.keys().collect::<Vec<_>>()
        );
        assert!(
            mod_a.function_safety.contains_key("unused"),
            "unused should have a function_safety entry, got keys: {:?}",
            mod_a.function_safety.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_propagate_re_export_replaces_conservative_verdict() {
        let mut cache = LibraryCache::empty();

        // Module C defines `foo` as Safe
        cache.modules.push(CachedModule {
            name: mn("c"),
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: AHashSet::new(),
            missing_imports: AHashSet::new(),
            ambiguous_imports: AHashSet::new(),
            side_effect_imports: AHashSet::new(),
            function_safety: HashMap::from([(
                "foo".to_string(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]),
        });

        // Module B re-exports `foo` from C, but already has it as UnsafeMissingDep
        cache.modules.push(CachedModule {
            name: mn("b"),
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: AHashSet::new(),
            missing_imports: AHashSet::new(),
            ambiguous_imports: AHashSet::new(),
            side_effect_imports: AHashSet::new(),
            function_safety: HashMap::from([(
                "foo".to_string(),
                FunctionSafetyInfo::new(FunctionSafety::UnsafeMissingDep),
            )]),
        });

        cache.exports.re_exports.push(CachedReExport {
            exported_module: mn("b"),
            exported_attr: "foo".to_string(),
            imported_module: mn("c"),
            imported_attr: "foo".to_string(),
        });

        cache.propagate_re_export_safety();

        let b = cache.modules.iter().find(|m| m.name == mn("b")).unwrap();
        assert_eq!(
            b.function_safety.get("foo").map(|info| info.verdict),
            Some(FunctionSafety::Safe),
            "propagation should replace UnsafeMissingDep with Safe from source module"
        );
    }

    #[test]
    fn test_resolve_cross_library_function_call() {
        use crate::test_lib::TestSources;

        let dep_cache = build_cache(&TestSources::new(&[("dep", "def safe_func(): return 1\n")]));

        let mut own_cache = build_cache(&TestSources::new(&[(
            "caller",
            "from dep import safe_func\nx = safe_func()\n",
        )]));

        let caller_before = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (dep is missing)"
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller_after = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            caller_after.is_safe(),
            "caller should be safe after resolving cross-library function call"
        );
    }

    #[test]
    fn test_errors_not_cleared_without_missing_imports() {
        let safety_map: SafetyMap = DashMap::new();
        let mut unsafe_safety = ModuleSafety::new();
        unsafe_safety.add_error(SafetyError::new(
            ErrorKind::UnknownFunctionCall,
            "dep.helper()".to_string(),
            TextRange::default(),
        ));
        safety_map.insert(mn("caller"), SafetyResult::Ok(unsafe_safety));

        let mut dep_safety = ModuleSafety::new();
        dep_safety.function_safety.insert(
            "helper".to_string(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        safety_map.insert(mn("dep"), SafetyResult::Ok(dep_safety));

        let mut import_graph = ImportGraph::new();
        import_graph.graph.add_node(&mn("caller"));
        import_graph.graph.add_node(&mn("dep"));
        import_graph.graph.add_edge(&mn("caller"), &mn("dep"));

        let exports = Exports::empty();
        let side_effect_imports: SideEffectMap = AHashMap::new();
        let mut cache =
            LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);

        assert!(
            cache
                .modules
                .iter()
                .find(|m| m.name == mn("caller"))
                .unwrap()
                .missing_imports
                .is_empty(),
            "no missing imports"
        );

        cache.resolve_cross_library_errors();

        let caller = cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller.is_safe(),
            "errors from already-imported modules should not be cleared (conservative)"
        );
    }

    #[test]
    fn test_error_cleared_from_ambiguous_import() {
        use crate::test_lib::TestSources;

        // dep library: pkg/__init__.py defines helper()
        let dep_cache = build_cache(&TestSources::new(&[
            ("pkg", ""),
            ("pkg.sub", "def helper(): return 1\n"),
        ]));

        // caller library: 'from pkg import sub' is ambiguous (pkg in graph, pkg.sub not)
        // then calls sub.helper() → produces an error since sub can't be resolved
        let mut own_cache = build_cache(&TestSources::new(&[(
            "caller",
            "from pkg import sub\nx = sub.helper()\n",
        )]));

        let caller_before = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (pkg.sub is unresolved)"
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            caller.imports.contains(&mn("pkg.sub")),
            "ambiguous import pkg.sub should be resolved as a real import"
        );
        assert!(
            caller.is_safe(),
            "caller error should be cleared once the ambiguous import feeds into error clearing"
        );
    }

    /// A function that is `UnsafeMissingDep` only because of a cross-library
    /// callee must NOT be promoted to `Safe` if that callee turns out to be
    /// unsafe once resolved. Otherwise a transitive caller is wrongly cleared.
    ///
    ///   dep.g  -> intrinsically unsafe (recursive)
    ///   mid.f  -> calls dep.g; UnsafeMissingDep while dep is missing
    ///   top    -> calls f() at module scope
    #[test]
    fn test_missing_dep_promotion_blocked_by_unsafe_callee() {
        use crate::test_lib::TestSources;

        let dep_cache = build_cache(&TestSources::new(&[("dep", "def g():\n    g()\n")]));

        let mut own_cache = build_cache(&TestSources::new(&[
            ("mid", "from dep import g\ndef f():\n    g()\n"),
            ("top", "from mid import f\nf()\n"),
        ]));

        assert!(
            !own_cache
                .modules
                .iter()
                .find(|m| m.name == mn("top"))
                .unwrap()
                .is_safe(),
            "top unsafe before merge (mid is missing)"
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let top = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("top"))
            .unwrap();
        assert!(
            !top.is_safe(),
            "top must stay unsafe: importing it runs f() -> unsafe g()"
        );
    }

    /// The flip side: when the previously-missing callee resolves to a *safe*
    /// function, the `UnsafeMissingDep` verdict is still promoted and the
    /// transitive caller's error is cleared.
    ///
    ///   dep.g  -> safe
    ///   mid.f  -> calls dep.g; UnsafeMissingDep while dep is missing
    ///   top    -> calls f() at module scope
    #[test]
    fn test_missing_dep_promotion_through_safe_callee() {
        use crate::test_lib::TestSources;

        let dep_cache = build_cache(&TestSources::new(&[("dep", "def g():\n    return 1\n")]));

        let mut own_cache = build_cache(&TestSources::new(&[
            ("mid", "from dep import g\ndef f():\n    g()\n"),
            ("top", "from mid import f\nf()\n"),
        ]));

        assert!(
            !own_cache
                .modules
                .iter()
                .find(|m| m.name == mn("top"))
                .unwrap()
                .is_safe(),
            "top unsafe before merge (mid is missing)"
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let top = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("top"))
            .unwrap();
        assert!(
            top.is_safe(),
            "top should be safe: f() only reaches the now-resolved safe g()"
        );
    }

    #[test]
    fn test_dotted_local_name_class_safety() {
        let mut fs = HashMap::new();
        fs.insert(
            "MyClass".to_string(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        let mut func_safety_by_module = HashMap::new();
        func_safety_by_module.insert(mn("dep"), fs);

        let resolved: AHashSet<ModuleName> = [mn("dep")].into_iter().collect();

        assert!(
            is_call_verified_safe("dep.MyClass.method", &resolved, &func_safety_by_module),
            "dep.MyClass.method should resolve via Class-level safety"
        );

        assert!(
            is_call_verified_safe("dep.MyClass", &resolved, &func_safety_by_module),
            "dep.MyClass should match directly"
        );

        assert!(
            !is_call_verified_safe("dep.OtherClass.method", &resolved, &func_safety_by_module),
            "dep.OtherClass.method should not match when OtherClass is not safe"
        );
    }
}
