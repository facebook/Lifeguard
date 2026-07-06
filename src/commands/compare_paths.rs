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

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use pyrefly_python::module_name::ModuleName;

use crate::cache::LibraryCache;
use crate::output::LifeGuardOutput;
use crate::project::ExecutionMode;
use crate::runner::DEFAULT_PYTHON_VERSION;
use crate::runner::Options;
use crate::runner::parse_python_version;
use crate::runner::process_source_map;
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

    /// Python version to use for parsing
    #[arg(long = "python-version", default_value = DEFAULT_PYTHON_VERSION)]
    pub python_version: String,
}

/// The failing (must-load-eagerly) deps of each lazy-eligible module.
type EligibleMap = HashMap<ModuleName, HashSet<ModuleName>>;

fn eligible_map(out: &LifeGuardOutput) -> EligibleMap {
    out.lazy_eligible
        .iter()
        .map(|e| (*e.key(), e.value().iter().copied().collect()))
        .collect()
}

fn eager_set(out: &LifeGuardOutput) -> HashSet<ModuleName> {
    out.load_imports_eagerly.iter().copied().collect()
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

    // Single-pass (whole-program) path. The source map is consumed, so load once
    // per path.
    let single_pass = {
        let src_map = source_map::load_source_map(&args.db_path)?;
        process_source_map(src_map, &root_dir, &options)?
    };

    // Incremental path: map (one library) then reduce, mirroring
    // analyze-library + analyze-binary for a single cache.
    let incremental = {
        let src_map = source_map::load_source_map(&args.db_path)?;
        let result = run_pipeline(src_map, &root_dir, ExecutionMode::Incremental, &options)?;
        let mut cache = LibraryCache::build(
            &result.safety_map,
            &result.import_graph,
            &result.exports,
            &result.side_effect_imports,
        );
        // Per-library caches drop stub-only modules; the reduce re-adds them.
        let graph_only_stubs = cache.inject_bundled_stub_graph(python_version);
        crate::output::LifeGuardAnalysis::from_cache(&mut cache, &graph_only_stubs, &options)
    };

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
