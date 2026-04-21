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

use crate::find_sources::build_source_db;
use crate::find_sources::make_source_map;
use crate::runner::Options;
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
}

pub fn run(args: RunTreeArgs) -> Result<()> {
    let timer = ProcessTimer::new();
    let cwd = std::env::current_dir()?;

    let (build_map, _) = time("Discovering sources", || {
        build_source_db(&args.input_dir, args.site_packages.as_deref())
    })?;
    println!("Found {} Python files", build_map.len());

    let source_map = make_source_map(build_map);

    let options = Options {
        verbose_output_path: args.verbose_output_path,
        sorted_output: args.sorted_output,
    };

    let lifeguard_output = process_source_map(&source_map, &cwd, &options)?;

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

    // Write the lifeguard_output to the specified output file
    let output_file = std::fs::File::create(&args.output_path)?;
    let writer = BufWriter::new(output_file);
    serde_json::to_writer_pretty(writer, &lifeguard_output.output)?;

    println!("Output written to {}", args.output_path.display());
    println!("Full time executing: {:.2?}", timer.elapsed_wall());
    if let Some(cpu) = timer.elapsed_cpu() {
        println!("Full time executing (CPU): {:.2?}", cpu);
    }
    Ok(())
}
