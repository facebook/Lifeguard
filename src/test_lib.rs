/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::process::Command;

use ahash::AHashMap;
use ahash::AHashSet;
use itertools::Itertools;
use pyrefly_python::module::Module;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use tempfile::TempDir;

use crate::analyzer::analyze;
use crate::effects::Effect;
use crate::errors::SafetyError;
use crate::exports::Exports;
use crate::format::ErrorString;
use crate::imports::ImportGraph;
use crate::module_effects::ModuleEffects;
use crate::module_parser::ParsedModule;
use crate::module_parser::parse_pyi;
use crate::module_parser::parse_source;
use crate::module_safety::ModuleSafety;
use crate::project;
use crate::project::AnalysisMap;
use crate::project::SafetyMap;
use crate::pyrefly::sys_info::SysInfo;
use crate::source_map::AstResult;
use crate::source_map::ModuleProvider;
use crate::stubs::Stubs;
use crate::traits::AsStr;
use crate::traits::ModuleExt;
use crate::traits::SysInfoExt;

// ---------------------------------------------------------------------------
// TestSources: in-memory ModuleProvider for tests
// ---------------------------------------------------------------------------

/// Test implementation of ModuleProvider: wraps in-memory code strings + Stubs.
/// Parses modules from strings on demand.
pub struct TestSources {
    modules: HashMap<ModuleName, String, ahash::RandomState>,
    stub_modules: AHashSet<ModuleName>,
    stubs: Stubs,
    names: Vec<ModuleName>,
}

impl TestSources {
    pub fn new(modules: &[(&str, &str)]) -> Self {
        Self::new_impl(modules, &[])
    }

    pub fn new_with_stubs(modules: &[(&str, &str)], stub_names: &[&str]) -> Self {
        Self::new_impl(modules, stub_names)
    }

    fn new_impl(modules: &[(&str, &str)], stub_names: &[&str]) -> Self {
        let stubs = Stubs::new();

        // Collect all names: stubs first, then test modules (test modules override stubs)
        let mut name_set: AHashSet<ModuleName> =
            stubs.raw_sources_iter().map(|(name, _)| *name).collect();

        let mut module_map = HashMap::<ModuleName, String, ahash::RandomState>::default();
        for (name, code) in modules {
            let mod_name = ModuleName::from_str(name);
            name_set.insert(mod_name);
            module_map.insert(mod_name, code.to_string());
        }

        let stub_modules: AHashSet<ModuleName> =
            stub_names.iter().map(|n| ModuleName::from_str(n)).collect();

        let names: Vec<ModuleName> = name_set.into_iter().collect();

        Self {
            modules: module_map,
            stub_modules,
            stubs,
            names,
        }
    }

    pub fn get_code(&self, name: &ModuleName) -> Option<&str> {
        self.modules.get(name).map(|s| s.as_str())
    }
}

impl ModuleProvider for TestSources {
    fn module_names_iter(&self) -> impl Iterator<Item = &ModuleName> {
        self.names.iter()
    }

    fn module_names_par_iter(&self) -> impl ParallelIterator<Item = &ModuleName> {
        self.names.par_iter()
    }

    fn len(&self) -> usize {
        self.names.len()
    }

    fn parse(&self, name: &ModuleName) -> Option<AstResult> {
        // Test modules take priority over stubs
        if let Some(code) = self.modules.get(name) {
            if self.stub_modules.contains(name) {
                return Some(AstResult::Ok(parse_pyi(code, *name, false)));
            }
            // A module is an __init__.py (package) if any other module is a child of it
            let name_prefix = format!("{}.", name.as_str());
            let is_init = self
                .names
                .iter()
                .any(|n| n.as_str().starts_with(&name_prefix));
            return Some(AstResult::Ok(parse_source(code, *name, is_init)));
        }

        // Fall back to stubs
        if let Some(src) = self.stubs.get_raw_source(name) {
            return Some(AstResult::Ok(parse_pyi(src, *name, false)));
        }

        None
    }

    fn is_stub(&self, name: &ModuleName) -> bool {
        if self.stub_modules.contains(name) {
            return true;
        }
        // A module is a stub only if it comes from stubs and is NOT overridden by a test module
        !self.modules.contains_key(name) && self.stubs.get_raw_source(name).is_some()
    }

    fn overrides_source(&self, name: &ModuleName) -> bool {
        self.stub_modules.contains(name) && self.modules.contains_key(name)
    }

    fn stubs(&self) -> &Stubs {
        &self.stubs
    }
}

// ---------------------------------------------------------------------------
// Test expectation infrastructure
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExpectedError {
    line_no: usize,
    error: String,
}

#[derive(Clone, Debug)]
pub struct Expectation {
    errors: Vec<ExpectedError>,
}

impl Expectation {
    fn parse_line(&mut self, line_no: usize, mut s: &str) {
        while let Some((prefix, err)) = s.trim().rsplit_once("# E:") {
            self.errors.push(ExpectedError {
                line_no,
                error: err.trim().to_owned(),
            });
            s = prefix.trim_end();
        }
    }

    pub fn parse(s: &str) -> Self {
        let mut res = Self { errors: Vec::new() };
        for (line_no, line) in s.lines().enumerate() {
            res.parse_line(line_no + 1, line)
        }
        res
    }
}

trait ToExpected {
    fn to_expected(&self, mi: &Module) -> ExpectedError;
}

impl ToExpected for SafetyError {
    fn to_expected(&self, mi: &Module) -> ExpectedError {
        ExpectedError {
            line_no: mi.get_line_no(self.range.start()),
            error: self.kind.error_string(),
        }
    }
}

impl ToExpected for Effect {
    fn to_expected(&self, mi: &Module) -> ExpectedError {
        ExpectedError {
            line_no: mi.get_line_no(self.range.start()),
            error: self.kind.error_string(),
        }
    }
}

enum Check {
    Errors,
    Effects,
}

// Parse code for expected error strings, and compare them to the actual results.
// Handles both errors and effects.
fn check_output(
    modules: Vec<(&str, &str)>,
    check: Check,
    implicit_imports: Option<Vec<(&str, Vec<&str>)>>,
) {
    let sources = TestSources::new(&modules);
    let sys_info = SysInfo::lg_default();
    let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &sys_info);

    let safety_map = match check {
        Check::Errors => {
            project::run_analysis(
                &sources,
                &exports,
                &import_graph,
                &sys_info,
                project::CachingMode::Disabled,
            )
            .safety_map
        }
        _ => SafetyMap::new(),
    };
    let effect_map = match check {
        Check::Effects => project::analyze_all(&sources, &exports, &import_graph, &sys_info).0,
        _ => HashMap::<_, _, ahash::RandomState>::default(),
    };

    for (module_name_str, code) in &modules {
        let module_name = ModuleName::from_str(module_name_str);
        let module_info = Module::make(module_name_str, code);

        let exp = Expectation::parse(code);
        let expected_errs: AHashSet<&ExpectedError> = exp.errors.iter().collect();

        let errs = if matches!(check, Check::Errors) {
            let safety_ref = safety_map.get(&module_name).unwrap();
            let module_safety = safety_ref.as_safety().expect("Failed to get module safety");
            if let Some(ref implicit_imports) = implicit_imports {
                check_implicit_imports(
                    &module_name,
                    implicit_imports.to_vec(),
                    module_safety.implicit_imports.clone().into_iter().collect(),
                );
            }
            get_safety_errors(module_safety, &module_info)
        } else {
            let module_analysis = effect_map.get(&module_name).unwrap();
            get_effects(&module_analysis.module_effects, &module_info)
        };
        let actual_errs: AHashSet<&ExpectedError> = errs.iter().collect();

        // Take both set differences, {actual} - {expected} and {expected} - {actual}
        let not_asserted: Vec<_> = actual_errs.difference(&expected_errs).collect();
        let not_raised: Vec<_> = expected_errs.difference(&actual_errs).collect();
        assert!(
            not_asserted.is_empty() && not_raised.is_empty(),
            "Not asserted: {:?}\nNot raised: {:?}",
            not_asserted,
            not_raised
        );
    }
}

fn get_safety_errors(sft: &ModuleSafety, mi: &Module) -> Vec<ExpectedError> {
    let err = sft.errors.iter().map(|e| e.to_expected(mi));
    let excl = sft
        .force_imports_eager_overrides
        .iter()
        .map(|e| e.to_expected(mi));
    err.chain(excl).collect()
}

fn get_effects(effs: &ModuleEffects, mi: &Module) -> Vec<ExpectedError> {
    effs.effects
        .values()
        .flatten()
        .map(|e| e.to_expected(mi))
        .collect()
}

pub fn check(code: &str) {
    check_output(vec![("test", code)], Check::Errors, None);
}

pub fn check_all(modules: Vec<(&str, &str)>) {
    check_output(modules, Check::Errors, None);
}

pub fn check_errors_and_implicit_imports(
    modules: Vec<(&str, &str)>,
    implicit_imports: Vec<(&str, Vec<&str>)>,
) {
    check_output(modules, Check::Errors, Some(implicit_imports));
}

pub fn check_effects(code: &str) {
    check_output(vec![("test", code)], Check::Effects, None);
}

pub fn check_all_effects(modules: Vec<(&str, &str)>) {
    check_output(modules, Check::Effects, None);
}

pub fn check_implicit_imports(
    module_name: &ModuleName,
    expected_implicit_imports_str_map: Vec<(&str, Vec<&str>)>,
    actual_implicit_imports: AHashSet<ModuleName>,
) {
    let expected_implicit_imports_map: AHashMap<ModuleName, AHashSet<ModuleName>> =
        expected_implicit_imports_str_map
            .into_iter()
            .map(|(k, v)| {
                (
                    ModuleName::from_str(k),
                    module_names(v).into_iter().collect(),
                )
            })
            .collect();
    assert_eq!(
        *actual_implicit_imports,
        expected_implicit_imports_map
            .get(module_name)
            .unwrap_or(&AHashSet::new())
            .clone()
            .into()
    );
}

pub fn check_imports(
    module_effects: ModuleEffects,
    pending_imports: Vec<(&str, Vec<&str>)>,
    called_imports: Vec<(&str, Vec<&str>)>,
) {
    let mut expected_pending_imports: Vec<(ModuleName, Vec<ModuleName>)> = module_effects
        .pending_imports
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .sorted()
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    expected_pending_imports.sort_by(|a, b| a.0.cmp(&b.0));

    let mut expected_called_imports: Vec<(ModuleName, Vec<ModuleName>)> = module_effects
        .called_imports
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .sorted()
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    expected_called_imports.sort_by(|a, b| a.0.cmp(&b.0));

    let called_imports_as_module_names: Vec<(ModuleName, Vec<ModuleName>)> = called_imports
        .iter()
        .map(|(k, v)| {
            (
                ModuleName::from_str(k),
                v.iter().map(|s| ModuleName::from_str(s)).collect(),
            )
        })
        .collect();

    let pending_imports_as_module_names: Vec<(ModuleName, Vec<ModuleName>)> = pending_imports
        .iter()
        .map(|(k, v)| {
            (
                ModuleName::from_str(k),
                v.iter().map(|s| ModuleName::from_str(s)).collect(),
            )
        })
        .collect();

    assert_eq!(pending_imports_as_module_names, expected_pending_imports);
    assert_eq!(called_imports_as_module_names, expected_called_imports);
}

/// Run analysis on a parsed module.
pub fn run_module_analysis(code: &str, parsed_module: &ParsedModule) -> ModuleEffects {
    let exports = Exports::empty();
    let sys_info = SysInfo::lg_default();
    let sources = TestSources::new(&[(parsed_module.name.as_str(), code)]);
    let import_graph = ImportGraph::make(&sources, &sys_info);
    let stubs = sources.stubs();
    analyze(parsed_module, &exports, &import_graph, stubs, &sys_info).module_effects
}

pub fn module_names(names: Vec<&str>) -> Vec<ModuleName> {
    names.iter().map(|s| ModuleName::from_str(s)).collect()
}

// Compares a collection of items that implement .as_str() with a vector of expected strings.
// Uses the name str_keys() to indicate that the strings are expected to be unique.
pub fn assert_str_keys<'a, I, T>(actual: I, expected: Vec<&str>)
where
    I: IntoIterator<Item = &'a T>,
    T: AsStr + 'a,
{
    let a: HashSet<&str> = actual.into_iter().map(|k| k.as_str()).collect();
    let e: HashSet<&str> = expected.into_iter().collect();
    let extra: Vec<_> = a.difference(&e).collect();
    let missing: Vec<_> = e.difference(&a).collect();
    assert!(
        extra.is_empty() && missing.is_empty(),
        "Extra: {:?}\nMissing: {:?}",
        extra,
        missing
    );
}

/// Run the analysis pipeline on a set of modules and return the per-module analysis results.
/// Input is a vector of (module_name, code) pairs.
pub fn analyze_tree(modules: &Vec<(&str, &str)>) -> AnalysisMap {
    let sources = TestSources::new(modules);
    let sys_info = SysInfo::lg_default();
    let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &sys_info);
    project::analyze_all(&sources, &exports, &import_graph, &sys_info).0
}

pub fn check_buck_availability() -> bool {
    if Command::new("buck2").output().is_err() {
        eprintln!("buck2 not available");
        return false;
    }
    // Also check we're inside a Buck project. buck2 may be on PATH
    // even when running from the OSS checkout.
    let root = Command::new("buck2").args(["root"]).output();
    match root {
        Ok(o) if o.status.success() => true,
        _ => {
            eprintln!("not in a Buck project, skipping");
            false
        }
    }
}

/// Create a new temp directory and write each `(rel_path, contents)` pair
/// into it, creating intermediate directories as needed. The returned
/// [`TempDir`] owns the path and deletes it on drop.
pub fn populate_temp_dir(files: &[(&str, &str)]) -> TempDir {
    let tmp = TempDir::new().expect("create temp dir");
    for (rel, contents) in files {
        let path = tmp.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&path, contents).expect("write file");
    }
    tmp
}
