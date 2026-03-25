/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */
#![feature(box_patterns)]

pub mod analyzer;
pub mod bindings;
pub mod builtins;
pub mod class;
pub mod commands;
pub mod cursor;
pub mod debug;
pub mod effects;
pub mod errors;
pub mod exports;
pub mod format;
pub mod graph;
pub mod imports;
pub mod manual_override;
pub mod module_effects;
pub mod module_info;
pub mod module_parser;
pub mod module_safety;
pub mod output;
pub mod project;
pub mod pyrefly;
pub mod runner;
pub mod source_analyzer;
pub mod source_map;
pub mod stub_analyzer;
pub mod stubs;
pub mod test_lib;
pub mod tracing;
pub mod traits;
