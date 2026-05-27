/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::Result;
use clap::ArgAction;
use clap::Parser;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use tracing::info;

use crate::cache::LibraryCache;
use crate::debug::report_peak_memory;
use crate::output::LifeGuardAnalysis;
use crate::runner::Options;
use crate::tracing::ProcessTimer;
use crate::tracing::time;

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

    /// Name of the main module (the module run as __main__)
    #[arg(long = "main-module")]
    pub main_module: Option<String>,
}

pub fn run(args: AnalyzeBinaryArgs) -> Result<()> {
    let timer = ProcessTimer::new();

    let cache_paths: Vec<PathBuf> = std::fs::read_to_string(&args.cache_manifest)?
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();

    anyhow::ensure!(!cache_paths.is_empty(), "no cache paths provided");

    let mut caches: Vec<LibraryCache> = time("Loading caches", || {
        cache_paths
            .par_iter()
            .map(|p| {
                info!("Loading cache from {}", p.display());
                LibraryCache::read_from_file(p)
            })
            .collect::<Result<Vec<_>>>()
    })?;

    let mut merged = caches.swap_remove(0);
    if !caches.is_empty() {
        time("merge_dep_caches", || merged.merge_dep_caches(caches));
    }

    info!("Merged cache: {} modules", merged.modules.len());

    let options = Options {
        verbose_output_path: None,
        sorted_output: args.sorted_output,
        main_module: args.main_module.map(|s| ModuleName::from_str(&s)),
    };

    let analysis = time("Building analysis from cache", || {
        LifeGuardAnalysis::from_cache(&mut merged, &options)
    });

    info!("{}", time("Generating report", || analysis.get_report()));

    let output_file = std::fs::File::create(&args.output_path)?;
    let writer = BufWriter::new(output_file);
    serde_json::to_writer_pretty(writer, &analysis.output)?;

    info!("Output written to {}", args.output_path.display());
    report_peak_memory();
    info!("Full time executing: {:.2?}", timer.elapsed_wall());
    if let Some(cpu) = timer.elapsed_cpu() {
        info!("Full time executing (CPU): {:.2?}", cpu);
    }
    Ok(())
}
