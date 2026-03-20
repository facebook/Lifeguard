/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::ModModule;
use ruff_python_ast::PySourceType;

// Operations on a single module, either .py or .pyi

/// Result of parsing a Python module in string form into an AST.
pub struct ParsedModule {
    pub name: ModuleName,
    pub ast: ModModule,
    pub source_type: PySourceType,
    /// Whether this module came from an `__init__.py` file.
    /// Affects relative import resolution: in `__init__.py`, `.foo` resolves
    /// relative to the current package rather than the parent package.
    pub is_init: bool,
    /// Sorted array of all of the byte positions of line numbers.
    newline_positions: Vec<u32>,
}

impl ParsedModule {
    pub fn is_stub(&self) -> bool {
        self.source_type == PySourceType::Stub
    }

    pub fn byte_to_line_number(&self, pos: u32) -> usize {
        let idx = match self.newline_positions.binary_search(&pos) {
            Ok(n) => n,
            Err(n) => n,
        };

        // Check if we're out-of-bounds which implies it's on the last line, or there's no newlines
        // at all and there's only one line.
        let size = self.newline_positions.len();
        if size == 0 {
            return 1;
        } else if idx >= size {
            return self.newline_positions[size - 1] as usize;
        }

        let newline_pos = self.newline_positions[idx];

        // Add 1 to go from 0-indexed to 1-indexed, then might have to add another to account for
        // how the line is _after_ the newline.
        if pos <= newline_pos { idx + 1 } else { idx + 2 }
    }
}

pub fn file_source_type(path: &Path) -> Option<PySourceType> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => Some(PySourceType::Python),
        Some("pyi") => Some(PySourceType::Stub),
        _ => None,
    }
}

pub fn parse_file(
    source: &str,
    typ: PySourceType,
    name: ModuleName,
    is_init: bool,
) -> ParsedModule {
    let res = ruff_python_parser::parse_unchecked_source(source, typ);
    let newline_positions = compute_newline_positions(source);
    ParsedModule {
        name,
        ast: res.into_syntax(),
        source_type: typ,
        is_init,
        newline_positions,
    }
}

pub fn parse_source(source: &str, module_name: ModuleName, is_init: bool) -> ParsedModule {
    parse_file(source, PySourceType::Python, module_name, is_init)
}

pub fn parse_pyi(source: &str, module_name: ModuleName, is_init: bool) -> ParsedModule {
    parse_file(source, PySourceType::Stub, module_name, is_init)
}

pub fn read_and_parse_source(
    path: &PathBuf,
    module_name: ModuleName,
    is_init: bool,
) -> Result<ParsedModule> {
    // Handle non-utf-8 encodings via lossy conversion.
    let bytes = std::fs::read(path)?;
    let source = String::from_utf8_lossy(&bytes);
    Ok(parse_source(&source, module_name, is_init))
}

/// Given the text contents of a file, compute a sorted array of byte positions where all the
/// line numbers are.
fn compute_newline_positions(source: &str) -> Vec<u32> {
    source
        .chars()
        .enumerate()
        .filter_map(
            |(index, ch)| {
                if ch == '\n' { Some(index as u32) } else { None }
            },
        )
        .collect::<Vec<_>>()
}
