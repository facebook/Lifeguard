/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use anyhow::Result;
use clap::ArgAction;
use clap::Parser;
use pyrefly_python::module_name::ModuleName;
use tracing::info;

use crate::commands::write_json_pretty;
use crate::runner::DEFAULT_PYTHON_VERSION;
use crate::runner::Options;
use crate::runner::parse_python_version;
use crate::runner::process_source_map;
use crate::source_map;
use crate::tracing::ProcessTimer;
use crate::tracing::time;

#[derive(Parser)]
pub struct AnalyzeArgs {
    /// Path to input source db JSON file
    pub db_path: Option<PathBuf>,

    /// Path to output file
    pub output_path: Option<PathBuf>,

    /// Path to verbose output file.
    #[arg(long = "verbose-output")]
    pub verbose_output_path: Option<PathBuf>,

    /// Name of the analyzed buck target.  Optional, used only for printing.
    #[arg(long = "target")]
    pub buck_target: Option<String>,

    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    pub print_diagnostics: bool,

    /// Deprecated: accepted for backwards compatibility but ignored
    #[arg(long = "buck_mode")]
    pub buck_mode: Option<String>,

    /// Root directory of the source tree (defaults to current working directory)
    #[arg(long = "root-dir")]
    pub root_dir: Option<PathBuf>,

    /// Sort output keys and values for deterministic results
    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    pub sorted_output: bool,

    /// Name of the main module (the module run as __main__)
    #[arg(long = "main-module")]
    pub main_module: Option<String>,

    /// Python version to use for parsing
    #[arg(long = "python-version", default_value = DEFAULT_PYTHON_VERSION)]
    pub python_version: String,
}

pub fn run(args: AnalyzeArgs) -> Result<()> {
    let timer = ProcessTimer::new();

    let db_path = args
        .db_path
        .ok_or_else(|| anyhow::anyhow!("missing required argument: <DB_PATH>"))?;
    let output_path = args
        .output_path
        .ok_or_else(|| anyhow::anyhow!("missing required argument: <OUTPUT_PATH>"))?;

    info!("Loading source db from {}", db_path.display());

    let src_map = time("Loading source db", || {
        source_map::load_source_map(&db_path)
    })?;

    let root_dir = match args.root_dir {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };

    let python_version = parse_python_version(&args.python_version)?;

    let options = Options {
        verbose_output_path: args.verbose_output_path,
        sorted_output: args.sorted_output,
        main_module: args.main_module.map(|s| ModuleName::from_str(&s)),
        python_version,
    };

    let lifeguard_output = process_source_map(src_map, &root_dir, &options)?;

    if let Some(buck_target) = args.buck_target {
        println!("--- Lifeguard Analysis for {} ---", buck_target);
    }
    println!(
        "{}",
        time("Generating report", || lifeguard_output.get_report())
    );

    if args.print_diagnostics {
        lifeguard_output.print_diagnostics();
    }

    write_json_pretty(&output_path, &lifeguard_output.output)?;

    println!("Output written to {}", output_path.display());
    timer.report_finish();
    Ok(())
}
