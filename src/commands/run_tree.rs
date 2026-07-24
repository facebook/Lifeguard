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

use crate::commands::write_json_pretty;
use crate::find_sources::build_source_db;
use crate::find_sources::make_source_map;
use crate::runner::DEFAULT_PYTHON_VERSION;
use crate::runner::Options;
use crate::runner::parse_python_version;
use crate::runner::process_source_map;
use crate::tracing::ProcessTimer;
use crate::tracing::time;

#[derive(Parser)]
pub struct RunTreeArgs {
    /// Directory containing Python files to analyze
    input_dir: PathBuf,

    /// Path to output file
    output_path: PathBuf,

    /// Path to verbose output file.
    #[arg(long = "verbose-output")]
    verbose_output_path: Option<PathBuf>,

    /// Path to site-packages directory (overrides pyproject.toml setting)
    #[arg(long)]
    site_packages: Option<PathBuf>,

    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    print_diagnostics: bool,

    /// Sort output keys and values for deterministic results
    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    sorted_output: bool,

    /// Name of the main module (the module run as __main__)
    #[arg(long = "main-module")]
    main_module: Option<String>,

    /// Python version to use for parsing
    #[arg(long = "python-version", default_value = DEFAULT_PYTHON_VERSION)]
    python_version: String,
}

pub fn run(args: RunTreeArgs) -> Result<()> {
    let timer = ProcessTimer::new();
    let cwd = std::env::current_dir()?;

    let python_version = parse_python_version(&args.python_version)?;

    let (build_map, _) = time("Discovering sources", || {
        build_source_db(
            &args.input_dir,
            args.site_packages.as_deref(),
            python_version,
        )
    })?;
    println!("Found {} Python files", build_map.len());

    let source_map = make_source_map(build_map);

    let options = Options {
        verbose_output_path: args.verbose_output_path,
        sorted_output: args.sorted_output,
        main_module: args.main_module.map(|s| ModuleName::from_str(&s)),
        python_version,
    };

    let lifeguard_output = process_source_map(source_map, &cwd, &options)?;

    println!(
        "--- Lifeguard Analysis for {} ---",
        args.input_dir.display()
    );
    println!(
        "{}",
        time("Generating report", || lifeguard_output.get_report())
    );

    if args.print_diagnostics {
        lifeguard_output.print_diagnostics();
    }

    write_json_pretty(&args.output_path, &lifeguard_output.output)?;

    println!("Output written to {}", args.output_path.display());
    timer.report_finish();
    Ok(())
}
