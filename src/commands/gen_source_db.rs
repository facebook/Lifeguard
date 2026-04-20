/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use ruff_python_ast::Stmt;
use ruff_python_parser::parse_unchecked_source;
use serde::Deserialize;
use serde::Serialize;
use walkdir::WalkDir;

use crate::source_map::is_python_file;
use crate::source_map::is_valid_python_identifier;

#[derive(Parser)]
pub struct GenSourceDbArgs {
    /// Directory containing Python files to scan
    input_dir: PathBuf,

    /// Path to output JSON file
    output_path: PathBuf,

    /// Path to site-packages directory (overrides pyproject.toml setting)
    #[arg(long)]
    site_packages: Option<PathBuf>,
}

#[derive(Serialize)]
struct SourceDb {
    build_map: BTreeMap<String, String>,
}

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

/// Extract dotted module names from import statements in Python source.
fn extract_imports(source: &str) -> Vec<String> {
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
                // Skip relative imports (level > 0) — they refer to the project itself
                if import_from.level > 0 {
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

pub fn run(args: GenSourceDbArgs) -> Result<()> {
    let input_dir = args.input_dir.canonicalize()?;

    // Use CLI arg if provided, otherwise load from pyproject.toml
    let site_packages = match args.site_packages {
        Some(sp) => Some(sp.canonicalize().context("site_packages path not found")?),
        None => load_site_packages(&input_dir)?,
    };
    if let Some(ref sp) = site_packages {
        eprintln!("Using site-packages: {}", sp.display());
    }

    // Build search roots
    let mut roots: Vec<&Path> = vec![&input_dir];
    if let Some(ref sp) = site_packages {
        roots.push(sp.as_path());
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
    eprintln!(
        "Seeded with {} files from {}",
        seed_count,
        input_dir.display()
    );

    // Process the work queue: parse each file for imports, resolve them, add new files
    while let Some(file_path) = queue.pop_front() {
        let source = match std::fs::read_to_string(&file_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let imports = extract_imports(&source);
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

    let source_db = SourceDb { build_map };
    let output_file = std::fs::File::create(&args.output_path)?;
    let mut writer = BufWriter::new(output_file);
    serde_json::to_writer_pretty(&mut writer, &source_db)?;
    writer.flush()?;

    eprintln!(
        "Wrote {} entries ({} from imports) to {}",
        source_db.build_map.len(),
        source_db.build_map.len() - seed_count,
        args.output_path.display()
    );

    Ok(())
}
