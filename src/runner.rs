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
use pyrefly_python::module_name::ModuleName;

use crate::config::AnalysisConfig;
use crate::debug::report_memory;
use crate::imports::ImportGraph;
use crate::module_safety;
use crate::output::LifeGuardAnalysis;
use crate::output::write_verbose;
use crate::project;
use crate::project::ExecutionMode;
use crate::pyrefly::sys_info::PythonVersion;
use crate::source_map::SourceMap;
use crate::source_map::Sources;
use crate::tracing::time;

pub const DEFAULT_PYTHON_VERSION: &str = "3.14";

pub fn default_python_version() -> PythonVersion {
    DEFAULT_PYTHON_VERSION
        .parse()
        .expect("invalid DEFAULT_PYTHON_VERSION")
}

pub fn default_ruff_version() -> ruff_python_ast::PythonVersion {
    to_ruff_version(&default_python_version())
}

pub fn parse_python_version(s: &str) -> Result<PythonVersion> {
    let version = s
        .parse::<PythonVersion>()
        .map_err(|e| anyhow::anyhow!("Invalid python version '{}': {}", s, e))?;
    if version.major != 3 || version.minor < 12 {
        anyhow::bail!(
            "Unsupported python version '{}': minimum supported version is 3.12",
            s
        );
    }
    Ok(version)
}

pub fn to_ruff_version(v: &PythonVersion) -> ruff_python_ast::PythonVersion {
    match (v.major, v.minor) {
        (3, 12) => ruff_python_ast::PythonVersion::PY312,
        (3, 13) => ruff_python_ast::PythonVersion::PY313,
        (3, 14) => ruff_python_ast::PythonVersion::PY314,
        (3, 15) => ruff_python_ast::PythonVersion::PY315,
        // parse_python_version validates >= 3.12, so this only triggers
        // for future versions not yet in ruff; fall back to latest known.
        _ => ruff_python_ast::PythonVersion::PY315,
    }
}

/// Options for the analysis pipeline.
pub struct Options {
    pub verbose_output_path: Option<PathBuf>,
    pub sorted_output: bool,
    pub main_module: Option<ModuleName>,
    pub python_version: PythonVersion,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            verbose_output_path: None,
            sorted_output: false,
            main_module: None,
            python_version: default_python_version(),
        }
    }
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
pub fn run_pipeline(
    src_map: SourceMap,
    root_dir: &std::path::Path,
    mode: ExecutionMode,
    options: &Options,
) -> Result<PipelineResult> {
    let config = AnalysisConfig::with_python_version(options.python_version, options.main_module);

    let sources = time("Building sources", || {
        Sources::new_with_version(src_map, root_dir.to_path_buf(), options.python_version)
    });

    let (import_graph, exports) = time("Creating import graph and exports", || {
        ImportGraph::make_with_exports(&sources, &config)
    });
    report_memory("After creating import graph and exports");

    let output = time("Analyzing AST", || {
        project::run_analysis(&sources, &exports, &import_graph, &config, mode)
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
    src_map: SourceMap,
    root_dir: &std::path::Path,
    options: &Options,
) -> Result<LifeGuardAnalysis> {
    let result = run_pipeline(src_map, root_dir, ExecutionMode::WholeProgram, options)?;
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
