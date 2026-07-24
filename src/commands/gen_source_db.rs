/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use serde::Serialize;

use crate::commands::write_json_pretty;
use crate::find_sources::build_source_db;
use crate::runner::DEFAULT_PYTHON_VERSION;
use crate::runner::parse_python_version;

#[derive(Parser)]
pub struct GenSourceDbArgs {
    /// Directory containing Python files to scan
    input_dir: PathBuf,

    /// Path to output JSON file
    output_path: PathBuf,

    /// Path to site-packages directory (overrides pyproject.toml setting)
    #[arg(long)]
    site_packages: Option<PathBuf>,

    /// Python version to use for parsing
    #[arg(long = "python-version", default_value = DEFAULT_PYTHON_VERSION)]
    python_version: String,
}

#[derive(Serialize)]
struct SourceDb {
    build_map: BTreeMap<String, String>,
}

pub fn run(args: GenSourceDbArgs) -> Result<()> {
    let python_version = parse_python_version(&args.python_version)?;
    let (build_map, seed_count) = build_source_db(
        &args.input_dir,
        args.site_packages.as_deref(),
        python_version,
    )?;
    eprintln!(
        "Seeded with {} files from {}",
        seed_count,
        args.input_dir.display()
    );

    let source_db = SourceDb { build_map };
    write_json_pretty(&args.output_path, &source_db)?;

    eprintln!(
        "Wrote {} entries ({} from imports) to {}",
        source_db.build_map.len(),
        source_db.build_map.len() - seed_count,
        args.output_path.display()
    );

    Ok(())
}
