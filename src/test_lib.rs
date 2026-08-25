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
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use pyrefly_python::module::Module;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use tempfile::TempDir;

use crate::analyzer::analyze;
use crate::config::AnalysisConfig;
use crate::effects::Effect;
use crate::errors::SafetyError;
use crate::exports::Exports;
use crate::format::ErrorString;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::module_effects::ModuleEffects;
use crate::module_parser::ParsedModule;
use crate::module_parser::parse_pyi_with_version;
use crate::module_parser::parse_source_with_version;
use crate::module_safety::ModuleSafety;
use crate::module_safety::SafetyResult;
use crate::output::LifeGuardAnalysis;
use crate::project;
use crate::project::AnalysisMap;
use crate::project::AnalysisOutput;
use crate::project::SafetyMap;
use crate::pyrefly::sys_info::PythonVersion;
use crate::runner::Options;
use crate::source_map::AstResult;
use crate::source_map::ModuleProvider;
use crate::source_map::bundled_stub_sources;
use crate::stubs::Stubs;
use crate::traits::AsStr;
use crate::traits::ModuleExt;

// ---------------------------------------------------------------------------
// Shared stub state
// ---------------------------------------------------------------------------

/// Strip the indentation common to every line, so a snippet can be indented to
/// line up with the surrounding Rust without being a Python `IndentationError`.
///
/// Line count is preserved, so error line numbers are unaffected.
pub fn dedent(code: &str) -> String {
    let indent = code
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    if indent == 0 {
        return code.to_owned();
    }
    itertools::Itertools::intersperse(
        code.lines().map(|line| line.get(indent..).unwrap_or("")),
        "\n",
    )
    .chain(code.ends_with('\n').then_some("\n"))
    .collect()
}

/// The bundled stubs, decompressed once per process and shared by every
/// [`TestSources`]. `Stubs` memoizes each stub's analysis internally, so
/// sharing one instance also shares that work across tests.
pub fn shared_stubs() -> &'static Stubs {
    static STUBS: OnceLock<Stubs> = OnceLock::new();
    STUBS.get_or_init(Stubs::new)
}

/// The import graph over the bundled stubs, built once per Python version.
///
/// Keyed on the version because stubs guard imports on `sys.version_info`, so
/// the edges genuinely differ: at 3.15 `typing` imports `annotationlib` and
/// `tarfile` imports `compression.zstd`, neither of which exist at 3.13.
/// Sharing one default-version graph would hide those edges from a test built
/// with [`TestSources::new_with_version`], and the stub would be treated as a
/// missing import rather than resolved.
fn stub_import_graph(python_version: PythonVersion) -> Arc<ImportGraph> {
    type Cell = Arc<OnceLock<Arc<ImportGraph>>>;
    static GRAPHS: OnceLock<Mutex<HashMap<PythonVersion, Cell>>> = OnceLock::new();

    // The map lock only guards the lookup. Building under it would block a
    // thread wanting a different version, and a panic mid-build would poison
    // the cache for every later test in the process.
    let cell: Cell = {
        let graphs = GRAPHS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut graphs = graphs.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(graphs.entry(python_version).or_default())
    };

    // Threads racing for the same version block here rather than each building
    // a graph and discarding all but one.
    cell.get_or_init(|| {
        Arc::new(ImportGraph::make(
            &bundled_stub_sources(python_version),
            &AnalysisConfig::with_python_version(python_version, None),
        ))
    })
    .clone()
}

/// Every stub reachable from `roots`, following stub-to-stub imports and
/// including the ancestor packages that missing-import resolution walks up to.
/// `leaves` are admitted without following their imports.
///
/// Tests only need the stubs their own modules can actually reach. Admitting
/// all ~950 bundled stubs instead makes every test parse, build exports for,
/// and analyze the whole of typeshed before discarding the results.
fn reachable_stubs(
    roots: impl Iterator<Item = ModuleName>,
    leaves: impl Iterator<Item = ModuleName>,
    python_version: PythonVersion,
) -> AHashSet<ModuleName> {
    fn admit(name: ModuleName, seen: &mut AHashSet<ModuleName>, queue: &mut Vec<ModuleName>) {
        if shared_stubs().get_raw_source(&name).is_some() && seen.insert(name) {
            queue.push(name);
        }
    }

    /// A dotted import can name a submodule, but resolution may fall back to
    /// any of its ancestor packages, so admit the whole prefix chain.
    fn admit_with_ancestors(
        name: ModuleName,
        seen: &mut AHashSet<ModuleName>,
        queue: &mut Vec<ModuleName>,
    ) {
        let parts: Vec<&str> = name.as_str().split('.').collect();
        for i in 1..=parts.len() {
            admit(ModuleName::from_str(&parts[..i].join(".")), seen, queue);
        }
    }

    let mut seen = AHashSet::new();
    let mut queue: Vec<ModuleName> = Vec::new();

    for root in roots {
        admit_with_ancestors(root, &mut seen, &mut queue);
    }

    let graph = stub_import_graph(python_version);
    while let Some(name) = queue.pop() {
        for import in graph.get_imports(&name) {
            admit(*import, &mut seen, &mut queue);
        }
    }

    // Discarding the queue is what keeps a leaf's imports out of the result.
    let mut unvisited = Vec::new();
    for leaf in leaves {
        admit_with_ancestors(leaf, &mut seen, &mut unvisited);
    }
    seen
}

// ---------------------------------------------------------------------------
// TestSources: in-memory ModuleProvider for tests
// ---------------------------------------------------------------------------

/// Test implementation of ModuleProvider: wraps in-memory code strings + Stubs.
/// Parses modules from strings on demand.
pub struct TestSources {
    modules: HashMap<ModuleName, String, ahash::RandomState>,
    stub_modules: AHashSet<ModuleName>,
    parse_errors: AHashSet<ModuleName>,
    names: Vec<ModuleName>,
    python_version: PythonVersion,
}

impl TestSources {
    pub fn new(modules: &[(&str, &str)]) -> Self {
        Self::new_impl(modules, &[], PythonVersion::default())
    }

    pub fn new_with_version(modules: &[(&str, &str)], python_version: PythonVersion) -> Self {
        Self::new_impl(modules, &[], python_version)
    }

    pub fn new_with_stubs(modules: &[(&str, &str)], stub_names: &[&str]) -> Self {
        Self::new_impl(modules, stub_names, PythonVersion::default())
    }

    fn new_impl(
        modules: &[(&str, &str)],
        stub_names: &[&str],
        python_version: PythonVersion,
    ) -> Self {
        let mut module_map = HashMap::<ModuleName, String, ahash::RandomState>::default();
        let mut names = Vec::new();
        for (name, code) in modules {
            let mod_name = ModuleName::from_str(name);
            if module_map.insert(mod_name, dedent(code)).is_none() {
                names.push(mod_name);
            }
        }

        let stub_modules: AHashSet<ModuleName> =
            stub_names.iter().map(|n| ModuleName::from_str(n)).collect();

        let mut sources = Self {
            modules: module_map,
            stub_modules,
            parse_errors: AHashSet::new(),
            names,
            python_version,
        };
        sources.names.extend(sources.referenced_stubs());
        sources
    }

    /// The stubs the test modules can reach. Resolved by graphing the test
    /// modules on their own: every import that fails to resolve against them is
    /// a candidate stub name.
    fn referenced_stubs(&self) -> AHashSet<ModuleName> {
        let config = AnalysisConfig::with_python_version(self.python_version, None);
        let graph = ImportGraph::make(self, &config);
        let unresolved = self.names.iter().flat_map(|name| {
            let missing = graph.get_missing_imports(name).into_iter();
            let ambiguous = graph.get_ambiguous_imports(name).into_iter();
            missing.chain(ambiguous).flatten().copied()
        });
        // `builtins` is in scope for every module without being imported, but
        // its own imports are served by `Stubs`, which resolves independently
        // of the module graph.
        let implicit = std::iter::once(ModuleName::builtins());
        reachable_stubs(unresolved, implicit, self.python_version)
    }

    pub fn with_parse_errors(mut self, error_modules: &[&str]) -> Self {
        for name in error_modules {
            let mod_name = ModuleName::from_str(name);
            if !self.names.contains(&mod_name) {
                self.names.push(mod_name);
            }
            self.parse_errors.insert(mod_name);
        }
        self
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
        if self.parse_errors.contains(name) {
            return Some(AstResult::ParserError(anyhow::anyhow!("parse error")));
        }

        // Test modules take priority over stubs
        if let Some(code) = self.modules.get(name) {
            if self.stub_modules.contains(name) {
                return Some(AstResult::Ok(parse_pyi_with_version(
                    code,
                    *name,
                    false,
                    self.python_version,
                )));
            }
            // A module is an __init__.py (package) if any other module is a child of it
            let name_prefix = format!("{}.", name.as_str());
            let is_init = self
                .names
                .iter()
                .any(|n| n.as_str().starts_with(&name_prefix));
            let parsed = parse_source_with_version(code, *name, is_init, self.python_version);
            // Ruff recovers from syntax errors and the production read path
            // treats a broken file as missing, so a malformed snippet would
            // otherwise be analyzed as a best-effort AST and quietly assert
            // against behaviour real code never reaches. Use `with_parse_errors`
            // to exercise unparseable modules on purpose.
            assert!(
                parsed.first_syntax_error().is_none(),
                "test snippet for module `{}` does not parse: {}",
                name.as_str(),
                parsed.first_syntax_error().unwrap_or_default(),
            );
            return Some(AstResult::Ok(parsed));
        }

        // Fall back to stubs
        if let Some(src) = shared_stubs().get_raw_source(name) {
            return Some(AstResult::Ok(parse_pyi_with_version(
                src,
                *name,
                shared_stubs().is_init(name),
                self.python_version,
            )));
        }

        None
    }

    fn is_stub(&self, name: &ModuleName) -> bool {
        if self.stub_modules.contains(name) {
            return true;
        }
        // A module is a stub only if it comes from stubs and is NOT overridden by a test module
        !self.modules.contains_key(name) && shared_stubs().get_raw_source(name).is_some()
    }

    fn overrides_source(&self, name: &ModuleName) -> bool {
        self.stub_modules.contains(name) && self.modules.contains_key(name)
    }

    fn stubs(&self) -> &Stubs {
        shared_stubs()
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
    check_output_with_config(modules, check, implicit_imports, AnalysisConfig::default());
}

fn check_output_with_config(
    modules: Vec<(&str, &str)>,
    check: Check,
    implicit_imports: Option<Vec<(&str, Vec<&str>)>>,
    config: AnalysisConfig,
) {
    if let Some(mismatch) = output_mismatch(modules, check, implicit_imports, config) {
        panic!("{mismatch}");
    }
}

/// How the analysis differs from the modules' annotations, or `None` when they
/// agree. Split out from [`check_output_with_config`] so a test over a table of
/// snippets can report every row that fails rather than only the first.
fn output_mismatch(
    modules: Vec<(&str, &str)>,
    check: Check,
    implicit_imports: Option<Vec<(&str, Vec<&str>)>>,
    config: AnalysisConfig,
) -> Option<String> {
    let sources = TestSources::new(&modules);
    let (import_graph, exports, in_scope) = ImportGraph::make_with_exports(&sources, &config);

    let safety_map = match check {
        Check::Errors => {
            project::run_analysis(
                &sources,
                &exports,
                &import_graph,
                &config,
                project::ExecutionMode::WholeProgram,
                &in_scope,
            )
            .safety_map
        }
        _ => SafetyMap::new(),
    };
    let effect_map = match check {
        Check::Effects => {
            project::analyze_all(&sources, &exports, &import_graph, &config, &in_scope).0
        }
        _ => HashMap::<_, _, ahash::RandomState>::default(),
    };

    for (module_name_str, code) in &modules {
        let module_name = ModuleName::from_str(module_name_str);
        // Must match what TestSources analyzed, or the reported byte offsets
        // resolve against the wrong text.
        let code = &dedent(code);
        let module_info = Module::make(module_name_str, code);

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
        if let Some(mismatch) = expectation_mismatch(code, &errs) {
            return Some(mismatch);
        }
    }
    None
}

/// Compare what the analysis found in a module against the expectations
/// annotated in its source.
fn assert_expectations(code: &str, errs: &[ExpectedError]) {
    if let Some(mismatch) = expectation_mismatch(code, errs) {
        panic!("{mismatch}");
    }
}

/// How the analysis differs from the code's annotations, or `None` when they agree.
fn expectation_mismatch(code: &str, errs: &[ExpectedError]) -> Option<String> {
    let exp = Expectation::parse(code);
    let expected_errs: AHashSet<&ExpectedError> = exp.errors.iter().collect();
    let actual_errs: AHashSet<&ExpectedError> = errs.iter().collect();

    // Take both set differences, {actual} - {expected} and {expected} - {actual}
    let not_asserted: Vec<_> = actual_errs.difference(&expected_errs).collect();
    let not_raised: Vec<_> = expected_errs.difference(&actual_errs).collect();
    (!not_asserted.is_empty() || !not_raised.is_empty())
        .then(|| format!("Not asserted: {not_asserted:?}\nNot raised: {not_raised:?}"))
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

/// [`check_all`], with `stub_names` analyzed as `.pyi` stubs. Expectations are
/// only read from the other modules: a stub declares effects rather than
/// executing them, so annotating errors in one is meaningless.
pub fn check_all_with_stubs(modules: Vec<(&str, &str)>, stub_names: &[&str]) {
    let sources = TestSources::new_with_stubs(&modules, stub_names);
    let safety_map = run_analysis_on(&sources).0.safety_map;
    for (name, code) in modules.iter().filter(|(n, _)| !stub_names.contains(n)) {
        let code = &dedent(code);
        let module_info = Module::make(name, code);
        let safety_ref = safety_map.get(&ModuleName::from_str(name)).unwrap();
        let module_safety = safety_ref.as_safety().expect("Failed to get module safety");
        assert_expectations(code, &get_safety_errors(module_safety, &module_info));
    }
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

/// [`check_effects`], reporting the mismatch instead of panicking.
pub fn effects_mismatch(code: &str) -> Option<String> {
    output_mismatch(
        vec![("test", code)],
        Check::Effects,
        None,
        AnalysisConfig::default(),
    )
}

pub fn check_effects_as_main(code: &str) {
    let config = AnalysisConfig {
        main_module: Some(ModuleName::from_str("test")),
        ..AnalysisConfig::default()
    };
    check_output_with_config(vec![("test", code)], Check::Effects, None, config);
}

pub fn check_effects_not_main(code: &str) {
    let config = AnalysisConfig {
        main_module: Some(ModuleName::from_str("__other__")),
        ..AnalysisConfig::default()
    };
    check_output_with_config(vec![("test", code)], Check::Effects, None, config);
}

pub fn check_effects_no_main(code: &str) {
    let config = AnalysisConfig {
        main_module: Some(ModuleName::from_str("")),
        ..AnalysisConfig::default()
    };
    check_output_with_config(vec![("test", code)], Check::Effects, None, config);
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
        actual_implicit_imports,
        expected_implicit_imports_map
            .get(module_name)
            .unwrap_or(&AHashSet::new())
            .clone()
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
            let mut imports = v.iter().cloned().collect::<Vec<_>>();
            imports.sort();
            (k.clone(), imports)
        })
        .collect();
    expected_pending_imports.sort_by_key(|a| a.0);

    let mut expected_called_imports: Vec<(ModuleName, Vec<ModuleName>)> = module_effects
        .called_imports
        .iter()
        .map(|(k, v)| {
            let mut imports = v.iter().cloned().collect::<Vec<_>>();
            imports.sort();
            (k.clone(), imports)
        })
        .collect();
    expected_called_imports.sort_by_key(|a| a.0);

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
    let config = AnalysisConfig::default();
    let sources = TestSources::new(&[(parsed_module.name.as_str(), code)]);
    let import_graph = ImportGraph::make(&sources, &config);
    let stubs = sources.stubs();
    analyze(parsed_module, &exports, &import_graph, stubs, &config).module_effects
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
    let config = AnalysisConfig::default();
    let (import_graph, exports, in_scope) = ImportGraph::make_with_exports(&sources, &config);
    project::analyze_all(&sources, &exports, &import_graph, &config, &in_scope).0
}

/// Build the import graph for a set of modules.
pub fn build_import_graph(modules: &Vec<(&str, &str)>) -> ImportGraph {
    let sources = TestSources::new(modules);
    ImportGraph::make(&sources, &AnalysisConfig::default())
}

/// Sorted output keeps assertions on the generated JSON stable.
pub fn test_options() -> Options {
    Options {
        sorted_output: true,
        ..Options::default()
    }
}

/// [`test_options`] plus verbose output, which is what makes
/// [`LifeGuardAnalysis`] report `import_cycles`.
pub fn verbose_test_options() -> Options {
    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    Options {
        verbose_output_path: Some(
            std::env::temp_dir().join(format!("lifeguard_test_{}_{id}", std::process::id())),
        ),
        ..test_options()
    }
}

/// Run the whole-program pipeline over `sources`, returning the analysis output
/// alongside the import graph and exports it was computed from. Use this
/// directly when the test needs to build its own [`TestSources`], for example
/// to inject parse errors.
pub fn run_analysis_on(sources: &TestSources) -> (AnalysisOutput, ImportGraph, Exports) {
    let config = AnalysisConfig::default();
    let (import_graph, exports, in_scope) = ImportGraph::make_with_exports(sources, &config);
    let output = project::run_analysis(
        sources,
        &exports,
        &import_graph,
        &config,
        project::ExecutionMode::WholeProgram,
        &in_scope,
    );
    (output, import_graph, exports)
}

/// Run the whole pipeline and build the final analysis, the way `main` does.
pub fn run_lifeguard_analysis(modules: &Vec<(&str, &str)>) -> LifeGuardAnalysis {
    run_lifeguard_analysis_with(modules, &test_options())
}

pub fn run_lifeguard_analysis_with(
    modules: &Vec<(&str, &str)>,
    options: &Options,
) -> LifeGuardAnalysis {
    run_lifeguard_analysis_on(&TestSources::new(modules), options)
}

pub fn run_lifeguard_analysis_on(sources: &TestSources, options: &Options) -> LifeGuardAnalysis {
    let (output, import_graph, exports) = run_analysis_on(sources);
    for entry in output.parse_errors.iter() {
        output.safety_map.insert(
            *entry.key(),
            SafetyResult::AnalysisError(anyhow::anyhow!("Parse error: {}", entry.value())),
        );
    }
    let mut analysis = LifeGuardAnalysis::new(output.safety_map, import_graph, &exports, options);
    analysis.propagate_side_effect_imports(&output.side_effect_imports);
    analysis
}

pub fn assert_passing(result: &LifeGuardAnalysis, expected: Vec<&str>) {
    assert_str_keys(&result.passing_modules, expected);
}

pub fn assert_failing(result: &LifeGuardAnalysis, expected: Vec<&str>) {
    assert_str_keys(&result.failing_modules, expected);
}

/// Whether `module` is lazy-eligible but must load `dep` eagerly.
pub fn has_lazy_eligible_dep(result: &LifeGuardAnalysis, module: &str, dep: &str) -> bool {
    result
        .output
        .lazy_eligible
        .get(&ModuleName::from_str(module))
        .is_some_and(|deps| deps.contains(&ModuleName::from_str(dep)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedent_strips_common_indentation() {
        assert_eq!(
            dedent("\n    import os\n    x = 1\n"),
            "\nimport os\nx = 1\n"
        );
    }

    #[test]
    fn test_dedent_keeps_relative_indentation() {
        assert_eq!(
            dedent("\n    def f():\n        return 1\n"),
            "\ndef f():\n    return 1\n"
        );
    }

    #[test]
    fn test_dedent_preserves_line_count() {
        let code = "\n    import os\n\n    x = 1\n";
        assert_eq!(dedent(code).lines().count(), code.lines().count());
    }

    #[test]
    fn test_dedent_leaves_flush_left_code_alone() {
        let code = "import os\nx = 1\n";
        assert_eq!(dedent(code), code);
    }

    #[test]
    fn test_stubs_are_shared_across_sources() {
        let a = TestSources::new(&[("a", "x = 1")]);
        let b = TestSources::new(&[("b", "y = 2")]);
        assert!(
            std::ptr::eq(a.stubs(), b.stubs()),
            "every TestSources should borrow the same process-wide Stubs; \
             constructing one per instance re-decompresses the whole bundle"
        );
    }

    #[test]
    fn test_only_reachable_stubs_are_admitted() {
        let bundled = shared_stubs().raw_sources_iter().count();
        let sources = TestSources::new(&[("a", "x = 1")]);
        assert!(
            sources.len() < bundled / 10,
            "a module that imports nothing pulled in {} of {} bundled stubs",
            sources.len(),
            bundled
        );
    }

    #[test]
    fn test_duplicate_module_names_are_admitted_once() {
        let sources = TestSources::new(&[("a", "x = 1"), ("a", "x = 2")]);
        assert_eq!(
            sources
                .module_names_iter()
                .filter(|name| name.as_str() == "a")
                .count(),
            1
        );
        assert_eq!(sources.get_code(&ModuleName::from_str("a")), Some("x = 2"));
    }

    #[test]
    fn test_closure_follows_version_guarded_stub_imports() {
        // `typing` imports `annotationlib` only under `sys.version_info >= 3.14`.
        // A version-blind stub graph would drop that edge and leave the test
        // resolving `annotationlib` as a missing import.
        let names = |version| {
            let sources = TestSources::new_with_version(&[("a", "import typing")], version);
            sources
                .module_names_iter()
                .copied()
                .collect::<AHashSet<ModuleName>>()
        };
        let annotationlib = ModuleName::from_str("annotationlib");
        assert!(names(PythonVersion::new(3, 15, 0)).contains(&annotationlib));
        assert!(!names(PythonVersion::new(3, 13, 0)).contains(&annotationlib));
    }

    #[test]
    fn test_imported_stub_and_its_dependencies_are_admitted() {
        let sources = TestSources::new(&[("a", "import subprocess")]);
        let names: AHashSet<ModuleName> = sources.module_names_iter().copied().collect();
        assert!(names.contains(&ModuleName::from_str("subprocess")));
        // subprocess.pyi imports types, so the closure must reach it too.
        assert!(names.contains(&ModuleName::from_str("types")));
    }

    #[test]
    fn test_verbose_options_use_unique_paths() {
        assert_ne!(
            verbose_test_options().verbose_output_path,
            verbose_test_options().verbose_output_path
        );
    }

    #[test]
    fn test_final_analysis_includes_parse_errors() {
        let sources = TestSources::new(&[("a", "import broken")]).with_parse_errors(&["broken"]);
        let analysis = run_lifeguard_analysis_on(&sources, &test_options());
        assert!(
            analysis
                .failing_modules
                .contains(&ModuleName::from_str("broken"))
        );
    }
}
