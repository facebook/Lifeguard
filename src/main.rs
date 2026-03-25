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
use clap::Subcommand;
use lifeguard::commands::run_tree::RunTreeArgs;
use lifeguard::debug::report_peak_memory;
use lifeguard::runner::Options;
use lifeguard::runner::process_source_map;
use lifeguard::source_map;
use lifeguard::tracing::ProcessTimer;
use lifeguard::tracing::time;
use tracing::info;

#[derive(Parser)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to input source db JSON file
    db_path: Option<PathBuf>,

    /// Path to output file
    output_path: Option<PathBuf>,

    /// Path to verbose output file.
    #[arg(long = "verbose-output")]
    verbose_output_path: Option<PathBuf>,

    /// Name of the analyzed buck target.  Optional, used only for printing.
    #[arg(long = "target")]
    buck_target: Option<String>,

    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    print_diagnostics: bool,

    /// Deprecated: accepted for backwards compatibility but ignored
    #[arg(long = "buck_mode")]
    buck_mode: Option<String>,

    /// Root directory of the source tree (defaults to current working directory)
    #[arg(long = "root-dir")]
    root_dir: Option<PathBuf>,

    /// Sort output keys and values for deterministic results
    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    sorted_output: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Analyze all Python files in a directory tree
    RunTree(RunTreeArgs),
}

fn run_analyze(args: Cli) -> Result<()> {
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

    let options = Options {
        verbose_output_path: args.verbose_output_path,
        sorted_output: args.sorted_output,
    };

    let lifeguard_output = process_source_map(&src_map, &root_dir, &options)?;

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

    // Write the lifeguard_output to the specified output file
    let output_file = std::fs::File::create(&output_path)?;
    let writer = BufWriter::new(output_file);
    serde_json::to_writer_pretty(writer, &lifeguard_output.output)?;

    println!("Output written to {}", output_path.display());
    report_peak_memory();
    println!("Full time executing: {:.2?}", timer.elapsed_wall());
    if let Some(cpu) = timer.elapsed_cpu() {
        println!("Full time executing (CPU): {:.2?}", cpu);
    }
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Cli::parse();

    match args.command {
        Some(Commands::RunTree(args)) => lifeguard::commands::run_tree::run(args),
        None => run_analyze(args),
    }
}
