/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

pub mod analyze;
pub mod analyze_binary;
pub mod analyze_library;
pub mod compare_paths;
pub mod gen_source_db;
pub mod run_tree;
pub mod show_effects;

/// Serialize `value` as pretty JSON to `path` through a buffered writer.
pub fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    serde_json::to_writer_pretty(&mut writer, value)?;
    Ok(writer.flush()?)
}

/// Serialize `value` as compact JSON to `path` through a buffered writer.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.flush()?)
}
