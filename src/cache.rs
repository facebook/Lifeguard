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

use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::exports::ExportType;
use crate::exports::Exports;
use crate::imports::ImportGraph;
use crate::module_safety::FunctionSafety;
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
    /// Module-level imports never accessed in any scope (side-effect imports).
    pub side_effect_imports: AHashSet<ModuleName>,
    /// Per-function safety verdicts from call graph analysis.
    /// Keys are function-local names (e.g., "helper" for `mod.helper`).
    pub function_safety: HashMap<String, FunctionSafety>,
}

/// Safety analysis result for a cached module.
#[derive(Serialize, Deserialize)]
pub enum CachedSafety {
    Ok(CachedModuleSafety),
    AnalysisError { message: String },
}

/// Detailed safety information for a module.
#[derive(Serialize, Deserialize)]
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
        let mut i = 0;
        while i + 1 < self.modules.len() {
            if self.modules[i].name == self.modules[i + 1].name {
                let other = self.modules.remove(i + 1);
                self.modules[i].merge(other);
            } else {
                i += 1;
            }
        }
    }

    /// Resolve missing imports against the merged cache and selectively clear
    /// false errors using per-function safety verdicts.
    ///
    /// Each missing import is resolved independently. For each Unknown* error,
    /// we consult the resolved module's cached function_safety to determine
    /// whether the called function is actually safe. Only errors where the
    /// function is verified safe are cleared.
    pub fn resolve_cross_library_errors(&mut self) {
        let module_names: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();

        let func_safety_by_module: HashMap<ModuleName, HashMap<String, FunctionSafety>> = self
            .modules
            .iter()
            .map(|m| (m.name, m.function_safety.clone()))
            .collect();

        for module in &mut self.modules {
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                resolve_implicit_imports(&mut safety.implicit_imports, &module_names);
            }

            if module.missing_imports.is_empty() {
                continue;
            }

            let mut still_missing: AHashSet<ModuleName> = AHashSet::new();
            let mut resolved_modules: AHashSet<ModuleName> = AHashSet::new();

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
                safety.errors.retain(|e| {
                    if !e.kind.could_be_caused_by_missing_import() {
                        return true;
                    }
                    let func_name = e.metadata.trim_end_matches("()");
                    !is_call_verified_safe(func_name, &resolved_modules, &func_safety_by_module)
                });
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
    func_safety_by_module: &HashMap<ModuleName, HashMap<String, FunctionSafety>>,
) -> bool {
    let fqn = ModuleName::from_str(func_name);
    for (parent, dot_pos) in fqn.iter_parents() {
        if resolved_modules.contains(&parent) {
            let local_name = &func_name[dot_pos + 1..];
            if let Some(fs) = func_safety_by_module.get(&parent) {
                return matches!(fs.get(local_name), Some(FunctionSafety::Safe));
            }
            return false;
        }
    }

    matches!(
        resolved_modules
            .iter()
            .filter_map(|r| func_safety_by_module.get(r))
            .filter_map(|fs| fs.get(func_name))
            .try_fold(false, |_, s| {
                if *s == FunctionSafety::Safe {
                    Ok(true)
                } else {
                    Err(())
                }
            }),
        Ok(true)
    )
}

fn resolve_to_known_module(name: &ModuleName, known: &AHashSet<ModuleName>) -> Option<ModuleName> {
    if known.contains(name) {
        return Some(*name);
    }
    name.iter_parents()
        .find(|(p, _)| known.contains(p))
        .map(|(p, _)| p)
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
    pub fn is_safe(&self) -> bool {
        matches!(&self.safety, CachedSafety::Ok(s) if s.is_safe())
    }

    /// Merge another CachedModule (same name) into this one.
    fn merge(&mut self, other: CachedModule) {
        self.imports.extend(other.imports);
        self.missing_imports
            .retain(|m| other.missing_imports.contains(m));
        self.side_effect_imports.extend(other.side_effect_imports);
        self.safety.merge(other.safety);
        for (name, safety) in other.function_safety {
            self.function_safety
                .entry(name)
                .and_modify(|existing| {
                    *existing = (*existing).max(safety);
                })
                .or_insert(safety);
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
        use crate::imports::ImportGraph;
        use crate::project;
        use crate::traits::SysInfoExt;

        let sys_info = crate::pyrefly::sys_info::SysInfo::lg_default();
        let (import_graph, exports) = ImportGraph::make_with_exports(sources, &sys_info);
        let output = project::run_analysis(sources, &exports, &import_graph, &sys_info);
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
        assert_eq!(std::mem::size_of::<CachedModule>(), 320);
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
            defs_mod.function_safety.get("Safe"),
            Some(&FunctionSafety::Safe),
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
            defs_mod.function_safety.get("Foo"),
            Some(&FunctionSafety::Safe),
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
}
