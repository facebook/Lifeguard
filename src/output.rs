/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Write;

use ahash::AHashMap;
use ahash::AHashSet;
use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeMap;
use serde::ser::SerializeStruct;
use starlark_map::small_set::SmallSet;

use crate::errors::ErrorKind;
use crate::errors::ErrorMetadata;
use crate::errors::SafetyError;
use crate::exports::Exports;
use crate::imports::ImportGraph;
use crate::module_parser::ParsedModule;
use crate::module_safety::SafetyResult;
use crate::project::SafetyMap;
use crate::project::SideEffectMap;
use crate::runner::Options;
use crate::source_map::ModuleProvider;

pub struct LifeGuardAnalysis {
    pub output: LifeGuardOutput,
    pub failing_modules: SmallSet<ModuleName>,
    pub passing_modules: SmallSet<ModuleName>,
    // Dictionary mapping (error kind, metadata) : num of occurrences
    pub aggregated_errors: AHashMap<(ErrorKind, ErrorMetadata), usize>,
}

pub struct LifeGuardOutput {
    // Set of modules where we would like to load all of its imports eagerly
    pub load_imports_eagerly: SmallSet<ModuleName>,

    // Dictionary mapping safe modules to Lazy Imports incompatible modules
    // that are preventing them from being loaded lazily.
    // Uses DashMap for concurrent insertion during analysis.
    pub lazy_eligible: DashMap<ModuleName, SmallSet<ModuleName>>,

    // Whether to sort keys and values for deterministic output.
    pub sorted_output: bool,

    // Verbose-mode fields: only populated when --verbose-output is used.
    pub implicit_imports: Option<AHashMap<ModuleName, Vec<ModuleName>>>,
    pub import_cycles: Option<Vec<Vec<ModuleName>>>,
}

impl Serialize for LifeGuardOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let num_fields =
            2 + self.implicit_imports.is_some() as usize + self.import_cycles.is_some() as usize;
        let mut state = serializer.serialize_struct("LifeGuardOutput", num_fields)?;
        if self.sorted_output {
            let mut items: Vec<&ModuleName> = self.load_imports_eagerly.iter().collect();
            items.sort();
            state.serialize_field("LOAD_IMPORTS_EAGERLY", &items)?;
        } else {
            let items: Vec<&ModuleName> = self.load_imports_eagerly.iter().collect();
            state.serialize_field("LOAD_IMPORTS_EAGERLY", &items)?;
        }
        if self.sorted_output {
            let mut keys: Vec<ModuleName> = self.lazy_eligible.iter().map(|e| *e.key()).collect();
            keys.sort();
            let sorted: Vec<(ModuleName, Vec<ModuleName>)> = keys
                .iter()
                .map(|k| {
                    let entry = self.lazy_eligible.get(k).unwrap();
                    let mut vals: Vec<ModuleName> = entry.value().iter().copied().collect();
                    vals.sort();
                    (*k, vals)
                })
                .collect();
            state.serialize_field("LAZY_ELIGIBLE", &SortedModuleMap(&sorted))?;
        } else {
            state.serialize_field("LAZY_ELIGIBLE", &UnsortedDashMap(&self.lazy_eligible))?;
        }

        // Always sort implicit_imports and import_cycles — these are only
        // included in verbose mode where determinism matters more than speed.
        if let Some(implicit_imports) = &self.implicit_imports {
            let mut sorted: Vec<(ModuleName, Vec<ModuleName>)> = implicit_imports
                .iter()
                .map(|(k, v)| {
                    let mut vals = v.clone();
                    vals.sort();
                    (*k, vals)
                })
                .collect();
            sorted.sort_by_key(|(k, _)| *k);
            state.serialize_field("IMPLICIT_IMPORTS", &SortedModuleMap(&sorted))?;
        }

        if let Some(import_cycles) = &self.import_cycles {
            let mut sorted = import_cycles.clone();
            for cycle in &mut sorted {
                cycle.sort();
            }
            sorted.sort();
            state.serialize_field("IMPORT_CYCLES", &sorted)?;
        }

        state.end()
    }
}

/// Helper to serialize a pre-sorted list of (key, values) as a JSON map.
struct SortedModuleMap<'a>(&'a [(ModuleName, Vec<ModuleName>)]);

impl Serialize for SortedModuleMap<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

/// Helper to serialize a DashMap as a JSON map without sorting.
struct UnsortedDashMap<'a>(&'a DashMap<ModuleName, SmallSet<ModuleName>>);

impl Serialize for UnsortedDashMap<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for entry in self.0.iter() {
            map.serialize_entry(entry.key(), entry.value())?;
        }
        map.end()
    }
}

impl LifeGuardOutput {
    pub fn new(sorted_output: bool) -> Self {
        LifeGuardOutput {
            load_imports_eagerly: SmallSet::new(),
            lazy_eligible: DashMap::new(),
            sorted_output,
            implicit_imports: None,
            import_cycles: None,
        }
    }
}

/// Result of classifying modules from the safety map into passing/failing.
struct ClassifiedModules {
    failing_modules: SmallSet<ModuleName>,
    passing_modules: SmallSet<ModuleName>,
    load_imports_eagerly: SmallSet<ModuleName>,
    implicit_imports: AHashMap<ModuleName, Vec<ModuleName>>,
    aggregated_errors: AHashMap<(ErrorKind, ErrorMetadata), usize>,
}

/// Iterate the safety map and classify each module as passing or failing.
/// Also collects load_imports_eagerly, implicit imports, and aggregated error counts.
fn classify_modules(safety_map: SafetyMap) -> ClassifiedModules {
    let mut result = ClassifiedModules {
        failing_modules: SmallSet::new(),
        passing_modules: SmallSet::new(),
        load_imports_eagerly: SmallSet::new(),
        implicit_imports: AHashMap::<ModuleName, Vec<ModuleName>>::new(),
        aggregated_errors: AHashMap::new(),
    };

    for (module_name, safety_result) in safety_map {
        // Skip modules that failed analysis
        let module_safety = match safety_result {
            SafetyResult::Ok(safety) => safety,
            SafetyResult::AnalysisError(_) => {
                result.failing_modules.insert(module_name);
                continue;
            }
        };

        let mut module_errors = AHashSet::new();
        let is_safe = module_safety.is_safe();
        if module_safety.should_load_imports_eagerly() {
            result.load_imports_eagerly.insert(module_name);
        }
        if module_safety.has_implicit_imports() {
            result
                .implicit_imports
                .insert(module_name, module_safety.implicit_imports);
        }

        let module_set = if is_safe {
            &mut result.passing_modules
        } else {
            &mut result.failing_modules
        };
        module_set.insert(module_name);

        for error in module_safety.errors {
            module_errors.insert((error.kind, error.metadata));
        }

        // TODO: Should we add force_imports_eager_overrides to a separate error count?
        for error in module_safety.force_imports_eager_overrides {
            module_errors.insert((error.kind, error.metadata));
        }
        for k in module_errors.drain() {
            *result.aggregated_errors.entry(k).or_insert(0) += 1;
        }
    }

    result
}

/// Build a map from module -> set of source modules for its re-exports that are failing.
fn build_re_export_map(
    exports: &Exports,
    failing_modules: &SmallSet<ModuleName>,
) -> AHashMap<ModuleName, AHashSet<ModuleName>> {
    let mut map: AHashMap<ModuleName, AHashSet<ModuleName>> = AHashMap::new();
    for (re_export_name, (source_name, _)) in exports.get_re_exports() {
        let source_module = source_name.module;
        if failing_modules.contains(&source_module) {
            let module_part = re_export_name.module;
            map.entry(module_part).or_default().insert(source_module);
        }
    }
    map
}

/// Build the lazy_eligible dict by scanning the import graph, handling cycles,
/// and adding implicit imports.
fn build_lazy_eligible(
    import_graph: &ImportGraph,
    classified: &ClassifiedModules,
    re_export_map: &AHashMap<ModuleName, AHashSet<ModuleName>>,
    all_cycles: &[Vec<ModuleName>],
) -> DashMap<ModuleName, SmallSet<ModuleName>> {
    let lazy_eligible: DashMap<ModuleName, SmallSet<ModuleName>> = DashMap::new();

    // Build a set of cycle members so we can identify children of cycle modules
    // during the parallel iteration below.
    let cycle_module_set: AHashSet<ModuleName> = all_cycles.iter().flatten().cloned().collect();

    // Compute the lazy_eligible dict by scanning the import graph. Also identify missing modules.
    // We also need to check the source module for any re-exports imported.
    //
    // Simultaneously, collect children of cycle modules into a DashMap so we can
    // propagate cycle deps without a separate iteration pass.
    let cycle_children: DashMap<ModuleName, Vec<ModuleName>> = DashMap::new();
    import_graph.modules_par_iter().for_each(|module_name| {
        // Record if this module is a direct child of a cycle module
        if let Some(parent) = module_name.parent() {
            if cycle_module_set.contains(&parent) {
                cycle_children.entry(parent).or_default().push(*module_name);
            }
        }

        if classified.passing_modules.contains(module_name) {
            let mut failing_imported_modules: SmallSet<ModuleName> = SmallSet::new();

            for imported_module in import_graph.get_imports(module_name) {
                // Check if directly failing
                if classified.failing_modules.contains(imported_module) {
                    failing_imported_modules.insert(*imported_module);
                }
                // Check if this module has re-exports from failing modules
                if let Some(source_modules) = re_export_map.get(imported_module) {
                    failing_imported_modules.extend(source_modules.iter().copied());
                }
            }

            // Modules without python source code are marked as missing; by default,
            // these should be included in the list of "failing modules".
            if let Some(missing) = import_graph.get_missing_imports(module_name) {
                for missing_module in missing {
                    failing_imported_modules.insert(*missing_module);
                }
            }
            lazy_eligible.insert(*module_name, failing_imported_modules);
        }
    });

    let cycle_ctx = CycleDepsContext {
        import_graph,
        lazy_eligible: &lazy_eligible,
        passing_modules: &classified.passing_modules,
        cycle_children: &cycle_children,
    };
    add_cycle_deps(all_cycles, &cycle_ctx);

    // Add implicit imports to lazy_eligible dict.
    for (module_name, implicit_imports_set) in &classified.implicit_imports {
        if classified.passing_modules.contains(module_name) {
            lazy_eligible
                .entry(*module_name)
                .or_default()
                .extend(implicit_imports_set.iter().copied());
        }
    }

    lazy_eligible
}

impl LifeGuardAnalysis {
    pub fn new(
        safety_map: SafetyMap,
        import_graph: ImportGraph,
        exports: &Exports,
        options: &Options,
    ) -> Self {
        // Collect all modules in the safety map for filtering cycles later.
        let source_modules: AHashSet<ModuleName> =
            safety_map.iter().map(|entry| *entry.key()).collect();

        let classified = classify_modules(safety_map);
        let all_cycles = collect_cycles(&import_graph, &source_modules);
        let re_export_map = build_re_export_map(exports, &classified.failing_modules);
        let lazy_eligible =
            build_lazy_eligible(&import_graph, &classified, &re_export_map, &all_cycles);

        let verbose = options.verbose_output_path.is_some();
        let output = if verbose {
            LifeGuardOutput {
                load_imports_eagerly: classified.load_imports_eagerly,
                lazy_eligible,
                sorted_output: options.sorted_output,
                implicit_imports: Some(classified.implicit_imports),
                import_cycles: Some(all_cycles),
            }
        } else {
            LifeGuardOutput {
                load_imports_eagerly: classified.load_imports_eagerly,
                lazy_eligible,
                sorted_output: options.sorted_output,
                implicit_imports: None,
                import_cycles: None,
            }
        };

        Self {
            output,
            failing_modules: classified.failing_modules,
            passing_modules: classified.passing_modules,
            aggregated_errors: classified.aggregated_errors,
        }
    }

    /// Propagate side-effect imports: if module A has an unused import of module B,
    /// and B is a passing module with non-empty failing deps, add B to A's failing
    /// deps so B is eagerly imported.
    pub fn propagate_side_effect_imports(&mut self, side_effect_imports: &SideEffectMap) {
        for (module_name, se_imports) in side_effect_imports {
            if !self.passing_modules.contains(module_name) {
                continue;
            }
            let mut new_deps: SmallSet<ModuleName> = SmallSet::new();
            for se_import in se_imports {
                if let Some(deps) = self.output.lazy_eligible.get(se_import) {
                    if !deps.is_empty() {
                        new_deps.insert(*se_import);
                    }
                }
            }
            if !new_deps.is_empty() {
                self.output
                    .lazy_eligible
                    .entry(*module_name)
                    .or_default()
                    .extend(new_deps.into_iter());
            }
        }
    }

    pub fn get_report(&self) -> String {
        let mut error_vec: Vec<_> = self.aggregated_errors.iter().collect();

        let default_size = 20; // This could be made configurable
        let max_size = default_size.min(error_vec.len());

        error_vec.sort_by(|a, b| b.1.cmp(a.1));
        error_vec.truncate(max_size);

        let error_reports = error_vec
            .into_iter()
            .map(|((kind, metadata), prevalence)| {
                format!("{}, ({:?}, \"{}\")", prevalence, kind, metadata)
            })
            .collect::<Vec<_>>()
            .join("\n");

        let total_modules = self.failing_modules.len() + self.passing_modules.len();
        let pass_rate_by_file = if total_modules > 0 {
            (self.passing_modules.len() as f64 / total_modules as f64) * 100.0
        } else {
            0.0
        };

        let avg_num_of_errors = if self.failing_modules.is_empty() {
            0.0
        } else {
            {
                self.aggregated_errors.values().sum::<usize>() as f64
                    / self.failing_modules.len() as f64
            }
        };

        format!(
            "{}\nPASS RATE BY FILE %    | AVG NUM OF ERRORS IN FAILING MODULES\n{:.2} %                | {:.2}\nNum of failing files: {}\nNum of passing files: {}\nNum of load-imports-eagerly modules: {}",
            error_reports,
            pass_rate_by_file,
            avg_num_of_errors,
            self.failing_modules.len(),
            self.passing_modules.len(),
            self.output.load_imports_eagerly.len(),
        )
    }

    pub fn print_diagnostics(&self) {
        for m in &self.passing_modules {
            println!("Passing: {:?}", m);
        }
        for m in &self.failing_modules {
            println!("Failing: {:?}", m);
        }
    }
}

/// Collect import cycles as lists of module names, filtered to source modules only.
fn collect_cycles(
    import_graph: &ImportGraph,
    source_modules: &AHashSet<ModuleName>,
) -> Vec<Vec<ModuleName>> {
    import_graph
        .graph
        .find_cycles()
        .into_iter()
        .filter_map(|cycle| {
            let members: Vec<ModuleName> = import_graph
                .graph
                .cycle_names(&cycle)
                .filter(|m| source_modules.contains(m))
                .collect();
            if members.is_empty() {
                None
            } else {
                Some(members)
            }
        })
        .collect()
}

/// Shared context for cycle dependency propagation.
struct CycleDepsContext<'a> {
    import_graph: &'a ImportGraph,
    lazy_eligible: &'a DashMap<ModuleName, SmallSet<ModuleName>>,
    passing_modules: &'a SmallSet<ModuleName>,
    cycle_children: &'a DashMap<ModuleName, Vec<ModuleName>>,
}

/// Add cycle dependencies to the lazy_eligible dict and propagate to child modules.
/// For each module in a cycle, only its *direct imports* that are also in the cycle
/// are added as lazy_eligible deps, rather than all cycle members.
/// Only passing modules are added to the lazy_eligible dict.
///
/// Propagation to children is needed because CPython's `from X import Y` lazy_eligible check
/// constructs "X.Y" and checks that against the lazy_eligible dict. If X has cycle deps but
/// X.Y doesn't, the import would be incorrectly marked as lazy.
fn add_cycle_deps(all_cycles: &[Vec<ModuleName>], ctx: &CycleDepsContext) {
    for cycle_modules in all_cycles {
        let cycle_set: AHashSet<ModuleName> = cycle_modules.iter().cloned().collect();
        for module_name in cycle_modules {
            if !ctx.passing_modules.contains(module_name) {
                continue;
            }
            let cycle_imports: SmallSet<ModuleName> = ctx
                .import_graph
                .get_imports(module_name)
                .filter(|m| cycle_set.contains(m))
                .cloned()
                .collect();

            if !cycle_imports.is_empty() {
                ctx.lazy_eligible
                    .entry(*module_name)
                    .or_default()
                    .extend(cycle_imports.iter().cloned());

                // Propagate to direct children of this cycle module
                if let Some(children) = ctx.cycle_children.get(module_name) {
                    for child in children.value() {
                        if ctx.passing_modules.contains(child) {
                            ctx.lazy_eligible
                                .entry(*child)
                                .or_default()
                                .extend(cycle_imports.iter().cloned());
                        }
                    }
                }
            }
        }
    }
}

/// Write all errors to a file. Parses each module on demand to get line numbers.
pub fn write_verbose<W: Write>(
    out: &mut W,
    safety_map: &SafetyMap,
    sources: &impl ModuleProvider,
) -> anyhow::Result<()> {
    writeln!(out, "# Lifeguard Verbose Output:")?;
    writeln!(
        out,
        "------------------------------------------------------------------------------"
    )?;

    let mut keys: Vec<ModuleName> = safety_map.iter().map(|entry| *entry.key()).collect();
    keys.sort();

    let write_error = |out: &mut W, module: &ParsedModule, error: &SafetyError| {
        let line = module.byte_to_line_number(error.range.start().into());

        writeln!(
            out,
            "  Line {} - {:?} {}",
            line,
            error.kind,
            error.metadata.as_str(),
        )
    };

    for module_name in &keys {
        let ast_result = sources.parse(module_name);

        let parsed_module = match &ast_result {
            Some(r) => match r.as_parsed() {
                Ok(m) => m,
                Err(_) => {
                    writeln!(out, "## {} ", module_name.as_str())?;
                    writeln!(out, "### Could not parse module\n")?;
                    continue;
                }
            },
            None => {
                writeln!(out, "## {} ", module_name.as_str())?;
                writeln!(out, "### Could not parse module\n")?;
                continue;
            }
        };

        writeln!(out, "## {} ", module_name.as_str())?;

        let Some(mut safety_ref) = safety_map.get_mut(module_name) else {
            continue;
        };
        let module_safety = match safety_ref.value_mut() {
            SafetyResult::Ok(safety) => safety,
            SafetyResult::AnalysisError(e) => {
                writeln!(out, "### Analysis Error")?;
                writeln!(out, "  {}", e)?;
                continue;
            }
        };

        if module_safety.errors.is_empty()
            && module_safety.force_imports_eager_overrides.is_empty()
            && module_safety.implicit_imports.is_empty()
        {
            writeln!(
                out,
                "### Lazy imports incompatibilities were not detected\n"
            )?;
            continue;
        }

        writeln!(out, "### Errors")?;
        module_safety.errors.sort();
        for error in module_safety.errors.iter() {
            write_error(out, parsed_module, error)?;
        }

        writeln!(out, "### Load Imports Eagerly")?;
        module_safety.force_imports_eager_overrides.sort();
        for exclude in module_safety.force_imports_eager_overrides.iter() {
            write_error(out, parsed_module, exclude)?;
        }

        writeln!(out, "### Implicit Imports")?;
        module_safety.implicit_imports.sort();
        for import in module_safety.implicit_imports.iter() {
            writeln!(out, "  {}", import.as_str())?;
        }

        writeln!(out)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use ruff_text_size::TextRange;
    use ruff_text_size::TextSize;

    use super::*;
    use crate::errors::ErrorKind;
    use crate::errors::SafetyError;
    use crate::module_safety::ModuleSafety;
    use crate::module_safety::SafetyResult;
    use crate::test_lib::TestSources;

    fn mn(s: &str) -> ModuleName {
        ModuleName::from_str(s)
    }

    fn make_error(kind: ErrorKind, metadata: &str, offset: u32) -> SafetyError {
        SafetyError::new(
            kind,
            metadata.to_string(),
            TextRange::new(TextSize::new(offset), TextSize::new(offset + 1)),
        )
    }

    // ---- classify_modules tests ----

    #[test]
    fn test_classify_modules_passing() {
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(mn("foo"), SafetyResult::Ok(ModuleSafety::new()));
        safety_map.insert(mn("bar"), SafetyResult::Ok(ModuleSafety::new()));

        let result = classify_modules(safety_map);
        assert_eq!(result.passing_modules.len(), 2);
        assert_eq!(result.failing_modules.len(), 0);
        assert!(result.load_imports_eagerly.is_empty());
    }

    #[test]
    fn test_classify_modules_failing() {
        let safety_map: SafetyMap = DashMap::new();
        let mut safety = ModuleSafety::new();
        safety.add_error(make_error(ErrorKind::UnsafeFunctionCall, "some_func()", 0));
        safety_map.insert(mn("bad"), SafetyResult::Ok(safety));

        let result = classify_modules(safety_map);
        assert_eq!(result.failing_modules.len(), 1);
        assert!(result.failing_modules.contains(&mn("bad")));
        assert_eq!(result.passing_modules.len(), 0);
    }

    #[test]
    fn test_classify_modules_analysis_error() {
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(
            mn("broken"),
            SafetyResult::AnalysisError(anyhow::anyhow!("parse failed")),
        );
        safety_map.insert(mn("good"), SafetyResult::Ok(ModuleSafety::new()));

        let result = classify_modules(safety_map);
        assert!(result.failing_modules.contains(&mn("broken")));
        assert!(result.passing_modules.contains(&mn("good")));
        assert_eq!(result.aggregated_errors.len(), 0);
    }

    #[test]
    fn test_classify_modules_load_imports_eagerly() {
        let safety_map: SafetyMap = DashMap::new();
        let mut safety = ModuleSafety::new();
        safety.add_force_import_override(make_error(ErrorKind::ExecCall, "exec()", 0));
        safety_map.insert(mn("exec_mod"), SafetyResult::Ok(safety));

        let result = classify_modules(safety_map);
        assert!(result.load_imports_eagerly.contains(&mn("exec_mod")));
    }

    #[test]
    fn test_classify_modules_aggregated_errors() {
        let safety_map: SafetyMap = DashMap::new();

        let mut s1 = ModuleSafety::new();
        s1.add_error(make_error(ErrorKind::UnsafeFunctionCall, "f()", 0));
        safety_map.insert(mn("a"), SafetyResult::Ok(s1));

        let mut s2 = ModuleSafety::new();
        s2.add_error(make_error(ErrorKind::UnsafeFunctionCall, "f()", 10));
        safety_map.insert(mn("b"), SafetyResult::Ok(s2));

        let result = classify_modules(safety_map);
        let key = (
            ErrorKind::UnsafeFunctionCall,
            "f()".parse::<ErrorMetadata>().unwrap(),
        );
        assert_eq!(result.aggregated_errors[&key], 2);
    }

    // ---- LifeGuardOutput serialization tests ----

    #[test]
    fn test_serialize_sorted_output() {
        let mut output = LifeGuardOutput::new(true);
        output.load_imports_eagerly.insert(mn("z_mod"));
        output.load_imports_eagerly.insert(mn("a_mod"));
        output.lazy_eligible.insert(mn("foo"), SmallSet::new());

        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        let eager = parsed["LOAD_IMPORTS_EAGERLY"].as_array().unwrap();
        assert_eq!(eager[0].as_str().unwrap(), "a_mod");
        assert_eq!(eager[1].as_str().unwrap(), "z_mod");
        assert!(parsed["LAZY_ELIGIBLE"]["foo"].is_array());
    }

    #[test]
    fn test_serialize_unsorted_output() {
        let output = LifeGuardOutput::new(false);
        output.lazy_eligible.insert(mn("mod_a"), {
            let mut s = SmallSet::new();
            s.insert(mn("dep_x"));
            s
        });

        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(
            parsed["LOAD_IMPORTS_EAGERLY"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(parsed["LAZY_ELIGIBLE"].is_object());
    }

    // ---- write_verbose tests ----

    #[test]
    fn test_write_verbose_analysis_error() {
        let sources = TestSources::new(&[("broken", "x = 1\n")]);
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(
            mn("broken"),
            SafetyResult::AnalysisError(anyhow::anyhow!("something went wrong")),
        );

        let mut buf = Vec::new();
        write_verbose(&mut buf, &safety_map, &sources).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("## broken"));
        assert!(output.contains("### Analysis Error"));
        assert!(output.contains("something went wrong"));
    }

    #[test]
    fn test_write_verbose_no_errors() {
        let sources = TestSources::new(&[("clean", "x = 1\n")]);
        let safety_map: SafetyMap = DashMap::new();
        safety_map.insert(mn("clean"), SafetyResult::Ok(ModuleSafety::new()));

        let mut buf = Vec::new();
        write_verbose(&mut buf, &safety_map, &sources).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("## clean"));
        assert!(output.contains("Lazy imports incompatibilities were not detected"));
    }

    #[test]
    fn test_write_verbose_with_errors() {
        let sources = TestSources::new(&[("bad", "some_func()\n")]);
        let safety_map: SafetyMap = DashMap::new();
        let mut safety = ModuleSafety::new();
        safety.add_error(make_error(ErrorKind::UnsafeFunctionCall, "some_func()", 0));
        safety_map.insert(mn("bad"), SafetyResult::Ok(safety));

        let mut buf = Vec::new();
        write_verbose(&mut buf, &safety_map, &sources).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("## bad"));
        assert!(output.contains("### Errors"));
        assert!(output.contains("UnsafeFunctionCall"));
        assert!(output.contains("some_func()"));
    }

    // ---- build_lazy_eligible / cycle propagation tests ----

    #[test]
    fn test_build_lazy_eligible_basic() {
        let mut import_graph = ImportGraph::new();
        import_graph.graph.add_node(&mn("safe"));
        import_graph.graph.add_node(&mn("unsafe_mod"));
        import_graph.graph.add_edge(&mn("safe"), &mn("unsafe_mod"));

        let mut classified = ClassifiedModules {
            failing_modules: SmallSet::new(),
            passing_modules: SmallSet::new(),
            load_imports_eagerly: SmallSet::new(),
            implicit_imports: AHashMap::new(),
            aggregated_errors: AHashMap::new(),
        };
        classified.passing_modules.insert(mn("safe"));
        classified.failing_modules.insert(mn("unsafe_mod"));

        let re_export_map = AHashMap::new();
        let all_cycles: Vec<Vec<ModuleName>> = vec![];
        let lazy_eligible =
            build_lazy_eligible(&import_graph, &classified, &re_export_map, &all_cycles);

        let entry = lazy_eligible.get(&mn("safe")).unwrap();
        assert!(entry.contains(&mn("unsafe_mod")));
    }

    #[test]
    fn test_cycle_deps_propagate_to_children() {
        // Create a cycle: a -> b -> a
        // And a child module a.child that is passing
        let mut import_graph = ImportGraph::new();
        let a = mn("a");
        let b = mn("b");
        let a_child = mn("a.child");

        import_graph.graph.add_node(&a);
        import_graph.graph.add_node(&b);
        import_graph.graph.add_node(&a_child);
        import_graph.graph.add_edge(&a, &b);
        import_graph.graph.add_edge(&b, &a);

        let mut classified = ClassifiedModules {
            failing_modules: SmallSet::new(),
            passing_modules: SmallSet::new(),
            load_imports_eagerly: SmallSet::new(),
            implicit_imports: AHashMap::new(),
            aggregated_errors: AHashMap::new(),
        };
        classified.passing_modules.insert(a);
        classified.passing_modules.insert(b);
        classified.passing_modules.insert(a_child);

        let re_export_map = AHashMap::new();
        let all_cycles = vec![vec![a, b]];
        let lazy_eligible =
            build_lazy_eligible(&import_graph, &classified, &re_export_map, &all_cycles);

        // a should have b as a cycle dep
        let a_deps = lazy_eligible.get(&a).unwrap();
        assert!(a_deps.contains(&b));

        // b should have a as a cycle dep
        let b_deps = lazy_eligible.get(&b).unwrap();
        assert!(b_deps.contains(&a));

        // a.child (child of cycle member a) should also get the cycle deps propagated
        let child_deps = lazy_eligible.get(&a_child).unwrap();
        assert!(child_deps.contains(&b));
    }

    // ---- get_report tests ----

    #[test]
    fn test_get_report_format() {
        let analysis = LifeGuardAnalysis {
            output: LifeGuardOutput::new(true),
            failing_modules: {
                let mut s = SmallSet::new();
                s.insert(mn("bad"));
                s
            },
            passing_modules: {
                let mut s = SmallSet::new();
                s.insert(mn("good1"));
                s.insert(mn("good2"));
                s
            },
            aggregated_errors: {
                let mut m = AHashMap::new();
                m.insert(
                    (
                        ErrorKind::UnsafeFunctionCall,
                        "f()".parse::<ErrorMetadata>().unwrap(),
                    ),
                    3,
                );
                m
            },
        };

        let report = analysis.get_report();
        assert!(report.contains("66.67 %"));
        assert!(report.contains("Num of failing files: 1"));
        assert!(report.contains("Num of passing files: 2"));
    }
}
