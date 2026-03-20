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
}

impl Serialize for LifeGuardOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("LifeGuardOutput", 2)?;
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
            state.serialize_field("LAZY_ELIGIBLE", &LazyEligibleSorted(&sorted))?;
        } else {
            state.serialize_field("LAZY_ELIGIBLE", &LazyEligibleUnsorted(&self.lazy_eligible))?;
        }
        state.end()
    }
}

/// Helper to serialize a pre-sorted list of (key, values) as a JSON map.
struct LazyEligibleSorted<'a>(&'a [(ModuleName, Vec<ModuleName>)]);

impl Serialize for LazyEligibleSorted<'_> {
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
struct LazyEligibleUnsorted<'a>(&'a DashMap<ModuleName, SmallSet<ModuleName>>);

impl Serialize for LazyEligibleUnsorted<'_> {
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
        }
    }
}

impl LifeGuardAnalysis {
    pub fn new(
        safety_map: SafetyMap,
        import_graph: ImportGraph,
        exports: &Exports,
        options: &Options,
    ) -> Self {
        let mut output = LifeGuardOutput::new(options.sorted_output);
        let mut failing_modules = SmallSet::new();
        let mut passing_modules = SmallSet::new();
        let mut aggregated_errors = AHashMap::new();

        let mut implicit_imports = AHashMap::new();

        // Collect all modules in the safety map for filtering cycles later.
        let source_modules: AHashSet<ModuleName> =
            safety_map.iter().map(|entry| *entry.key()).collect();

        // Iterate all processed modules and their errors.  Identify implicit imports.
        for (module_name, safety_result) in safety_map {
            // Skip modules that failed analysis
            let module_safety = match safety_result {
                SafetyResult::Ok(safety) => safety,
                SafetyResult::AnalysisError(_) => {
                    failing_modules.insert(module_name);
                    continue;
                }
            };

            let mut module_errors = AHashSet::new();
            let is_safe = module_safety.is_safe();
            if module_safety.should_load_imports_eagerly() {
                output.load_imports_eagerly.insert(module_name);
            }
            if module_safety.has_implicit_imports() {
                implicit_imports.insert(module_name, module_safety.implicit_imports);
            }

            let module_set = if is_safe {
                &mut passing_modules
            } else {
                &mut failing_modules
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
                *aggregated_errors.entry(k).or_insert(0) += 1;
            }
        }

        let all_cycles = collect_cycles(&import_graph, &source_modules);

        // Build a set of cycle members so we can identify children of cycle modules
        // during the parallel iteration below.
        let cycle_module_set: AHashSet<ModuleName> = all_cycles.iter().flatten().cloned().collect();

        // Pre-compute a map from module -> set of definition modules for its re-exports
        let re_export_map: AHashMap<ModuleName, AHashSet<ModuleName>> = {
            let mut map: AHashMap<ModuleName, AHashSet<ModuleName>> = AHashMap::new();
            for (re_export_name, (source_name, _)) in exports.get_re_exports() {
                // Get the source module from source_name
                let source_module = source_name.module;
                // Only track if the source module is failing
                if failing_modules.contains(&source_module) {
                    let module_part = re_export_name.module;
                    map.entry(module_part).or_default().insert(source_module);
                }
            }
            map
        };

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

            if passing_modules.contains(module_name) {
                let mut failing_imported_modules: SmallSet<ModuleName> = SmallSet::new();

                for imported_module in import_graph.get_imports(module_name) {
                    // Check if directly failing
                    if failing_modules.contains(imported_module) {
                        failing_imported_modules.insert(*imported_module);
                    }

                    // Check if this module has re-exports from failing modules, if so add them to the lazy_eligible list
                    if let Some(source_modules) = re_export_map.get(imported_module) {
                        failing_imported_modules.extend(source_modules.iter().copied());
                    }
                }

                // Modules without python source code are marked as missing-- by default, these
                // files should be included in the list of "failing modules".
                for missing_module in import_graph.get_missing_imports(module_name) {
                    failing_imported_modules.insert(*missing_module);
                }
                output
                    .lazy_eligible
                    .insert(*module_name, failing_imported_modules);
            }
        });

        add_cycle_deps(
            &all_cycles,
            &import_graph,
            &output.lazy_eligible,
            &passing_modules,
            &cycle_children,
        );

        // Add implicit imports to lazy_eligible dict.
        for (module_name, implicit_imports_set) in implicit_imports {
            if passing_modules.contains(&module_name) {
                output
                    .lazy_eligible
                    .entry(module_name)
                    .or_default()
                    .extend(implicit_imports_set.into_iter());
            }
        }

        Self {
            output,
            failing_modules,
            passing_modules,
            aggregated_errors,
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
        .map(|cycle| {
            import_graph
                .graph
                .cycle_names(&cycle)
                .filter(|m| source_modules.contains(m))
                .collect()
        })
        .collect()
}

/// Add cycle dependencies to the lazy_eligible dict and propagate to child modules.
/// For each module in a cycle, only its *direct imports* that are also in the cycle
/// are added as lazy_eligible deps, rather than all cycle members.
/// Only passing modules are added to the lazy_eligible dict.
///
/// Propagation to children is needed because CPython's `from X import Y` lazy_eligible check
/// constructs "X.Y" and checks that against the lazy_eligible dict. If X has cycle deps but
/// X.Y doesn't, the import would be incorrectly marked as lazy.
fn add_cycle_deps(
    all_cycles: &[Vec<ModuleName>],
    import_graph: &ImportGraph,
    lazy_eligible: &DashMap<ModuleName, SmallSet<ModuleName>>,
    passing_modules: &SmallSet<ModuleName>,
    cycle_children: &DashMap<ModuleName, Vec<ModuleName>>,
) {
    for cycle_modules in all_cycles {
        let cycle_set: AHashSet<ModuleName> = cycle_modules.iter().cloned().collect();
        for module_name in cycle_modules {
            if !passing_modules.contains(module_name) {
                continue;
            }
            let cycle_imports: SmallSet<ModuleName> = import_graph
                .get_imports(module_name)
                .filter(|m| cycle_set.contains(m))
                .cloned()
                .collect();

            if !cycle_imports.is_empty() {
                lazy_eligible
                    .entry(*module_name)
                    .or_default()
                    .extend(cycle_imports.iter().cloned());

                // Propagate to direct children of this cycle module
                if let Some(children) = cycle_children.get(module_name) {
                    for child in children.value() {
                        if passing_modules.contains(child) {
                            lazy_eligible
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
