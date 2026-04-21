/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::io::BufWriter;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use serde::Serialize;

use crate::find_sources::build_source_db;

#[derive(Parser)]
pub struct GenSourceDbArgs {
    /// Directory containing Python files to scan
    input_dir: PathBuf,

    /// Path to output JSON file
    output_path: PathBuf,

    /// Path to site-packages directory (overrides pyproject.toml setting)
    #[arg(long)]
    site_packages: Option<PathBuf>,
}

#[derive(Serialize)]
struct SourceDb {
    build_map: BTreeMap<String, String>,
}

pub fn run(args: GenSourceDbArgs) -> Result<()> {
    let (build_map, seed_count) = build_source_db(&args.input_dir, args.site_packages.as_deref())?;
    eprintln!(
        "Seeded with {} files from {}",
        seed_count,
        args.input_dir.display()
    );

    let source_db = SourceDb { build_map };
    let output_file = std::fs::File::create(&args.output_path)?;
    let mut writer = BufWriter::new(output_file);
    serde_json::to_writer_pretty(&mut writer, &source_db)?;
    writer.flush()?;

    eprintln!(
        "Wrote {} entries ({} from imports) to {}",
        source_db.build_map.len(),
        source_db.build_map.len() - seed_count,
        args.output_path.display()
    );

    Ok(())
}
