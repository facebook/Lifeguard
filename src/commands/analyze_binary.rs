/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::ArgAction;
use clap::Parser;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use tracing::info;

use crate::cache::CachedSafety;
use crate::cache::LibraryCache;
use crate::cache::ReduceWorkspace;
use crate::cache::ResolvedCache;
use crate::commands::write_json;
use crate::hasher::AHashSet;
use crate::output::LifeGuardAnalysis;
use crate::runner::DEFAULT_PYTHON_VERSION;
use crate::runner::Options;
use crate::runner::parse_python_version;
use crate::tracing::ProcessTimer;
use crate::tracing::time;

/// How many list entries `--explain` prints before truncating.
const EXPLAIN_LIMIT: usize = 20;

#[derive(Parser)]
pub struct AnalyzeBinaryArgs {
    /// Path to output file
    pub output_path: PathBuf,

    /// Path to a manifest file listing cache paths (one per line).
    #[arg(long = "cache-manifest")]
    pub cache_manifest: PathBuf,

    /// Sort output keys and values for deterministic results
    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    pub sorted_output: bool,

    /// Name of the main module (the module run as __main__).
    ///
    /// Facts a `__main__` guard produced are kept for this module and dropped
    /// for every other, which is the only point in the pipeline that knows the
    /// answer: a library map action does not know which binary it serves.
    #[arg(long = "main-module")]
    pub main_module: Option<String>,

    /// Python version to use for parsing
    #[arg(long = "python-version", default_value = DEFAULT_PYTHON_VERSION)]
    pub python_version: String,

    /// Report what the reduce concluded about one module, to stdout.
    #[arg(long = "explain")]
    pub explain: Option<String>,
}

pub fn run(args: AnalyzeBinaryArgs) -> Result<()> {
    let timer = ProcessTimer::new();

    let mut cache_paths: Vec<PathBuf> = std::fs::read_to_string(&args.cache_manifest)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    let mut seen = AHashSet::default();
    cache_paths.retain(|path| seen.insert(path.clone()));

    anyhow::ensure!(!cache_paths.is_empty(), "no cache paths provided");

    let caches: Vec<LibraryCache> = time("Loading caches", || {
        cache_paths
            .par_iter()
            .map(|p| {
                info!("Loading cache from {}", p.display());
                LibraryCache::read_from_file(p)
            })
            .collect::<Result<Vec<_>>>()
    })?;

    let python_version = parse_python_version(&args.python_version)?;
    let workspace = time("Preparing reduce workspace", || {
        ReduceWorkspace::merge(caches, python_version)
    })?;
    info!(
        "Prepared cache: {} artifact modules, {} total modules including bundled stubs",
        workspace.artifact_module_count(),
        workspace.module_count()
    );
    let main_module = args.main_module.as_deref().map(ModuleName::from_str);
    workspace.warn_unknown_main_module(main_module);
    let resolved = time("Resolving cross-library facts", || {
        workspace.resolve(main_module)
    });

    let options = Options {
        verbose_output_path: None,
        sorted_output: args.sorted_output,
        main_module,
        python_version,
    };

    let analysis = time("Building analysis from cache", || {
        LifeGuardAnalysis::from_resolved_cache(&resolved, &options)
    });

    info!("{}", time("Generating report", || analysis.get_report()));

    if let Some(target) = &args.explain {
        let mut stdout = io::stdout().lock();
        explain_module(
            &mut stdout,
            &resolved,
            &analysis,
            ModuleName::from_str(target),
        )?;
    }

    write_json(&args.output_path, &analysis.output)?;

    info!("Output written to {}", args.output_path.display());
    timer.report_finish();
    Ok(())
}

/// Print what the reduce concluded about `module`.
///
/// Fixes a diagnostic gap - the output JSON cannot distinguish a module the
/// reduce ruled unsafe from one it never saw.
fn explain_module<W: Write>(
    out: &mut W,
    resolved: &ResolvedCache,
    analysis: &LifeGuardAnalysis,
    module: ModuleName,
) -> io::Result<()> {
    writeln!(out, "--- explain: {} ---", module)?;

    let Some(cached) = resolved.find_module(module) else {
        writeln!(
            out,
            "  merged cache: ABSENT -- no library contributed this module"
        )?;
        writeln!(
            out,
            "  consumers therefore treat every import of it as unresolved"
        )?;
        return Ok(());
    };
    writeln!(out, "  merged cache: present")?;

    match &cached.safety {
        CachedSafety::AnalysisError { message } => {
            writeln!(out, "  map-phase analysis: FAILED ({})", message)?;
        }
        CachedSafety::Ok(safety) => {
            writeln!(out, "  map-phase errors: {}", safety.errors.len())?;
            for error in safety.errors.iter().take(EXPLAIN_LIMIT) {
                writeln!(out, "    {:?} {}", error.kind, error.metadata)?;
            }
            if safety.errors.len() > EXPLAIN_LIMIT {
                writeln!(out, "    ... {} more", safety.errors.len() - EXPLAIN_LIMIT)?;
            }
        }
    }

    writeln!(
        out,
        "  imports: {} resolved, {} missing, {} ambiguous",
        cached.imports.len(),
        cached.missing_imports.len(),
        cached.ambiguous_imports.len()
    )?;
    for (label, set) in [
        ("missing", &cached.missing_imports),
        ("ambiguous", &cached.ambiguous_imports),
    ] {
        for name in set.iter().take(EXPLAIN_LIMIT) {
            writeln!(out, "    {}: {}", label, name)?;
        }
    }

    let mut functions: Vec<&String> = cached.function_safety.keys().collect();
    functions.sort();
    writeln!(out, "  function_safety entries: {}", functions.len())?;
    for name in functions.iter().take(EXPLAIN_LIMIT) {
        writeln!(out, "    {}", name)?;
    }

    let exports = &resolved.resolved_cache().exports;
    let out_edges: Vec<_> = exports
        .re_exports
        .iter()
        .filter(|re| re.exported_module == module)
        .collect();
    let in_edges = exports
        .re_exports
        .iter()
        .filter(|re| re.imported_module == module)
        .count();
    writeln!(
        out,
        "  re-exports: {} it exports, {} that read from it",
        out_edges.len(),
        in_edges
    )?;
    for re in out_edges.iter().take(EXPLAIN_LIMIT) {
        writeln!(
            out,
            "    exports {} <- {}.{}",
            re.exported_attr, re.imported_module, re.imported_attr
        )?;
    }

    let eager = analysis.output.load_imports_eagerly.contains(&module);
    match analysis.output.lazy_eligible.get(&module) {
        Some(deps) => {
            writeln!(
                out,
                "  verdict: LAZY_ELIGIBLE ({} deps to load eagerly){}",
                deps.len(),
                if eager { " [LOAD_IMPORTS_EAGERLY]" } else { "" }
            )?;
            for dep in deps.iter().take(EXPLAIN_LIMIT) {
                writeln!(out, "    {}", dep)?;
            }
        }
        None => {
            writeln!(
                out,
                "  verdict: not lazy-eligible{}",
                if eager { " [LOAD_IMPORTS_EAGERLY]" } else { "" }
            )?;
        }
    }

    let consumers: Vec<ModuleName> = analysis
        .output
        .lazy_eligible
        .iter()
        .filter(|entry| entry.value().contains(&module))
        .map(|entry| *entry.key())
        .collect();
    writeln!(
        out,
        "  modules that must load it eagerly: {}",
        consumers.len()
    )?;
    for consumer in consumers.iter().take(EXPLAIN_LIMIT) {
        writeln!(out, "    {}", consumer)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::name::Name;

    use super::*;
    use crate::cache::CachedModule;
    use crate::cache::CachedReExport;
    use crate::cache::LibraryCache;
    use crate::cache::MergedClassFacts;
    use crate::hasher::AHashSet;
    use crate::hasher::HashSetExt;
    use crate::module_safety::FunctionSafety;
    use crate::module_safety::FunctionSafetyInfo;
    use crate::output::LifeGuardAnalysis;
    use crate::pyrefly::sys_info::PythonVersion;
    use crate::runner::Options;
    use crate::test_lib::reduce_workspace_from_merged;

    /// The one question the output JSON cannot answer: a module the reduce
    /// ruled unsafe and one it never saw are both absent from `LAZY_ELIGIBLE`.
    fn explain_for(present: bool, target: &str) -> String {
        explain_with(present, target, |_| {}, Vec::new())
    }

    fn explain_with(
        present: bool,
        target: &str,
        shape: impl FnOnce(&mut CachedModule),
        re_exports: Vec<CachedReExport>,
    ) -> String {
        let mut cache = LibraryCache::default();
        if present {
            let mut module = CachedModule::empty(ModuleName::from_str(target));
            shape(&mut module);
            cache.modules.push(module);
        }
        cache.exports.re_exports = re_exports;
        let workspace =
            reduce_workspace_from_merged(cache, AHashSet::new(), MergedClassFacts::default());
        let resolved = workspace.resolve(None);
        let options = Options {
            verbose_output_path: None,
            sorted_output: true,
            main_module: None,
            python_version: PythonVersion::default(),
        };
        let analysis = LifeGuardAnalysis::from_resolved_cache(&resolved, &options);
        let mut out = Vec::new();
        explain_module(&mut out, &resolved, &analysis, ModuleName::from_str(target))
            .expect("writing to a Vec should not fail");
        String::from_utf8(out).expect("explain output should be UTF-8")
    }

    #[test]
    fn explain_reports_a_module_no_library_contributed() {
        let out = explain_for(false, "pkg.never_cached");
        assert!(
            out.contains("merged cache: ABSENT"),
            "a module no cache carries must be called out as absent, got:\n{out}",
        );
    }

    #[test]
    fn explain_distinguishes_present_from_absent() {
        let out = explain_for(true, "pkg.analyzed");
        assert!(
            out.contains("merged cache: present"),
            "a module a cache carries must not read as absent, got:\n{out}",
        );
    }

    /// The function table is what shows a re-exported name has no verdict to
    /// carry: present upstream and absent downstream is the whole diagnosis.
    #[test]
    fn explain_lists_the_modules_function_table() {
        let out = explain_with(
            true,
            "pkg.mod",
            |module| {
                module.function_safety.insert(
                    "defined_here".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                );
            },
            Vec::new(),
        );
        assert!(
            out.contains("function_safety entries: 1") && out.contains("defined_here"),
            "the function table must be listed, got:\n{out}",
        );
    }

    /// A re-export edge can exist while the name it carries has no verdict;
    /// telling those apart needs both printed.
    #[test]
    fn explain_lists_re_export_edges() {
        let out = explain_with(
            true,
            "pkg.facade",
            |_| {},
            vec![CachedReExport {
                exported_module: ModuleName::from_str("pkg.facade"),
                exported_attr: Name::new("thing"),
                imported_module: ModuleName::from_str("pkg.real"),
                imported_attr: Name::new("thing"),
            }],
        );
        assert!(
            out.contains("1 it exports") && out.contains("exports thing <- pkg.real.thing"),
            "an outgoing re-export must be shown with its source, got:\n{out}",
        );
    }
}
