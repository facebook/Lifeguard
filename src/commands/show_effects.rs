/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use ruff_python_ast::PySourceType;

use crate::analyzer;
use crate::config::AnalysisConfig;
use crate::debug::print_module_imports_map;
use crate::imports::ImportGraph;
use crate::module_parser;
use crate::pyrefly::module_name::ModuleName;
use crate::runner::DEFAULT_PYTHON_VERSION;
use crate::runner::parse_python_version;
use crate::source_map::ModuleProvider;
use crate::test_lib::TestSources;

#[derive(Parser)]
pub struct ShowEffectsArgs {
    input_file: PathBuf,

    /// Python version to use for parsing
    #[arg(long = "python-version", default_value = DEFAULT_PYTHON_VERSION)]
    python_version: String,
}

pub fn run(args: ShowEffectsArgs) -> Result<()> {
    let module_name = ModuleName::from_str("current_module");
    let path = args.input_file;
    let source = std::fs::read_to_string(&path)?;

    let python_version = parse_python_version(&args.python_version)?;
    let config = AnalysisConfig::with_python_version(python_version, None);

    let sources = TestSources::new(&[("current_module", &source)]);
    let (import_graph, exports, _) = ImportGraph::make_with_exports(&sources, &config);

    let typ = module_parser::file_source_type(&path).unwrap();
    let is_init = path
        .file_name()
        .is_some_and(|f| f == "__init__.py" || f == "__init__.pyi");
    let module = module_parser::parse_file(&source, typ, module_name, is_init, python_version);
    if let Some(err) = module.first_syntax_error() {
        eprintln!(
            "warning: {} has a syntax error, analyzing best-effort: {err}",
            path.display()
        );
    }
    let output = analyzer::analyze(&module, &exports, &import_graph, sources.stubs(), &config);

    // Display output
    let module_effects = output.module_effects;
    let is_stub = typ == PySourceType::Stub;
    module_effects
        .effects
        .pretty_print(&module, &source, !is_stub);

    // Display called imports
    if !module_effects.called_imports.is_empty() {
        println!("\nCalled imports:");
        print_module_imports_map(&module_effects.called_imports);
    }

    // Display pending imports
    if !module_effects.pending_imports.is_empty() {
        println!("\nPending imports:");
        print_module_imports_map(&module_effects.pending_imports);
    }

    Ok(())
}
