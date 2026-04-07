/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::BufWriter;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use clap::ArgAction;
use clap::Parser;
use walkdir::WalkDir;

use crate::runner::Options;
use crate::runner::process_source_map;
use crate::source_map::ModuleName;
use crate::source_map::SourceMap;
use crate::source_map::SourceResult;
use crate::source_map::is_python_file;
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

    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    print_diagnostics: bool,

    /// Sort output keys and values for deterministic results
    #[arg(long, default_value_t = false, action = ArgAction::SetTrue)]
    sorted_output: bool,
}

/// Returns true if `name` is a valid Python identifier (ASCII subset),
/// i.e. it can appear as a component of a dotted module name.
fn is_valid_python_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        None => false,
        Some(c) if !c.is_ascii_alphabetic() && c != '_' => false,
        _ => chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
    }
}

/// Recursively find all .py files in a directory, skipping directories
/// and files whose names are not valid Python identifiers
/// (e.g. `.venv`, `site-packages`, `2024-07-23-0813_migration.py`).
fn find_python_files(dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return true;
            }
            e.depth() == 0
                || e.file_name()
                    .to_str()
                    .is_some_and(is_valid_python_identifier)
        })
        .filter_map(|e| e.ok())
        .filter(|e| {
            is_python_file(e.path())
                && e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(is_valid_python_identifier)
        })
        .map(|e| e.into_path())
        .collect()
}

/// Build a SourceMap from a directory of Python files.
/// Keys are module names derived from paths relative to the input directory.
/// Values are paths relative to the current directory.
fn build_source_map(input_dir: &Path, cwd: &Path) -> Result<SourceMap> {
    let input_dir = input_dir.canonicalize()?;
    let cwd = cwd.canonicalize()?;

    let py_files = find_python_files(&input_dir);

    let mut source_map = SourceMap::default();
    for file_path in py_files {
        // Module name derived from path relative to input directory
        let rel_to_input = file_path
            .strip_prefix(&input_dir)
            .expect("file should be under input_dir");
        let mod_name = match ModuleName::from_relative_path(rel_to_input) {
            Ok(name) => name,
            Err(_) => continue,
        };

        // Value: path relative to current working directory
        let rel_to_cwd = if file_path.starts_with(&cwd) {
            file_path
                .strip_prefix(&cwd)
                .expect("file should be under cwd")
                .to_path_buf()
        } else {
            // If file is not under cwd, use absolute path
            file_path.clone()
        };

        source_map.insert(mod_name, SourceResult::Ok(rel_to_cwd));
    }

    Ok(source_map)
}

pub fn run(args: RunTreeArgs) -> Result<()> {
    let timer = ProcessTimer::new();
    let cwd = std::env::current_dir()?;

    // Build source map from directory
    let source_map = time("Building source map", || {
        build_source_map(&args.input_dir, &cwd)
    })?;
    println!("Found {} Python files", source_map.len());

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
