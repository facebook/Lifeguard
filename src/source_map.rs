/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;

use ahash::AHashSet;
use anyhow::Result;
use anyhow::anyhow;
// Re-exported because ModuleName is part of the public SourceMap type
pub use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_python_ast::PySourceType;
use serde::Deserialize;
use tracing::warn;

use crate::debug::report_memory;
use crate::module_parser::ParsedModule;
use crate::module_parser::parse_pyi;
use crate::module_parser::read_and_parse_source;
use crate::stubs::Stubs;
use crate::tracing::time;

/// Lightweight per-module metadata without a parsed AST.
/// Used by the on-demand parsing pipeline to avoid holding all ASTs in memory.
struct SourceInfo {
    pub name: ModuleName,
    pub source_type: PySourceType,
    pub is_init: bool,
    /// File path relative to root_dir. None for in-memory stubs.
    pub path: Option<PathBuf>,
}

type SourceInfoMap = HashMap<ModuleName, SourceInfo, ahash::RandomState>;

pub enum SourceResult {
    Ok(PathBuf),
    SourceError(anyhow::Error),
}

impl SourceResult {
    /// Returns a reference to the PathBuf if this is Ok, otherwise None.
    pub fn as_path(&self) -> Option<&PathBuf> {
        match self {
            SourceResult::Ok(path) => Some(path),
            _ => None,
        }
    }
}
pub enum AstResult {
    Ok(ParsedModule),
    ParserError(anyhow::Error),
}

impl AstResult {
    /// Returns a reference to the ParsedModule if this is Ok, otherwise an error.
    pub fn as_parsed(&self) -> Result<&ParsedModule> {
        match self {
            AstResult::Ok(parsed) => Ok(parsed),
            AstResult::ParserError(e) => Err(anyhow!("Parser error: {}", e)),
        }
    }
}

// Type aliases
pub type SourceMap = HashMap<ModuleName, SourceResult, ahash::RandomState>;

// Raw deserialized source DB (string paths) before module name resolution.
pub(crate) type RawSourceMap = HashMap<String, PathBuf, ahash::RandomState>;

/// Typed envelope for the BXL source DB format.
#[derive(Deserialize)]
struct BxlSourceDb {
    build_map: RawSourceMap,
}

/// Typed envelope for the dbg-source-db format.
#[derive(Deserialize)]
struct DbgSourceDb {
    sources: RawSourceMap,
    dependencies: RawSourceMap,
}

// TODO: We are not including pyi files from the source db for now; we will consider external stubs
// once we get the internal stubs fully working.
static PYTHON_EXTENSIONS: LazyLock<AHashSet<&'static OsStr>> =
    LazyLock::new(|| AHashSet::from([OsStr::new("py")]));

/// Check if a path has a python file extension
pub fn is_python_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| PYTHON_EXTENSIONS.contains(ext))
}

/// Returns true if `name` is a valid Python identifier (ASCII subset),
/// i.e. it can appear as a component of a dotted module name.
pub fn is_valid_python_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        None => false,
        Some(c) if !c.is_ascii_alphabetic() && c != '_' => false,
        _ => chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
    }
}

pub fn load_source_map<P: AsRef<Path>>(db_path: P) -> Result<SourceMap> {
    let raw = time("  Parsing source map", || parse_source_map(db_path))?;
    Ok(time("  Resolving source map", || resolve_source_map(raw)))
}

/// Parse a source DB JSON file. Supports three formats:
/// - dbg-source-db: `{"sources": {...}, "dependencies": {...}}`
/// - BXL: `{"build_map": {...}}`
/// - flat: `{"path": "path", ...}` (from source-db-no-deps)
///
/// Deserializes directly into typed structs to avoid an intermediate
/// serde_json::Value tree, saving allocation and conversion overhead.
fn parse_source_map<P: AsRef<Path>>(db_path: P) -> Result<RawSourceMap> {
    let content = std::fs::read_to_string(db_path)?;

    // Detect format from the beginning of the file to pick the right
    // deserialization path without parsing the full JSON first.
    let prefix = content.get(..200).unwrap_or(&content);
    if prefix.contains("\"build_map\"") {
        let db: BxlSourceDb = serde_json::from_str(&content)?;
        Ok(db.build_map)
    } else if prefix.contains("\"sources\"") {
        // dbg-source-db format: sources win over dependencies on duplicate keys
        let db: DbgSourceDb = serde_json::from_str(&content)?;
        let mut deps = db.dependencies;
        deps.extend(db.sources);
        Ok(deps)
    } else {
        // Flat format (source-db-no-deps)
        Ok(serde_json::from_str(&content)?)
    }
}

/// Filters non-Python files, converts paths to module names, and resolves
/// priority conflicts (e.g. __init__.py vs regular .py).
///
/// Phase 1 uses rayon to parallelize per-entry filtering and name conversion.
/// Phase 2 sequentially merges results to resolve priority conflicts.
pub(crate) fn resolve_source_map(raw: RawSourceMap) -> SourceMap {
    let entries: Vec<(ModuleName, u8, PathBuf)> = raw
        .into_par_iter()
        .filter_map(|(module_path, full_path)| {
            if !is_python_file(&full_path) {
                return None;
            }
            let mod_name = match ModuleName::from_relative_path(module_path.as_ref()) {
                Ok(name) => name,
                Err(e) => {
                    warn!(
                        "Failed to convert path to module name '{}' (file '{}'): {}",
                        module_path,
                        full_path.display(),
                        e
                    );
                    return None;
                }
            };
            // TODO(T257095571): We need to surface the error where the path does not convert to a valid file.
            let priority = match source_priority(&full_path) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Skipping file with invalid path: {:?}: {}", full_path, e);
                    return None;
                }
            };
            Some((mod_name, priority, full_path))
        })
        .collect();

    let mut result = SourceMap::default();
    let mut priorities: HashMap<ModuleName, u8, ahash::RandomState> = HashMap::default();
    for (mod_name, priority, full_path) in entries {
        let dominated = priorities.get(&mod_name).is_some_and(|&p| p <= priority);
        if !dominated {
            priorities.insert(mod_name, priority);
            result.insert(mod_name, SourceResult::Ok(full_path));
        }
    }
    result
}

/// Returns priority value for Python extensions (lower number = higher priority).
/// Returns Err for invalid unicode paths or unrecognized extensions.
fn source_priority(path: &Path) -> anyhow::Result<u8> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow!("Path contains invalid UTF-8: {:?}", path))?;
    if s.ends_with("__init__.pyi") {
        Ok(0)
    } else if s.ends_with(".pyi") {
        Ok(1)
    } else if s.ends_with("__init__.py") {
        Ok(2)
    } else if s.ends_with(".py") {
        Ok(3)
    } else {
        Err(anyhow!("Unrecognized file extension for path: {:?}", path))
    }
}

/// Build a lightweight SourceInfoMap from the SourceMap and Stubs.
/// Contains only metadata (name, source_type, is_init, path) — no parsed ASTs.
fn make_source_info_map(source_map: &SourceMap, stubs: &Stubs) -> SourceInfoMap {
    let mut info_map = SourceInfoMap::default();

    // Add entries from the source map (real .py files)
    for (name, source_result) in source_map {
        if let Some(path) = source_result.as_path() {
            let is_init = path.file_name().is_some_and(|f| f == "__init__.py");
            let source_type = PySourceType::Python;
            info_map.insert(
                *name,
                SourceInfo {
                    name: *name,
                    source_type,
                    is_init,
                    path: Some(path.clone()),
                },
            );
        }
    }

    // Add entries from stubs
    for (mod_name, _) in stubs.raw_sources_iter() {
        info_map.insert(
            *mod_name,
            SourceInfo {
                name: *mod_name,
                source_type: PySourceType::Stub,
                is_init: false,
                path: None,
            },
        );
    }

    info_map
}

/// Parse a single module on demand from its SourceInfo.
/// For file-backed modules, reads and parses the file at root_dir/path.
/// For stub modules (path is None), parses the stub source from Stubs.
fn parse_module(info: &SourceInfo, root_dir: &Path, stubs: &Stubs) -> Option<AstResult> {
    match &info.path {
        Some(path) => {
            let full_path = root_dir.join(path);
            let result = match read_and_parse_source(&full_path, info.name, info.is_init) {
                Ok(parsed) => AstResult::Ok(parsed),
                Err(e) => AstResult::ParserError(e),
            };
            Some(result)
        }
        None => {
            // Stub module: look up raw source from stubs
            let src = stubs.get_raw_source(&info.name)?;
            Some(AstResult::Ok(parse_pyi(src, info.name, info.is_init)))
        }
    }
}

/// Abstraction over "a source of parseable modules."
/// Both production (disk-backed) and test (in-memory) pipelines implement this trait,
/// so the same analysis functions work for both.
pub trait ModuleProvider: Sync {
    fn module_names_iter(&self) -> impl Iterator<Item = &ModuleName>;
    fn module_names_par_iter(&self) -> impl ParallelIterator<Item = &ModuleName>;
    fn module_names(&self) -> Vec<ModuleName> {
        self.module_names_iter().copied().collect()
    }
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn parse(&self, name: &ModuleName) -> Option<AstResult>;
    fn is_stub(&self, name: &ModuleName) -> bool;
    fn stubs(&self) -> &Stubs;
}

/// Production implementation of ModuleProvider: wraps SourceInfoMap + root_dir + Stubs.
/// Parses modules on demand from disk.
pub struct Sources {
    info_map: SourceInfoMap,
    root_dir: PathBuf,
    stubs: Stubs,
}

impl Sources {
    pub fn new(source_map: &SourceMap, root_dir: PathBuf) -> Self {
        let stubs = Stubs::new();
        let info_map = time("Building source info map", || {
            make_source_info_map(source_map, &stubs)
        });
        report_memory("After building source info map");
        Self {
            info_map,
            root_dir,
            stubs,
        }
    }
}

impl ModuleProvider for Sources {
    fn module_names_iter(&self) -> impl Iterator<Item = &ModuleName> {
        self.info_map.keys()
    }

    fn module_names_par_iter(&self) -> impl ParallelIterator<Item = &ModuleName> {
        self.info_map.par_iter().map(|(k, _)| k)
    }

    fn len(&self) -> usize {
        self.info_map.len()
    }

    fn parse(&self, name: &ModuleName) -> Option<AstResult> {
        let info = self.info_map.get(name)?;
        parse_module(info, &self.root_dir, &self.stubs)
    }

    fn is_stub(&self, name: &ModuleName) -> bool {
        self.info_map
            .get(name)
            .is_some_and(|info| matches!(info.source_type, PySourceType::Stub))
    }

    fn stubs(&self) -> &Stubs {
        &self.stubs
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn resolve_case_helper(
        entries: Vec<(&str, &str)>,
        expected_count: usize,
        expected_modules: &[&str],
    ) {
        let mut raw = RawSourceMap::default();
        for (key, path) in entries {
            raw.insert(key.to_string(), PathBuf::from(path));
        }

        let result = resolve_source_map(raw);
        assert_eq!(result.len(), expected_count);
        for expected in expected_modules {
            let mod_name = ModuleName::from_str(expected);
            assert!(
                result.contains_key(&mod_name),
                "Expected module '{}' not found",
                expected
            );
        }
    }

    fn resolve_case_helper_with_paths(entries: Vec<(&str, &str)>, expected: Vec<(&str, &str)>) {
        let mut raw = RawSourceMap::default();
        for (key, path) in entries {
            raw.insert(key.to_string(), PathBuf::from(path));
        }

        let result = resolve_source_map(raw);
        assert_eq!(result.len(), expected.len());
        for (mod_str, expected_path) in expected {
            let mod_name = ModuleName::from_str(mod_str);
            let actual_path = result
                .get(&mod_name)
                .and_then(|sr| sr.as_path())
                .unwrap_or_else(|| panic!("Module '{}' not found", mod_str));
            assert_eq!(
                actual_path,
                &PathBuf::from(expected_path),
                "Module '{}' should map to '{}'",
                mod_str,
                expected_path
            );
        }
    }

    #[test]
    fn test_filter_keeps_py_files() {
        resolve_case_helper(
            vec![
                ("module1.py", "src/module1.py"),
                ("module2.pyx", "src/module2.pyx"),
                ("module3.pyi", "src/module3.pyi"),
            ],
            1,
            &["module1"],
        );
    }

    #[test]
    fn test_filter_removes_other_extensions() {
        resolve_case_helper(
            vec![
                ("module1.py", "src/module1.py"),
                ("module2.pyx", "src/module2.pyx"),
                ("module3.foo", "src/module3.foo"),
            ],
            1,
            &["module1"],
        );
    }

    #[test]
    fn test_filter_removes_no_extension() {
        let mut raw = RawSourceMap::default();
        raw.insert("module1".to_string(), PathBuf::from("src/module1.py"));
        raw.insert("module2".to_string(), PathBuf::from("module2"));

        let result = resolve_source_map(raw);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_resolve_source_priority_single_py_file() {
        resolve_case_helper(vec![("module1.py", "src/module1.py")], 1, &["module1"]);
    }

    #[test]
    fn test_resolve_source_priority_no_conflicts() {
        resolve_case_helper(
            vec![
                ("unique1.py", "src/unique1.py"),
                ("unique2.py", "src/unique2.py"),
            ],
            2,
            &["unique1", "unique2"],
        );
    }

    #[test]
    fn test_resolve_source_priority_no_conflicts_same_file_name() {
        resolve_case_helper(
            vec![
                ("foo/unique.py", "foo/src/unique.py"),
                ("bar/unique.py", "bar/src/unique.py"),
            ],
            2,
            &["foo.unique", "bar.unique"],
        );
    }

    #[test]
    fn test_resolve_source_priority_init_file() {
        resolve_case_helper_with_paths(
            vec![
                ("foo/bar.py", "foo/bar.py"),
                ("foo/bar/__init__.py", "foo/bar/__init__.py"),
            ],
            vec![("foo.bar", "foo/bar/__init__.py")],
        );
    }

    #[test]
    fn test_source_priority_values() {
        assert_eq!(source_priority(Path::new("pkg/__init__.pyi")).ok(), Some(0));
        assert_eq!(source_priority(Path::new("pkg/__init__.py")).ok(), Some(2));
        assert_eq!(source_priority(Path::new("module.pyi")).ok(), Some(1));
        assert_eq!(source_priority(Path::new("module.py")).ok(), Some(3));
        assert!(source_priority(Path::new("module.pyx")).is_err());
        assert!(source_priority(Path::new("module.rs")).is_err());
        assert!(source_priority(Path::new("module.txt")).is_err());
        assert!(source_priority(Path::new("no_extension")).is_err());
    }
}
