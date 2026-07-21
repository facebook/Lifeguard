/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Run the same source DB through both analysis paths — the single-pass
//! whole-program analyzer and the incremental map-reduce (map + reduce in a
//! single library) — and diff their outputs.
//!
//! Intended for CI: the two paths should agree, and this command fails when the
//! number of modules on which they disagree exceeds `--max-divergent-modules`.
//! The budget lets CI ratchet the gap down as the paths converge without
//! blocking on the residual (e.g. the discharge/eligibility divergence).
//!
//! `--explain <module>` prints each path's verdict, failing deps, and
//! post-resolution per-module errors side-by-side, then highlights the
//! path-local differences for that module.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use pyrefly_python::module_name::ModuleName;

use crate::cache::CachedSafety;
use crate::cache::LibraryCache;
use crate::output::LifeGuardAnalysis;
use crate::output::LifeGuardOutput;
use crate::project::ExecutionMode;
use crate::runner::DEFAULT_PYTHON_VERSION;
use crate::runner::Options;
use crate::runner::parse_python_version;
use crate::runner::run_pipeline;
use crate::source_map;

#[derive(Parser)]
pub struct ComparePathsArgs {
    /// Path to input source db JSON file
    pub db_path: PathBuf,

    /// Root directory of the source tree (defaults to current working directory).
    /// Required for the single-pass path to resolve repo-root-relative DB paths.
    #[arg(long = "root-dir")]
    pub root_dir: Option<PathBuf>,

    /// Fail (exit code 1) when more than this many modules diverge between the
    /// two paths. Defaults to 0 (require exact agreement).
    #[arg(long = "max-divergent-modules", default_value_t = 0)]
    pub max_divergent_modules: usize,

    /// Max example module names to print per category.
    #[arg(long = "examples", default_value_t = 20)]
    pub examples: usize,

    /// Print each path's verdict, failing deps, and errors for this module.
    #[arg(long = "explain")]
    pub explain: Option<String>,

    /// Python version to use for parsing
    #[arg(long = "python-version", default_value = DEFAULT_PYTHON_VERSION)]
    pub python_version: String,
}

/// The failing (must-load-eagerly) deps of each lazy-eligible module.
type EligibleMap = HashMap<ModuleName, HashSet<ModuleName>>;
/// Per-module analysis errors, formatted as "<kind> <metadata>".
type ErrorMap = HashMap<ModuleName, Vec<String>>;

/// The result of running one analysis path: its output plus the per-module
/// errors it produced (kept for `--explain`).
struct PathResult {
    output: LifeGuardOutput,
    errors: ErrorMap,
}

fn eligible_map(out: &LifeGuardOutput) -> EligibleMap {
    out.lazy_eligible
        .iter()
        .map(|e| (*e.key(), e.value().iter().copied().collect()))
        .collect()
}

fn eager_set(out: &LifeGuardOutput) -> HashSet<ModuleName> {
    out.load_imports_eagerly.iter().copied().collect()
}

/// Extract per-module errors from a library cache, matching the verbose
/// output's "<kind> <metadata>" form (without the line number, which is not
/// available on the incremental side for a symmetric comparison).
fn cache_errors(cache: &LibraryCache) -> ErrorMap {
    cache
        .modules
        .iter()
        .filter_map(|m| match &m.safety {
            CachedSafety::Ok(s) if !s.errors.is_empty() => Some((
                m.name,
                s.errors
                    .iter()
                    .map(|e| format!("{:?} {}", e.kind, e.metadata))
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

/// Run the same post-map reduction steps that can clear cross-library false
/// positives, then read the remaining per-module errors.
fn resolved_cache_errors(mut cache: LibraryCache) -> ErrorMap {
    cache.resolve_cross_library_errors();
    cache_errors(&cache)
}

/// Rebuild graph-only bundled stubs before resolution, matching the incremental
/// binary reduce path.
fn post_resolution_errors(mut cache: LibraryCache, options: &Options) -> ErrorMap {
    cache.inject_bundled_stub_graph(options.python_version);
    resolved_cache_errors(cache)
}

fn sorted(names: impl IntoIterator<Item = ModuleName>) -> Vec<ModuleName> {
    let mut v: Vec<ModuleName> = names.into_iter().collect();
    v.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    v
}

fn print_examples(label: &str, names: &[ModuleName], limit: usize) {
    if names.is_empty() {
        return;
    }
    println!("  {} ({}):", label, names.len());
    for name in names.iter().take(limit) {
        println!("    {}", name.as_str());
    }
    if names.len() > limit {
        println!("    ... and {} more", names.len() - limit);
    }
}

fn print_module_names(indent: &str, label: &str, names: &[ModuleName], limit: usize) {
    if names.is_empty() {
        return;
    }
    println!("{}{} ({}):", indent, label, names.len());
    for name in names.iter().take(limit) {
        println!("{}  {}", indent, name.as_str());
    }
    if names.len() > limit {
        println!("{}  ... and {} more", indent, names.len() - limit);
    }
}

/// Run the single-pass (whole-program) path, capturing its output and errors.
fn run_single_pass(
    args: &ComparePathsArgs,
    root_dir: &Path,
    options: &Options,
    compute_errors: bool,
) -> Result<PathResult> {
    let src_map = source_map::load_source_map(&args.db_path)?;
    let result = run_pipeline(src_map, root_dir, ExecutionMode::WholeProgram, options)?;
    let errors = if compute_errors {
        post_resolution_errors(
            LibraryCache::build(
                &result.safety_map,
                &result.import_graph,
                &result.exports,
                &result.side_effect_imports,
            ),
            options,
        )
    } else {
        HashMap::new()
    };
    let mut analysis = LifeGuardAnalysis::new(
        result.safety_map,
        result.import_graph,
        &result.exports,
        options,
    );
    analysis.propagate_side_effect_imports(&result.side_effect_imports);
    // Skip deallocation of large data structures since the process is about to exit.
    std::mem::forget(result.exports);
    Ok(PathResult {
        output: analysis.output,
        errors,
    })
}

/// Run the incremental path (single-library map + reduce), capturing its output
/// and the reduced errors.
fn run_incremental(
    args: &ComparePathsArgs,
    root_dir: &Path,
    options: &Options,
    compute_errors: bool,
) -> Result<PathResult> {
    let src_map = source_map::load_source_map(&args.db_path)?;
    let result = run_pipeline(src_map, root_dir, ExecutionMode::Incremental, options)?;
    let mut cache = LibraryCache::build(
        &result.safety_map,
        &result.import_graph,
        &result.exports,
        &result.side_effect_imports,
    );
    // Per-library caches drop stub-only modules; the reduce re-adds them.
    let graph_only_stubs = cache.inject_bundled_stub_graph(options.python_version);
    let analysis = LifeGuardAnalysis::from_cache(&mut cache, &graph_only_stubs, options);
    // The reduce cleared/retained errors in place, so read them post-resolution.
    let errors = if compute_errors {
        cache_errors(&cache)
    } else {
        HashMap::new()
    };
    Ok(PathResult {
        output: analysis.output,
        errors,
    })
}

/// Per-path state needed for `--explain` output.
#[derive(Clone, Copy)]
struct ExplainState<'a> {
    eligible: &'a EligibleMap,
    eager: &'a HashSet<ModuleName>,
    errors: &'a ErrorMap,
}

/// Print one path's verdict, failing deps, and errors for `module`.
fn explain_side(label: &str, module: &ModuleName, state: ExplainState, limit: usize) {
    let verdict = if let Some(deps) = state.eligible.get(module) {
        format!("LAZY_ELIGIBLE ({} failing deps)", deps.len())
    } else {
        "not lazy-eligible".to_owned()
    };
    let eager_note = if state.eager.contains(module) {
        " [LOAD_IMPORTS_EAGERLY]"
    } else {
        ""
    };
    println!("  {}: {}{}", label, verdict, eager_note);

    if let Some(deps) = state.eligible.get(module) {
        let deps = sorted(deps.iter().copied());
        print_module_names("    ", "failing deps", &deps, limit);
    }

    match state.errors.get(module) {
        None => println!("    errors: (none)"),
        Some(errs) => {
            // Group identical errors with a count (a decorator applied N times
            // produces N identical entries).
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for e in errs {
                *counts.entry(e.as_str()).or_default() += 1;
            }
            println!("    errors ({}):", errs.len());
            for (e, n) in counts {
                if n > 1 {
                    println!("      {} (x{})", e, n);
                } else {
                    println!("      {}", e);
                }
            }
        }
    }
}

fn eligible_deps_only_in(
    left: &EligibleMap,
    right: &EligibleMap,
    module: &ModuleName,
) -> Vec<ModuleName> {
    match (left.get(module), right.get(module)) {
        (Some(left), Some(right)) => sorted(left.difference(right).copied()),
        (Some(left), None) => sorted(left.iter().copied()),
        _ => Vec::new(),
    }
}

fn error_counts(errors: Option<&Vec<String>>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for error in errors.into_iter().flatten() {
        *counts.entry(error.clone()).or_default() += 1;
    }
    counts
}

fn error_counts_only_in(
    left: &BTreeMap<String, usize>,
    right: &BTreeMap<String, usize>,
) -> Vec<(String, usize)> {
    left.iter()
        .filter_map(|(error, left_count)| {
            let right_count = right.get(error).copied().unwrap_or_default();
            (left_count > &right_count).then(|| (error.clone(), *left_count - right_count))
        })
        .collect()
}

fn print_error_differences(indent: &str, label: &str, errors: &[(String, usize)], limit: usize) {
    if errors.is_empty() {
        return;
    }
    println!("{}{} ({}):", indent, label, errors.len());
    for (error, count) in errors.iter().take(limit) {
        if *count > 1 {
            println!("{}  {} (x{})", indent, error, count);
        } else {
            println!("{}  {}", indent, error);
        }
    }
    if errors.len() > limit {
        println!("{}  ... and {} more", indent, errors.len() - limit);
    }
}

fn explain_differences(
    module: &ModuleName,
    single_pass: ExplainState,
    incremental: ExplainState,
    limit: usize,
) {
    let sp_eligible = single_pass.eligible.contains_key(module);
    let inc_eligible = incremental.eligible.contains_key(module);
    let sp_eager = single_pass.eager.contains(module);
    let inc_eager = incremental.eager.contains(module);
    let deps_sp_only = eligible_deps_only_in(single_pass.eligible, incremental.eligible, module);
    let deps_inc_only = eligible_deps_only_in(incremental.eligible, single_pass.eligible, module);
    let sp_error_counts = error_counts(single_pass.errors.get(module));
    let inc_error_counts = error_counts(incremental.errors.get(module));
    let errors_sp_only = error_counts_only_in(&sp_error_counts, &inc_error_counts);
    let errors_inc_only = error_counts_only_in(&inc_error_counts, &sp_error_counts);

    let has_diff = sp_eligible != inc_eligible
        || sp_eager != inc_eager
        || !deps_sp_only.is_empty()
        || !deps_inc_only.is_empty()
        || !errors_sp_only.is_empty()
        || !errors_inc_only.is_empty();

    if !has_diff {
        println!("  divergence detail: none for this module");
        return;
    }

    println!("  divergence detail:");
    if sp_eligible != inc_eligible {
        let side = if sp_eligible {
            "single-pass"
        } else {
            "incremental"
        };
        println!("    lazy_eligible: only in {}", side);
    }
    if sp_eager != inc_eager {
        let side = if sp_eager {
            "single-pass"
        } else {
            "incremental"
        };
        println!("    load_imports_eagerly: only in {}", side);
    }
    print_module_names(
        "    ",
        "failing deps only in single-pass",
        &deps_sp_only,
        limit,
    );
    print_module_names(
        "    ",
        "failing deps only in incremental",
        &deps_inc_only,
        limit,
    );
    print_error_differences("    ", "errors only in single-pass", &errors_sp_only, limit);
    print_error_differences(
        "    ",
        "errors only in incremental",
        &errors_inc_only,
        limit,
    );
}

pub fn run(args: ComparePathsArgs) -> Result<()> {
    let root_dir = match &args.root_dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir()?,
    };
    let python_version = parse_python_version(&args.python_version)?;

    // Sort so failing-dep sets are order-independent (comparison uses sets anyway).
    let options = Options {
        verbose_output_path: None,
        sorted_output: true,
        main_module: None,
        python_version,
    };

    let compute_errors = args.explain.is_some();
    let single_pass = run_single_pass(&args, &root_dir, &options, compute_errors)?;
    let incremental = run_incremental(&args, &root_dir, &options, compute_errors)?;

    let sp = eligible_map(&single_pass.output);
    let inc = eligible_map(&incremental.output);
    let sp_eager = eager_set(&single_pass.output);
    let inc_eager = eager_set(&incremental.output);

    let sp_keys: HashSet<ModuleName> = sp.keys().copied().collect();
    let inc_keys: HashSet<ModuleName> = inc.keys().copied().collect();

    let sp_only_eligible = sorted(sp_keys.difference(&inc_keys).copied());
    let inc_only_eligible = sorted(inc_keys.difference(&sp_keys).copied());
    let differing_deps = sorted(
        sp_keys
            .intersection(&inc_keys)
            .copied()
            .filter(|m| sp.get(m) != inc.get(m)),
    );
    let eager_sp_only = sorted(sp_eager.difference(&inc_eager).copied());
    let eager_inc_only = sorted(inc_eager.difference(&sp_eager).copied());

    // A module is "divergent" if the two paths disagree about it in any way.
    let divergent: HashSet<ModuleName> = sp_only_eligible
        .iter()
        .chain(&inc_only_eligible)
        .chain(&differing_deps)
        .chain(&eager_sp_only)
        .chain(&eager_inc_only)
        .copied()
        .collect();

    println!("=== Lifeguard path comparison (single-pass vs incremental) ===");
    println!("DB: {}", args.db_path.display());
    println!(
        "lazy_eligible modules:      single-pass {}, incremental {}",
        sp.len(),
        inc.len()
    );
    println!(
        "load_imports_eagerly:       single-pass {}, incremental {}",
        sp_eager.len(),
        inc_eager.len()
    );
    println!("--- divergences ---");
    print_examples(
        "eligible only in single-pass",
        &sp_only_eligible,
        args.examples,
    );
    print_examples(
        "eligible only in incremental",
        &inc_only_eligible,
        args.examples,
    );
    print_examples(
        "eligible in both, differing failing-dep sets",
        &differing_deps,
        args.examples,
    );
    print_examples(
        "load-imports-eagerly only in single-pass",
        &eager_sp_only,
        args.examples,
    );
    print_examples(
        "load-imports-eagerly only in incremental",
        &eager_inc_only,
        args.examples,
    );

    if let Some(target) = &args.explain {
        let module = ModuleName::from_str(target);
        println!("--- explain: {} ---", target);
        println!("  note: errors are post cross-library resolution for both paths");
        let sp_state = ExplainState {
            eligible: &sp,
            eager: &sp_eager,
            errors: &single_pass.errors,
        };
        let inc_state = ExplainState {
            eligible: &inc,
            eager: &inc_eager,
            errors: &incremental.errors,
        };
        // Detect when the module is absent from both paths to avoid misleading empty output.
        let sp_present = sp_state.eligible.contains_key(&module)
            || sp_state.eager.contains(&module)
            || sp_state.errors.contains_key(&module);
        let inc_present = inc_state.eligible.contains_key(&module)
            || inc_state.eager.contains(&module)
            || inc_state.errors.contains_key(&module);
        if !sp_present && !inc_present {
            println!("  note: module not found in either path (check spelling or DB contents)");
        }
        explain_side("single-pass", &module, sp_state, args.examples);
        explain_side("incremental", &module, inc_state, args.examples);
        explain_differences(&module, sp_state, inc_state, args.examples);
    }

    let total = divergent.len();
    println!("--- summary ---");
    println!("total divergent modules: {}", total);
    println!(
        "budget (max-divergent-modules): {}",
        args.max_divergent_modules
    );

    if total > args.max_divergent_modules {
        anyhow::bail!(
            "single-pass and incremental outputs diverge on {} modules, exceeding the budget of {}",
            total,
            args.max_divergent_modules,
        );
    }
    println!("OK: divergence within budget.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CachedError;
    use crate::cache::CachedExports;
    use crate::cache::CachedModule;
    use crate::cache::CachedModuleSafety;
    use crate::errors::ErrorKind;
    use crate::hasher::AHashSet;
    use crate::hasher::HashSetExt;
    use crate::module_safety::FunctionSafety;
    use crate::module_safety::FunctionSafetyInfo;

    fn mn(name: &str) -> ModuleName {
        ModuleName::from_str(name)
    }

    fn empty_exports() -> CachedExports {
        CachedExports {
            definitions: Vec::new(),
            re_exports: Vec::new(),
            all: Vec::new(),
            return_types: Vec::new(),
        }
    }

    #[test]
    fn eligible_deps_only_in_detects_same_size_different_sets() {
        let module = mn("pkg.module");
        let dep_a = mn("pkg.dep_a");
        let dep_b = mn("pkg.dep_b");
        let dep_c = mn("pkg.dep_c");

        let single_pass =
            EligibleMap::from([(module, [dep_a, dep_b].into_iter().collect::<HashSet<_>>())]);
        let incremental =
            EligibleMap::from([(module, [dep_b, dep_c].into_iter().collect::<HashSet<_>>())]);

        assert_eq!(
            eligible_deps_only_in(&single_pass, &incremental, &module),
            vec![dep_a],
        );
        assert_eq!(
            eligible_deps_only_in(&incremental, &single_pass, &module),
            vec![dep_c],
        );
    }

    #[test]
    fn error_counts_only_in_respects_duplicate_counts() {
        let single_pass =
            BTreeMap::from([("shared".to_owned(), 2), ("single-pass-only".to_owned(), 1)]);
        let incremental =
            BTreeMap::from([("shared".to_owned(), 1), ("incremental-only".to_owned(), 3)]);

        assert_eq!(
            error_counts_only_in(&single_pass, &incremental),
            vec![("shared".to_owned(), 1), ("single-pass-only".to_owned(), 1),],
        );
        assert_eq!(
            error_counts_only_in(&incremental, &single_pass),
            vec![("incremental-only".to_owned(), 3)],
        );
    }

    #[test]
    fn resolved_cache_errors_clear_verified_missing_import_errors() {
        let caller = mn("pkg.caller");
        let dependency = mn("pkg.dependency");

        let cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: caller,
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnsafeFunctionCall,
                            metadata: "pkg.dependency.safe_func()".to_owned(),
                        }],
                        force_imports_eager_overrides: Vec::new(),
                        implicit_imports: Vec::new(),
                    }),
                    imports: AHashSet::new(),
                    missing_imports: [dependency].into_iter().collect(),
                    ambiguous_imports: AHashSet::new(),
                    side_effect_imports: AHashSet::new(),
                    function_safety: HashMap::new(),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: dependency,
                    safety: CachedSafety::Ok(CachedModuleSafety::default()),
                    imports: AHashSet::new(),
                    missing_imports: AHashSet::new(),
                    ambiguous_imports: AHashSet::new(),
                    side_effect_imports: AHashSet::new(),
                    function_safety: HashMap::from([(
                        "safe_func".to_owned(),
                        FunctionSafetyInfo::new(FunctionSafety::Safe),
                    )]),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: empty_exports(),
        };

        assert_eq!(
            cache_errors(&cache).get(&caller),
            Some(&vec![
                "UnsafeFunctionCall pkg.dependency.safe_func()".to_owned()
            ]),
        );

        let errors = resolved_cache_errors(cache);

        assert!(
            !errors.contains_key(&caller),
            "verified cross-library call should not remain in compare-path errors",
        );
    }
}
