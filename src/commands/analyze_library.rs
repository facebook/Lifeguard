/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use rayon::prelude::*;
use tracing::info;

use crate::cache::LibraryCache;
use crate::debug::report_peak_memory;
use crate::runner::run_pipeline;
use crate::source_map;
use crate::source_map::SourceMap;
use crate::source_map::SourceResult;
use crate::tracing::ProcessTimer;
use crate::tracing::time;

#[derive(Parser)]
pub struct AnalyzeLibraryArgs {
    /// Path to input source db JSON file
    pub db_path: PathBuf,

    /// Path to output cache file
    pub cache_output_path: PathBuf,

    /// Paths to pre-computed dependency cache files.
    /// When provided, the source db should contain only the target's own
    /// source files. Results for dependency modules are merged from these caches.
    #[arg(long = "dep-cache")]
    pub dep_caches: Vec<PathBuf>,
}

/// Detect the root directory by walking up from cwd until a source file resolves.
/// Source DB paths (e.g., "fbcode/eden/fs/cli/constants.py") are relative to the
/// repo root, which may be an ancestor of cwd.
fn detect_root_dir(src_map: &SourceMap) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;

    // Get the first real file path from the source map
    let sample_path = src_map
        .values()
        .find_map(|v| match v {
            SourceResult::Ok(p) => Some(p),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("Source map has no valid file entries"))?;

    // Walk up from cwd until we find a directory where the path resolves
    let mut candidate = cwd.as_path();
    loop {
        if candidate.join(sample_path).exists() {
            return Ok(candidate.to_path_buf());
        }
        candidate = candidate.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find root directory: '{}' does not resolve from any ancestor of '{}'",
                sample_path.display(),
                cwd.display(),
            )
        })?;
    }
}

pub fn run(args: AnalyzeLibraryArgs) -> Result<()> {
    let timer = ProcessTimer::new();

    info!("Loading source db from {}", args.db_path.display());

    let src_map = time("Loading source db", || {
        source_map::load_source_map(&args.db_path)
    })?;

    let mut cache = if src_map.is_empty() {
        info!("Source map is empty, producing empty cache");
        LibraryCache::empty()
    } else {
        let root_dir = detect_root_dir(&src_map)?;
        let result = run_pipeline(&src_map, &root_dir)?;

        time("Building cache", || {
            LibraryCache::build(
                &result.safety_map,
                &result.import_graph,
                &result.exports,
                &result.side_effect_imports,
            )
        })
    };

    if !args.dep_caches.is_empty() {
        let dep_caches: Vec<LibraryCache> = time("Loading dep caches", || {
            args.dep_caches
                .par_iter()
                .map(|p| {
                    info!("Loading dep cache from {}", p.display());
                    LibraryCache::read_from_file(p)
                })
                .collect::<Result<Vec<_>>>()
        })?;

        time("Merging dep caches", || {
            cache.merge_dep_caches(dep_caches);
        });
    }

    time("Writing cache", || {
        cache.write_to_file(&args.cache_output_path)
    })?;

    let module_count = cache.modules.len();
    let safe_count = cache.modules.iter().filter(|m| m.is_safe()).count();

    println!("Cache written to {}", args.cache_output_path.display());
    println!(
        "Modules: {} ({} safe, {} failing)",
        module_count,
        safe_count,
        module_count - safe_count
    );

    report_peak_memory();
    println!("Full time executing: {:.2?}", timer.elapsed_wall());
    if let Some(cpu) = timer.elapsed_cpu() {
        println!("Full time executing (CPU): {:.2?}", cpu);
    }
    Ok(())
}
