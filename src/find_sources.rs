/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Discover Python source files under a directory tree by seeding with every
//! `.py` file whose path components are valid Python identifiers and then
//! following imports transitively (optionally into a site-packages directory).
//!
//! Shared by the `gen-source-db` and `run-tree` subcommands.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use ruff_python_ast::Stmt;
use ruff_python_parser::parse_unchecked_source;
use serde::Deserialize;
use walkdir::WalkDir;

use crate::source_map::RawSourceMap;
use crate::source_map::SourceMap;
use crate::source_map::is_python_file;
use crate::source_map::is_valid_python_identifier;
use crate::source_map::resolve_source_map;

#[derive(Deserialize)]
struct PyprojectToml {
    lifeguard: Option<LifeguardConfig>,
}

#[derive(Deserialize)]
struct LifeguardConfig {
    site_packages: Option<String>,
}

/// Try to resolve a dotted module name to a .py file under the given root.
/// Returns the first match found, checking:
///   root/a/b/c.py
///   root/a/b/c/__init__.py
fn resolve_module(root: &Path, parts: &[&str]) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in parts {
        path.push(part);
    }

    // Try as a .py file
    let mut py_path = path.clone();
    py_path.set_extension("py");
    if py_path.is_file() {
        return Some(py_path);
    }

    // Try as a package (__init__.py)
    let init_path = path.join("__init__.py");
    if init_path.is_file() {
        return Some(init_path);
    }

    None
}

/// Try to resolve a dotted module name against multiple roots.
/// Also tries progressively shorter prefixes (for `from foo.bar import baz`
/// where baz is a name inside foo/bar.py, not a submodule).
fn resolve_import(roots: &[&Path], module: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = module.split('.').collect();

    // Try full path first, then progressively shorter prefixes
    for len in (1..=parts.len()).rev() {
        let prefix = &parts[..len];
        for root in roots {
            if let Some(path) = resolve_module(root, prefix) {
                return Some(path);
            }
        }
    }

    None
}

/// Compute the package name for a Python file given its path relative to a root.
/// The package is always the parent directory, dotted
/// (e.g. `sqlalchemy/sql/__init__.py` → `sqlalchemy.sql`,
/// `sqlalchemy/sql/schema.py` → `sqlalchemy.sql`).
fn package_from_rel_path(rel_path: &Path) -> Option<String> {
    let parts: Vec<&str> = rel_path
        .parent()?
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

/// Resolve a relative import to an absolute module name.
/// `level` is the number of leading dots, `module` is the part after the dots (if any),
/// and `package` is the current file's package.
fn resolve_relative_import(level: u32, module: Option<&str>, package: &str) -> Option<String> {
    let parts: Vec<&str> = package.split('.').collect();
    // level=1 means current package, level=2 means parent, etc.
    let keep = parts.len().checked_sub(level.saturating_sub(1) as usize)?;
    if keep == 0 {
        return module.map(|m| m.to_string());
    }
    let base = parts[..keep].join(".");
    match module {
        Some(m) => Some(format!("{}.{}", base, m)),
        None => Some(base),
    }
}

/// Extract dotted module names from import statements in Python source.
/// If `package` is provided, relative imports are resolved against it.
fn extract_imports(source: &str, package: Option<&str>) -> Vec<String> {
    let parsed = parse_unchecked_source(source, ruff_python_ast::PySourceType::Python);
    let mut imports = Vec::new();

    for stmt in parsed.suite() {
        match stmt {
            Stmt::Import(import) => {
                for alias in &import.names {
                    imports.push(alias.name.to_string());
                }
            }
            Stmt::ImportFrom(import_from) => {
                if import_from.level > 0 {
                    // Resolve relative import if we have a package context
                    let Some(pkg) = package else {
                        continue;
                    };
                    let base = resolve_relative_import(
                        import_from.level,
                        import_from.module.as_ref().map(|m| m.as_str()),
                        pkg,
                    );
                    let Some(base) = base else {
                        continue;
                    };
                    for alias in &import_from.names {
                        imports.push(format!("{}.{}", base, alias.name));
                    }
                    imports.push(base);
                    continue;
                }
                if let Some(module) = &import_from.module {
                    let module_str = module.to_string();
                    // Also check if any imported name is itself a submodule
                    // e.g. `from foo import bar` where foo/bar.py exists
                    for alias in &import_from.names {
                        imports.push(format!("{}.{}", module_str, alias.name));
                    }
                    imports.push(module_str);
                }
            }
            _ => {}
        }
    }

    imports
}

fn load_site_packages(input_dir: &Path) -> Result<Option<PathBuf>> {
    let pyproject_path = input_dir.join("pyproject.toml");
    if !pyproject_path.is_file() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&pyproject_path)
        .with_context(|| format!("Failed to read {}", pyproject_path.display()))?;
    let pyproject: PyprojectToml = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", pyproject_path.display()))?;

    let sp_str = match pyproject.lifeguard.and_then(|l| l.site_packages) {
        Some(s) => s,
        None => return Ok(None),
    };

    let sp_path = Path::new(&sp_str);
    let sp_path = if sp_path.is_absolute() {
        sp_path.to_path_buf()
    } else {
        input_dir.join(sp_path)
    };

    let sp_path = sp_path
        .canonicalize()
        .with_context(|| format!("site_packages path not found: {}", sp_path.display()))?;

    Ok(Some(sp_path))
}

/// Build the source-DB `build_map` for `input_dir` by seeding with every
/// `.py` file under it (whose path components are valid Python identifiers)
/// and then following imports transitively. If `site_packages_override` is
/// Some, it takes precedence over the `[lifeguard].site_packages` entry in
/// `<input_dir>/pyproject.toml`. Imports that resolve under site-packages
/// are included as well. Returns the map (rel-path → abs-path) and the
/// number of seed files (rest came from import-following).
pub fn build_source_db(
    input_dir: &Path,
    site_packages_override: Option<&Path>,
) -> Result<(BTreeMap<String, String>, usize)> {
    let input_dir = input_dir.canonicalize()?;

    // CLI override wins over pyproject.toml
    let site_packages = match site_packages_override {
        Some(sp) => Some(sp.canonicalize().context("site_packages path not found")?),
        None => load_site_packages(&input_dir)?,
    };
    if let Some(ref sp) = site_packages {
        eprintln!("Using site-packages: {}", sp.display());
    }

    // Build search roots
    let mut roots: Vec<&Path> = vec![&input_dir];
    if let Some(ref sp) = site_packages {
        roots.push(sp);
    }

    let mut build_map = BTreeMap::new();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    // Seed the queue with all .py files under input_dir, skipping directories
    // and files whose names are not valid Python identifiers.
    for entry in WalkDir::new(&input_dir)
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return true;
            }
            e.depth() == 0
                || e.file_name()
                    .to_str()
                    .is_some_and(is_valid_python_identifier)
        })
        .filter_map(|e| e.ok())
        .filter(|e| {
            is_python_file(e.path())
                && e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(is_valid_python_identifier)
        })
    {
        if let Ok(full_path) = entry.path().canonicalize() {
            if visited.insert(full_path.clone()) {
                let rel_path = full_path
                    .strip_prefix(&input_dir)
                    .context("file resolved to a path outside of input_dir")?;
                build_map.insert(
                    rel_path.to_string_lossy().into_owned(),
                    full_path.to_string_lossy().into_owned(),
                );
                queue.push_back(full_path);
            }
        }
    }

    let seed_count = build_map.len();

    // Process the work queue: parse each file for imports, resolve them, add new files
    while let Some(file_path) = queue.pop_front() {
        let source = match std::fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // For site_packages files, compute the package so relative imports
        // can be resolved. Project files don't need this: the initial walk
        // already seeds every .py under input_dir, so following their relative
        // imports wouldn't discover anything new.
        let package = site_packages
            .as_ref()
            .and_then(|sp| file_path.strip_prefix(sp).ok())
            .and_then(package_from_rel_path);
        let imports = extract_imports(&source, package.as_deref());
        for module_name in imports {
            if let Some(resolved) = resolve_import(&roots, &module_name) {
                let resolved = match resolved.canonicalize() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if visited.insert(resolved.clone()) {
                    // Determine the relative key based on which root it's under.
                    // Check most-specific root first (site_packages may be a
                    // subdirectory of input_dir).
                    let rel_key = roots
                        .iter()
                        .rev()
                        .find_map(|root| resolved.strip_prefix(root).ok());
                    let Some(rel_key) = rel_key else {
                        continue;
                    };
                    let rel_key = rel_key.to_string_lossy().into_owned();

                    build_map.insert(rel_key, resolved.to_string_lossy().into_owned());
                    queue.push_back(resolved);
                }
            }
        }
    }

    Ok((build_map, seed_count))
}

/// Convert a `build_map` (as produced by [`build_source_db`]) into a
/// [`SourceMap`], applying the standard priority resolution (`.pyi` >
/// `.py`, `__init__` > regular) on duplicates.
pub fn make_source_map(build_map: BTreeMap<String, String>) -> SourceMap {
    let raw: RawSourceMap = build_map
        .into_iter()
        .map(|(k, v)| (k, PathBuf::from(v)))
        .collect();
    resolve_source_map(raw)
}
