/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use lifeguard::commands::analyze::AnalyzeArgs;
use lifeguard::commands::gen_source_db::GenSourceDbArgs;
use lifeguard::commands::run_tree::RunTreeArgs;
use lifeguard::commands::show_effects::ShowEffectsArgs;

#[derive(Parser)]
#[command(args_conflicts_with_subcommands = true, version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[command(flatten)]
    analyze: AnalyzeArgs,
}

#[derive(Subcommand)]
enum Commands {
    /// [Default Behavior] Analyze a source DB to determine which modules can be safely lazily imported
    Analyze(AnalyzeArgs),
    /// Analyze all Python files in a directory tree
    RunTree(RunTreeArgs),
    /// Dump effects for a single Python file (.py or .pyi)
    ShowEffects(ShowEffectsArgs),
    /// Generate a source DB JSON file from a directory tree
    GenSourceDb(GenSourceDbArgs),
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Cli::parse();

    match args.command {
        Some(Commands::Analyze(args)) => lifeguard::commands::analyze::run(args),
        Some(Commands::RunTree(args)) => lifeguard::commands::run_tree::run(args),
        Some(Commands::ShowEffects(args)) => lifeguard::commands::show_effects::run(args),
        Some(Commands::GenSourceDb(args)) => lifeguard::commands::gen_source_db::run(args),
        None => lifeguard::commands::analyze::run(args.analyze),
    }
}
