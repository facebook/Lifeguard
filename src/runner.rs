/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Common analysis pipeline for processing a source map.

use std::io::BufWriter;
use std::path::PathBuf;

use anyhow::Result;

use crate::debug::report_memory;
use crate::imports::ImportGraph;
use crate::module_safety;
use crate::output::LifeGuardAnalysis;
use crate::output::write_verbose;
use crate::project;
use crate::source_map::SourceMap;
use crate::source_map::Sources;
use crate::tracing::time;
use crate::traits::SysInfoExt;

/// Options for the analysis pipeline.
pub struct Options {
    pub verbose_output_path: Option<PathBuf>,
    pub sorted_output: bool,
}

/// Intermediate results from the analysis pipeline, before final output generation.
pub struct PipelineResult {
    pub sources: Sources,
    pub safety_map: project::SafetyMap,
    pub import_graph: ImportGraph,
    pub exports: crate::exports::Exports,
    pub side_effect_imports: project::SideEffectMap,
}

/// Run the analysis pipeline up to (but not including) final output generation.
/// Returns intermediate results that can be consumed by different output formats.
pub fn run_pipeline(src_map: &SourceMap, root_dir: &std::path::Path) -> Result<PipelineResult> {
    let sys_info = crate::pyrefly::sys_info::SysInfo::lg_default();

    let sources = time("Building sources", || {
        Sources::new(src_map, root_dir.to_path_buf())
    });

    let (import_graph, exports) = time("Creating import graph and exports", || {
        ImportGraph::make_with_exports(&sources, &sys_info)
    });
    report_memory("After creating import graph and exports");

    let output = time("Analyzing AST", || {
        project::run_analysis(&sources, &exports, &import_graph, &sys_info)
    });
    report_memory("After analyzing AST");

    // Surface parse errors in the safety map so they appear in the final output.
    for entry in output.parse_errors.iter() {
        output.safety_map.insert(
            *entry.key(),
            module_safety::SafetyResult::AnalysisError(anyhow::anyhow!(
                "Parse error: {}",
                entry.value()
            )),
        );
    }

    Ok(PipelineResult {
        sources,
        safety_map: output.safety_map,
        import_graph,
        exports,
        side_effect_imports: output.side_effect_imports,
    })
}

/// Process a source map and run the full analysis pipeline.
pub fn process_source_map(
    src_map: &SourceMap,
    root_dir: &std::path::Path,
    options: &Options,
) -> Result<LifeGuardAnalysis> {
    let result = run_pipeline(src_map, root_dir)?;
    let PipelineResult {
        sources,
        safety_map,
        import_graph,
        exports,
        side_effect_imports,
    } = result;

    if let Some(out) = &options.verbose_output_path {
        println!("Writing verbose output to {}", out.display());
        let verbose_file = std::fs::File::create(out)?;
        let mut writer = BufWriter::new(verbose_file);
        write_verbose(&mut writer, &safety_map, &sources)?;
    }

    let lifeguard_output = time("Creating analysis object", || {
        let mut analysis = LifeGuardAnalysis::new(safety_map, import_graph, &exports, options);
        analysis.propagate_side_effect_imports(&side_effect_imports);
        analysis
    });

    // Skip deallocation of large data structures since the process is about to exit.
    std::mem::forget(exports);

    Ok(lifeguard_output)
}
