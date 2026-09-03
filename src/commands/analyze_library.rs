/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;
use tracing::warn;

use crate::cache::LibraryCache;
use crate::runner::DEFAULT_PYTHON_VERSION;
use crate::runner::Options;
use crate::runner::analyze_library;
use crate::runner::parse_python_version;
use crate::source_map;
use crate::source_map::SourceMap;
use crate::tracing::ProcessTimer;
use crate::tracing::time;

#[derive(Parser)]
pub struct AnalyzeLibraryArgs {
    /// Path to input source db JSON file
    pub db_path: PathBuf,

    /// Path to output cache file
    pub cache_output_path: PathBuf,

    /// Deprecated: dependency caches are no longer used during library analysis.
    /// Cross-library resolution is handled entirely in the reduce step (analyze-binary).
    #[arg(long = "dep-cache", hide = true)]
    pub dep_caches: Vec<PathBuf>,

    /// Python version to use for parsing
    #[arg(long = "python-version", default_value = DEFAULT_PYTHON_VERSION)]
    pub python_version: String,
}

/// Detect the root directory by walking up from cwd until a source file resolves.
/// Source DB paths (e.g., "fbcode/eden/fs/cli/constants.py") are relative to the
/// repo root, which may be an ancestor of cwd.
///
/// Tries multiple sample paths so that one transient build-output entry whose
/// hash doesn't match locally cannot break detection.
fn detect_root_dir(src_map: &SourceMap) -> Result<PathBuf> {
    const MAX_SAMPLES: usize = 32;

    let cwd = std::env::current_dir()?;

    let samples: Vec<&Path> = src_map
        .values()
        .map(PathBuf::as_path)
        .take(MAX_SAMPLES)
        .collect();
    if samples.is_empty() {
        return Err(anyhow::anyhow!("Source map has no valid file entries"));
    }

    let mut candidate = cwd.as_path();
    loop {
        if samples.iter().any(|s| candidate.join(s).exists()) {
            return Ok(candidate.to_path_buf());
        }
        candidate = candidate.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find root directory: none of {} sample paths resolve from any ancestor of '{}'",
                samples.len(),
                cwd.display(),
            )
        })?;
    }
}

/// Modules per worker thread. Below this, rayon splits the work so finely that
/// scheduling costs more than it saves.
const MODULES_PER_THREAD: usize = 64;

/// Cap the rayon pool to the size of this action's workload.
///
/// One `analyze-library` action analyzes its own sources plus the bundled stubs
/// they reach — usually a few hundred modules. On a many-core host rayon's default
/// pool turns that into a swarm of tiny tasks: measured on a one-module library,
/// going from 8 to 72 threads left wall time unchanged while CPU grew ~8x. Thousands
/// of these actions share a machine, so that CPU is contention, not speed, is the main issue.
fn size_rayon_pool(source_count: usize) {
    let available = std::thread::available_parallelism().map_or(1, |n| n.get());
    // Reached stubs dominate the workload for a small library, so a library with
    // almost no sources of its own still deserves more than one thread.
    let modules = source_count + REACHED_STUB_ESTIMATE;
    let threads = (modules / MODULES_PER_THREAD).clamp(1, available);
    if let Err(e) = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
    {
        warn!("could not size the rayon pool to {threads} threads: {e}");
    }
}

/// Rough count of bundled stubs a library pulls in; see `build_with_exports`.
const REACHED_STUB_ESTIMATE: usize = 200;

pub fn run(args: AnalyzeLibraryArgs) -> Result<()> {
    let timer = ProcessTimer::new();

    if !args.dep_caches.is_empty() {
        warn!(
            "--dep-cache is deprecated and ignored. \
             Cross-library resolution is now handled entirely by analyze-binary."
        );
    }

    info!("Loading source db from {}", args.db_path.display());

    let src_map = time("Loading source db", || -> Result<SourceMap> {
        // Sized before the first parallel iterator, so parsing stays serial here.
        let raw = source_map::load_raw_source_map(&args.db_path)?;
        size_rayon_pool(raw.len());
        Ok(time("  Resolving source map", || {
            source_map::resolve_source_map(raw)
        }))
    })?;

    let python_version = parse_python_version(&args.python_version)?;

    let options = Options {
        verbose_output_path: None,
        sorted_output: false,
        main_module: None,
        python_version,
    };

    let cache = if src_map.is_empty() {
        info!("Source map is empty, producing empty cache");
        LibraryCache::empty()
    } else {
        let root_dir = detect_root_dir(&src_map)?;

        let result = analyze_library(src_map, &root_dir, &options)?;

        let mut cache = time("Building cache", || {
            LibraryCache::build(
                &result.safety_map,
                &result.import_graph,
                &result.exports,
                &result.side_effect_imports,
            )
        });
        cache.set_class_bases(result.class_bases);
        cache.set_constructor_callees(result.constructor_callees);
        cache
    };

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

    timer.report_finish();
    Ok(())
}
