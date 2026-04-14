/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::exports::ExportType;
use crate::exports::Exports;
use crate::imports::ImportGraph;
use crate::module_safety::SafetyResult;
use crate::project::SafetyMap;
use crate::project::SideEffectMap;

const CACHE_VERSION: u32 = 1;

/// Cached analysis results for a single Python library.
/// Contains all information needed to merge with other libraries
/// in a map-reduce analysis pipeline.
#[derive(Serialize, Deserialize)]
pub struct LibraryCache {
    pub version: u32,
    pub modules: Vec<CachedModule>,
    pub exports: CachedExports,
}

/// Cached analysis for a single module within a library.
#[derive(Serialize, Deserialize)]
pub struct CachedModule {
    pub name: ModuleName,
    pub safety: CachedSafety,
    /// Resolved imports (edges in the import graph).
    pub imports: Vec<ModuleName>,
    /// Imports that could not be resolved to modules in the source DB.
    pub missing_imports: Vec<ModuleName>,
    /// Module-level imports never accessed in any scope (side-effect imports).
    pub side_effect_imports: Vec<ModuleName>,
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
#[derive(Serialize, Deserialize)]
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

                let mut imports: Vec<ModuleName> =
                    import_graph.get_imports(&name).cloned().collect();
                imports.sort();

                let mut missing_imports: Vec<ModuleName> = import_graph
                    .get_missing_imports(&name)
                    .map(|m| m.iter().cloned().collect())
                    .unwrap_or_default();
                missing_imports.sort();

                let mut se_imports: Vec<ModuleName> = side_effect_imports
                    .get(&name)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default();
                se_imports.sort();

                let safety = CachedSafety::from_safety_result(safety_result);

                CachedModule {
                    name,
                    safety,
                    imports,
                    missing_imports,
                    side_effect_imports: se_imports,
                }
            })
            .collect();

        modules.sort_by_key(|m| m.name);

        let cached_exports = CachedExports::from_exports(exports);

        LibraryCache {
            version: CACHE_VERSION,
            modules,
            exports: cached_exports,
        }
    }

    /// Write the cache to a JSON file.
    pub fn write_to_file(&self, path: &Path) -> anyhow::Result<()> {
        let file = std::fs::File::create(path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer(writer, self)?;
        Ok(())
    }

    /// Read a cache from a JSON file.
    pub fn read_from_file(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let cache: Self = serde_json::from_reader(reader)?;
        if cache.version != CACHE_VERSION {
            anyhow::bail!(
                "Cache version mismatch: expected {}, got {}",
                CACHE_VERSION,
                cache.version
            );
        }
        Ok(cache)
    }
}

impl CachedModule {
    pub fn is_safe(&self) -> bool {
        matches!(&self.safety, CachedSafety::Ok(s) if s.is_safe())
    }
}

impl CachedSafety {
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

impl CachedError {
    fn from_safety_error(error: &SafetyError) -> Self {
        CachedError {
            kind: error.kind,
            metadata: error.metadata.as_str().to_string(),
        }
    }
}

impl CachedExports {
    fn from_exports(exports: &Exports) -> Self {
        let mut definitions: Vec<(ModuleName, ExportType)> = exports
            .get_exports()
            .map(|(name, export)| (*name, export.typ))
            .collect();
        definitions.sort_by_key(|(name, _)| *name);

        let mut re_exports: Vec<CachedReExport> = exports
            .get_re_exports()
            .map(|(exported, (imported, _range))| CachedReExport {
                exported_module: exported.module,
                exported_attr: exported.attr.to_string(),
                imported_module: imported.module,
                imported_attr: imported.attr.to_string(),
            })
            .collect();
        re_exports.sort_by_key(|a| (a.exported_module, a.exported_attr.clone()));

        let mut all: Vec<(ModuleName, Vec<String>)> = exports
            .iter_all()
            .map(|(name, names)| (*name, names.iter().map(|n| n.to_string()).collect()))
            .collect();
        all.sort_by_key(|(name, _)| *name);

        let mut return_types: Vec<(ModuleName, ModuleName)> =
            exports.iter_return_types().map(|(k, v)| (*k, *v)).collect();
        return_types.sort_by_key(|(k, _)| *k);

        CachedExports {
            definitions,
            re_exports,
            all,
            return_types,
        }
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
        let json = serde_json::to_string(&cache).unwrap();
        let loaded: LibraryCache = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.version, CACHE_VERSION);
        assert_eq!(loaded.modules.len(), 2);

        let foo = loaded.modules.iter().find(|m| m.name == mn("foo")).unwrap();
        assert!(matches!(&foo.safety, CachedSafety::Ok(s) if s.is_safe()));
        assert_eq!(foo.imports, vec![mn("bar")]);

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
        let json = serde_json::to_string(&cache).unwrap();
        let loaded: LibraryCache = serde_json::from_str(&json).unwrap();

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

        let bytes = serde_json::to_vec(&cache).unwrap();
        let loaded: LibraryCache = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(loaded.version, CACHE_VERSION);
        assert_eq!(loaded.modules.len(), 1);
        assert_eq!(loaded.modules[0].name, mn("test"));
    }

    #[test]
    fn test_cache_from_pipeline() {
        use crate::imports::ImportGraph;
        use crate::project;
        use crate::test_lib::TestSources;
        use crate::traits::SysInfoExt;

        let sources = TestSources::new(&[
            ("foo", "import bar\nx = bar.func()\n"),
            ("bar", "def func(): return 1\n"),
        ]);
        let sys_info = crate::pyrefly::sys_info::SysInfo::lg_default();
        let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &sys_info);
        let (safety_map, side_effect_imports, _parse_errors) =
            project::run_analysis(&sources, &exports, &import_graph, &sys_info);

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);

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

        // Round-trip through JSON
        let json = serde_json::to_string(&cache).unwrap();
        let loaded: LibraryCache = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.modules.len(), 2);
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
        let json = serde_json::to_string(&cache).unwrap();
        let loaded: LibraryCache = serde_json::from_str(&json).unwrap();

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
        let json = serde_json::to_string(&cache).unwrap();
        let loaded: LibraryCache = serde_json::from_str(&json).unwrap();

        let a = loaded.modules.iter().find(|m| m.name == mn("a")).unwrap();
        assert_eq!(a.side_effect_imports, vec![mn("unused_dep")]);
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
}
