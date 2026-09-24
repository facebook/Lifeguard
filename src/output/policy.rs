/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Lazy-eligibility policy: classify each module as passing or failing, then
//! compute the eager-load guard set every passing module carries.

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use starlark_map::small_set::SmallSet;

use crate::cache::CachedModule;
use crate::cache::CachedReExport;
use crate::cache::CachedSafety;
use crate::cache::ResolvedCache;
use crate::errors::ErrorKind;
use crate::errors::ErrorMetadata;
use crate::exports::Exports;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::module_safety::SafetyResult;
use crate::output::LifeGuardOutput;
use crate::output::diagnostics::AnalysisSummary;
use crate::project::SafetyMap;
use crate::project::SideEffectMap;
use crate::runner::Options;
use crate::tracing::time;

pub struct LifeGuardAnalysis {
    pub output: LifeGuardOutput,
    pub summary: AnalysisSummary,
}

/// Result of classifying modules from the safety map into passing/failing.
#[derive(Default)]
struct ClassifiedModules {
    failing_modules: SmallSet<ModuleName>,
    passing_modules: SmallSet<ModuleName>,
    load_imports_eagerly: SmallSet<ModuleName>,
    implicit_imports: AHashMap<ModuleName, Vec<ModuleName>>,
    aggregated_errors: AHashMap<(ErrorKind, ErrorMetadata), usize>,
}

/// The source and cached representations intentionally use the same classification rules:
/// regular errors make a module fail, while eager-import overrides are tracked independently.
struct ModuleStatus {
    is_safe: bool,
    load_eagerly: bool,
}

enum ReExports<'a> {
    Whole(&'a Exports),
    Cached(&'a [CachedReExport]),
}

enum SideEffects<'a> {
    Whole(&'a SideEffectMap),
    Cached(&'a [CachedModule]),
}

/// Facts consumed by lazy-import policy generation, retaining the source
/// representation where materializing a common form would require cloning.
struct ResolvedProgram<'a> {
    classified: ClassifiedModules,
    import_graph: ImportGraph,
    re_exports: ReExports<'a>,
    side_effects: SideEffects<'a>,
}

impl<'a> ResolvedProgram<'a> {
    fn from_whole_program(
        safety_map: SafetyMap,
        mut import_graph: ImportGraph,
        exports: &'a Exports,
        side_effect_imports: &'a SideEffectMap,
    ) -> Self {
        import_graph.resolve_missing_to_known();
        let classified = classify_modules(safety_map);
        Self {
            classified,
            import_graph,
            re_exports: ReExports::Whole(exports),
            side_effects: SideEffects::Whole(side_effect_imports),
        }
    }

    fn from_cache(cache: &'a ResolvedCache) -> Self {
        let (classified, import_graph) = rayon::join(
            || classify_cached_modules(cache.modules(), cache.graph_only_stubs()),
            || cache.build_import_graph(),
        );
        Self {
            classified,
            import_graph,
            re_exports: ReExports::Cached(cache.re_exports()),
            side_effects: SideEffects::Cached(cache.modules()),
        }
    }
}

impl ClassifiedModules {
    fn record_status(&mut self, module: ModuleName, status: ModuleStatus) {
        if status.is_safe {
            self.passing_modules.insert(module);
        } else {
            self.failing_modules.insert(module);
        }
        if status.load_eagerly {
            self.load_imports_eagerly.insert(module);
        }
    }

    fn record_implicit_imports(&mut self, module: ModuleName, imports: Vec<ModuleName>) {
        if !imports.is_empty() {
            self.implicit_imports.insert(module, imports);
        }
    }

    fn record_errors(&mut self, errors: impl IntoIterator<Item = (ErrorKind, ErrorMetadata)>) {
        for error in errors.into_iter().collect::<AHashSet<_>>() {
            *self.aggregated_errors.entry(error).or_insert(0) += 1;
        }
    }
}

/// Iterate the safety map and classify each module as passing or failing.
/// Also collects load_imports_eagerly, implicit imports, and aggregated error counts.
fn classify_modules(safety_map: SafetyMap) -> ClassifiedModules {
    let mut result = ClassifiedModules::default();

    for (module_name, safety_result) in safety_map {
        // Skip modules that failed analysis
        let module_safety = match safety_result {
            SafetyResult::Ok(safety) => safety,
            SafetyResult::AnalysisError(_) => {
                result.failing_modules.insert(module_name);
                continue;
            }
        };

        result.record_status(
            module_name,
            ModuleStatus {
                is_safe: module_safety.is_safe(),
                load_eagerly: module_safety.should_load_imports_eagerly(),
            },
        );
        result.record_implicit_imports(module_name, module_safety.implicit_imports);
        result.record_errors(
            module_safety
                .errors
                .into_iter()
                .chain(module_safety.force_imports_eager_overrides)
                .map(|error| (error.kind, error.metadata)),
        );
    }

    result
}

fn classify_cached_modules(
    modules: &[CachedModule],
    graph_only_stubs: &AHashSet<ModuleName>,
) -> ClassifiedModules {
    let mut result = ClassifiedModules::default();

    for module in modules {
        if graph_only_stubs.contains(&module.name) {
            continue;
        }
        let safety = match &module.safety {
            CachedSafety::Ok(safety) => safety,
            CachedSafety::AnalysisError { .. } => {
                result.failing_modules.insert(module.name);
                continue;
            }
        };

        result.record_status(
            module.name,
            ModuleStatus {
                is_safe: safety.errors.is_empty(),
                load_eagerly: !safety.force_imports_eager_overrides.is_empty(),
            },
        );
        result.record_implicit_imports(module.name, safety.implicit_imports.clone());
        result.record_errors(
            safety
                .errors
                .iter()
                .chain(&safety.force_imports_eager_overrides)
                .map(|error| (error.kind, ErrorMetadata::from(error.metadata.as_str()))),
        );
    }

    result
}

/// Build a map from module -> set of source modules for its re-exports that are failing.
/// Follows re-export chains transitively so that multi-hop re-exports (A→B→C where C is
/// failing) are correctly attributed.
fn build_re_export_map(
    exports: &Exports,
    failing_modules: &SmallSet<ModuleName>,
) -> AHashMap<ModuleName, AHashSet<ModuleName>> {
    let mut map: AHashMap<ModuleName, AHashSet<ModuleName>> = AHashMap::new();
    for (module, _, (imported, _range)) in exports.get_re_exports() {
        let source_module = match exports.resolve_transitive(imported) {
            Some(resolved) => resolved.module,
            None => continue,
        };
        if failing_modules.contains(&source_module) {
            map.entry(module).or_default().insert(source_module);
        }
    }
    map
}

/// Build the re-export map from cached re-export data.
/// Follows re-export chains transitively, matching the non-cached path.
fn build_re_export_map_from_cache(
    re_exports: &[CachedReExport],
    failing_modules: &SmallSet<ModuleName>,
) -> AHashMap<ModuleName, AHashSet<ModuleName>> {
    type ReMap = AHashMap<ModuleName, AHashSet<ModuleName>>;
    /// Per-task accumulator threaded through `resolve_reexport_chain`.
    #[derive(Default)]
    struct ChainScratch {
        map: ReMap,
        memo: AHashMap<usize, Option<ModuleName>>,
        path: Vec<usize>,
        visited: AHashSet<usize>,
    }

    let chain: AHashMap<(ModuleName, &str), usize> = re_exports
        .iter()
        .enumerate()
        .map(|(i, r)| ((r.exported_module, r.exported_attr.as_str()), i))
        .collect();

    // Resolve each re-export's chain in parallel: the `chain` index is read-only,
    // and each task keeps its own scratch and result map (per-task memos recompute
    // some chains, but the pass parallelizes). Then union the maps.
    re_exports
        .par_iter()
        .enumerate()
        .fold(ChainScratch::default, |mut acc, (start_idx, re_export)| {
            if let Some(source_module) = resolve_reexport_chain(
                start_idx,
                re_exports,
                &chain,
                &mut acc.memo,
                &mut acc.path,
                &mut acc.visited,
            ) && failing_modules.contains(&source_module)
            {
                acc.map
                    .entry(re_export.exported_module)
                    .or_default()
                    .insert(source_module);
            }
            acc
        })
        .map(|acc| acc.map)
        .reduce(ReMap::new, |a, b| {
            // Extend the larger map with the smaller to minimize rehashing.
            let (mut large, small) = if a.len() >= b.len() { (a, b) } else { (b, a) };
            for (module, sources) in small {
                large.entry(module).or_default().extend(sources);
            }
            large
        })
}

fn resolve_reexport_chain(
    start: usize,
    re_exports: &[CachedReExport],
    chain: &AHashMap<(ModuleName, &str), usize>,
    memo: &mut AHashMap<usize, Option<ModuleName>>,
    path: &mut Vec<usize>,
    visited: &mut AHashSet<usize>,
) -> Option<ModuleName> {
    if let Some(&cached) = memo.get(&start) {
        return cached;
    }
    path.clear();
    visited.clear();
    let mut cur_module = re_exports[start].imported_module;
    let mut cur_attr = re_exports[start].imported_attr.as_str();

    let result = loop {
        match chain.get(&(cur_module, cur_attr)) {
            Some(&idx) => {
                if let Some(&cached) = memo.get(&idx) {
                    break cached;
                }
                if !visited.insert(idx) {
                    break None;
                }
                path.push(idx);
                cur_module = re_exports[idx].imported_module;
                cur_attr = &re_exports[idx].imported_attr;
            }
            None => break Some(cur_module),
        }
    };

    memo.insert(start, result);
    for &idx in path.iter() {
        memo.insert(idx, result);
    }
    result
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

    // Guard each consumer with its implicit imports, then also guard the provider
    // path so the import is loaded before the consumer's body references it.
    for (module_name, implicit_imports_set) in &classified.implicit_imports {
        if classified.passing_modules.contains(module_name) {
            lazy_eligible
                .entry(*module_name)
                .or_default()
                .extend(implicit_imports_set.iter().copied());
        }
    }
    time("  Propagating implicit imports", || {
        propagate_implicit_imports_along_paths(import_graph, classified, &lazy_eligible)
    });

    lazy_eligible
}

/// All modules that transitively import `target` (excluding `target` itself).
fn transitive_importers(import_graph: &ImportGraph, target: &ModuleName) -> AHashSet<ModuleName> {
    let mut seen = AHashSet::new();
    let mut stack: Vec<ModuleName> = import_graph.get_importers(target).copied().collect();
    while let Some(m) = stack.pop() {
        if seen.insert(m) {
            stack.extend(import_graph.get_importers(&m).copied());
        }
    }
    seen
}

/// Guard every passing module on an import path `consumer -> ... -> target` with
/// `target`, forcing the path eager until `target` is loaded.
fn propagate_implicit_imports_along_paths(
    import_graph: &ImportGraph,
    classified: &ClassifiedModules,
    lazy_eligible: &DashMap<ModuleName, SmallSet<ModuleName>>,
) {
    // Group by target so each is walked once, not once per consumer.
    let mut consumers_by_target: AHashMap<ModuleName, Vec<ModuleName>> = AHashMap::new();
    for (consumer, targets) in &classified.implicit_imports {
        if classified.passing_modules.contains(consumer) {
            for target in targets {
                consumers_by_target
                    .entry(*target)
                    .or_default()
                    .push(*consumer);
            }
        }
    }

    consumers_by_target
        .par_iter()
        .for_each(|(target, consumers)| {
            let ancestors = transitive_importers(import_graph, target);
            // Walk forward from the consumers within `target`'s ancestors,
            // guarding each passing module reached.
            let mut visited = AHashSet::new();
            let mut stack: Vec<ModuleName> = consumers
                .iter()
                .flat_map(|c| {
                    import_graph
                        .get_imports(c)
                        .filter(|m| ancestors.contains(*m))
                        .copied()
                })
                .collect();
            while let Some(m) = stack.pop() {
                if !visited.insert(m) {
                    continue;
                }
                if classified.passing_modules.contains(&m) {
                    lazy_eligible.entry(m).or_default().insert(*target);
                }
                stack.extend(
                    import_graph
                        .get_imports(&m)
                        .filter(|n| ancestors.contains(*n))
                        .copied(),
                );
            }
        });
}

impl LifeGuardAnalysis {
    pub fn from_whole_program(
        safety_map: SafetyMap,
        import_graph: ImportGraph,
        exports: &Exports,
        side_effect_imports: &SideEffectMap,
        options: &Options,
    ) -> Self {
        let program = ResolvedProgram::from_whole_program(
            safety_map,
            import_graph,
            exports,
            side_effect_imports,
        );
        Self::from_resolved_program(program, options)
    }

    /// Build a LifeGuardAnalysis from pre-computed library caches.
    /// This is the "reduce" step: no per-file analysis happens here.
    pub fn from_resolved_cache(cache: &ResolvedCache, options: &Options) -> Self {
        Self::from_resolved_program(ResolvedProgram::from_cache(cache), options)
    }

    fn from_resolved_program(program: ResolvedProgram<'_>, options: &Options) -> Self {
        let ResolvedProgram {
            classified,
            import_graph,
            re_exports,
            side_effects,
        } = program;
        let source_modules: AHashSet<ModuleName> = classified
            .passing_modules
            .iter()
            .chain(&classified.failing_modules)
            .copied()
            .collect();
        let (all_cycles, re_export_map) = rayon::join(
            || collect_cycles(&import_graph, &source_modules),
            || match re_exports {
                ReExports::Whole(exports) => {
                    build_re_export_map(exports, &classified.failing_modules)
                }
                ReExports::Cached(re_exports) => {
                    build_re_export_map_from_cache(re_exports, &classified.failing_modules)
                }
            },
        );
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

        let mut analysis = Self {
            output,
            summary: AnalysisSummary {
                failing_modules: classified.failing_modules,
                passing_modules: classified.passing_modules,
                aggregated_errors: classified.aggregated_errors,
            },
        };
        match side_effects {
            SideEffects::Whole(imports) => analysis.propagate_side_effect_imports(imports),
            SideEffects::Cached(modules) => analysis.propagate_cached_side_effect_imports(modules),
        }
        analysis
    }

    /// Propagate side-effect imports: if module A has an unused import of module B,
    /// and B is a passing module with non-empty failing deps, add B to A's failing
    /// deps so B is eagerly imported.
    fn propagate_side_effect_imports(&mut self, side_effect_imports: &SideEffectMap) {
        let has_failing_deps = self.modules_with_failing_deps();

        side_effect_imports
            .par_iter()
            .for_each(|(module_name, se_imports)| {
                self.propagate_side_effect_entry(*module_name, se_imports, &has_failing_deps);
            });
    }

    fn propagate_cached_side_effect_imports(&mut self, modules: &[CachedModule]) {
        let has_failing_deps = self.modules_with_failing_deps();
        modules.par_iter().for_each(|module| {
            self.propagate_side_effect_entry(
                module.name,
                &module.side_effect_imports,
                &has_failing_deps,
            );
        });
    }

    fn modules_with_failing_deps(&self) -> AHashSet<ModuleName> {
        self.output
            .lazy_eligible
            .iter()
            .filter_map(|entry| (!entry.value().is_empty()).then(|| *entry.key()))
            .collect()
    }

    fn propagate_side_effect_entry(
        &self,
        module: ModuleName,
        imports: &AHashSet<ModuleName>,
        has_failing_deps: &AHashSet<ModuleName>,
    ) {
        // Most modules have no side-effect imports. Check both conditions before
        // taking a `lazy_eligible` shard lock to avoid needless contention.
        if imports.is_empty() || !self.summary.passing_modules.contains(&module) {
            return;
        }
        self.output.lazy_eligible.entry(module).or_default().extend(
            imports
                .iter()
                .filter(|import| has_failing_deps.contains(*import))
                .copied(),
        );
    }

    pub fn get_report(&self) -> String {
        self.summary.report(self.output.load_imports_eagerly.len())
    }

    pub fn print_diagnostics(&self) {
        self.summary.print_diagnostics();
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
            (!members.is_empty()).then_some(members)
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
                .filter(|m| *m != module_name && cycle_set.contains(m))
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

#[cfg(test)]
mod tests {
    use ruff_text_size::TextRange;
    use ruff_text_size::TextSize;

    use super::*;
    use crate::errors::SafetyError;
    use crate::exports::Attribute;
    use crate::module_safety::ModuleSafety;
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

    #[test]
    fn test_cycle_deps_skip_self_edges() {
        let mut import_graph = ImportGraph::new();
        let a = mn("a");

        import_graph.graph.add_node(&a);
        import_graph.graph.add_edge(&a, &a);

        let mut classified = ClassifiedModules {
            failing_modules: SmallSet::new(),
            passing_modules: SmallSet::new(),
            load_imports_eagerly: SmallSet::new(),
            implicit_imports: AHashMap::new(),
            aggregated_errors: AHashMap::new(),
        };
        classified.passing_modules.insert(a);

        let re_export_map = AHashMap::new();
        let lazy_eligible =
            build_lazy_eligible(&import_graph, &classified, &re_export_map, &[vec![a]]);

        let deps = lazy_eligible.get(&a).unwrap();
        assert!(
            !deps.contains(&a),
            "a module should not list itself as a cycle dependency",
        );
    }

    #[test]
    fn test_side_effect_imports_do_not_observe_same_pass_updates() {
        let options = Options {
            sorted_output: true,
            ..Options::default()
        };
        let exports = Exports::empty();

        for iteration in 0..64 {
            let safety_map = SafetyMap::new();
            let mut import_graph = ImportGraph::new();
            let mut side_effect_imports: SideEffectMap = AHashMap::new();
            let mut chains = Vec::new();

            for chain_idx in 0..16 {
                let outer = mn(&format!("outer_{iteration}_{chain_idx}"));
                let middle = mn(&format!("middle_{iteration}_{chain_idx}"));
                let inner = mn(&format!("inner_{iteration}_{chain_idx}"));
                let leaf = mn(&format!("leaf_{iteration}_{chain_idx}"));

                import_graph.graph.add_node(&outer);
                import_graph.graph.add_node(&middle);
                import_graph.graph.add_node(&inner);
                import_graph.graph.add_node(&leaf);
                import_graph.graph.add_edge(&outer, &middle);
                import_graph.graph.add_edge(&middle, &inner);
                import_graph.graph.add_edge(&inner, &leaf);

                safety_map.insert(outer, SafetyResult::Ok(ModuleSafety::new()));
                safety_map.insert(middle, SafetyResult::Ok(ModuleSafety::new()));
                safety_map.insert(inner, SafetyResult::Ok(ModuleSafety::new()));

                let mut failing = ModuleSafety::new();
                failing.add_error(make_error(
                    ErrorKind::UnknownDecoratorCall,
                    "unknown-decorator-call",
                    chain_idx,
                ));
                safety_map.insert(leaf, SafetyResult::Ok(failing));

                side_effect_imports.insert(middle, [inner].into_iter().collect());
                side_effect_imports.insert(outer, [middle].into_iter().collect());
                chains.push((outer, middle, inner));
            }

            let analysis = LifeGuardAnalysis::from_whole_program(
                safety_map,
                import_graph,
                &exports,
                &side_effect_imports,
                &options,
            );

            for (outer, middle, inner) in chains {
                let middle_deps = analysis.output.lazy_eligible.get(&middle).unwrap();
                assert!(
                    middle_deps.contains(&inner),
                    "{middle} should be guarded by {inner}",
                );

                let outer_deps = analysis.output.lazy_eligible.get(&outer).unwrap();
                assert!(
                    !outer_deps.contains(&middle),
                    "{outer} should not observe side-effect deps added to {middle} during the same propagation pass",
                );
            }
        }
    }

    // ---- build_re_export_map tests ----

    fn attr(module: &str, name: &str) -> Attribute {
        Attribute::new(mn(module), name)
    }

    #[test]
    fn test_re_export_map_single_hop() {
        // A re-exports Foo from B, B is failing
        let mut exports = Exports::empty();
        exports.insert_re_export(attr("a", "Foo"), attr("b", "Foo"));

        let mut failing = SmallSet::new();
        failing.insert(mn("b"));

        let map = build_re_export_map(&exports, &failing);
        assert!(map[&mn("a")].contains(&mn("b")));
    }

    #[test]
    fn test_re_export_map_three_hops() {
        // A -> B -> C -> D, D is failing
        let mut exports = Exports::empty();
        exports.insert_re_export(attr("a", "Foo"), attr("b", "Foo"));
        exports.insert_re_export(attr("b", "Foo"), attr("c", "Foo"));
        exports.insert_re_export(attr("c", "Foo"), attr("d", "Foo"));

        let mut failing = SmallSet::new();
        failing.insert(mn("d"));

        let map = build_re_export_map(&exports, &failing);
        assert!(map[&mn("a")].contains(&mn("d")));
        assert!(map[&mn("b")].contains(&mn("d")));
        assert!(map[&mn("c")].contains(&mn("d")));
    }

    #[test]
    fn test_re_export_map_no_failing() {
        // A re-exports from B, but B is not failing
        let mut exports = Exports::empty();
        exports.insert_re_export(attr("a", "Foo"), attr("b", "Foo"));

        let failing = SmallSet::new();
        let map = build_re_export_map(&exports, &failing);
        assert!(map.is_empty());
    }

    #[test]
    fn test_re_export_map_cycle() {
        // A -> B -> A (cycle), A is failing
        let mut exports = Exports::empty();
        exports.insert_re_export(attr("a", "Foo"), attr("b", "Foo"));
        exports.insert_re_export(attr("b", "Foo"), attr("a", "Foo"));

        let mut failing = SmallSet::new();
        failing.insert(mn("a"));

        // Should not panic; cycle detection should handle this
        let map = build_re_export_map(&exports, &failing);
        // B re-exports from A which is failing — but the chain B->A->B is a cycle,
        // so resolve_transitive returns None and B is not in the map
        assert!(!map.contains_key(&mn("b")));
    }

    #[test]
    fn test_re_export_map_from_cache_three_hops() {
        let re_exports = vec![
            CachedReExport {
                exported_module: mn("a"),
                exported_attr: "Foo".to_string(),
                imported_module: mn("b"),
                imported_attr: "Foo".to_string(),
            },
            CachedReExport {
                exported_module: mn("b"),
                exported_attr: "Foo".to_string(),
                imported_module: mn("c"),
                imported_attr: "Foo".to_string(),
            },
            CachedReExport {
                exported_module: mn("c"),
                exported_attr: "Foo".to_string(),
                imported_module: mn("d"),
                imported_attr: "Foo".to_string(),
            },
        ];

        let mut failing = SmallSet::new();
        failing.insert(mn("d"));

        let map = build_re_export_map_from_cache(&re_exports, &failing);
        assert!(map[&mn("a")].contains(&mn("d")));
        assert!(map[&mn("b")].contains(&mn("d")));
        assert!(map[&mn("c")].contains(&mn("d")));
    }

    #[test]
    fn test_re_export_map_from_cache_cycle() {
        let re_exports = vec![
            CachedReExport {
                exported_module: mn("a"),
                exported_attr: "Foo".to_string(),
                imported_module: mn("b"),
                imported_attr: "Foo".to_string(),
            },
            CachedReExport {
                exported_module: mn("b"),
                exported_attr: "Foo".to_string(),
                imported_module: mn("a"),
                imported_attr: "Foo".to_string(),
            },
        ];

        let mut failing = SmallSet::new();
        failing.insert(mn("a"));

        let map = build_re_export_map_from_cache(&re_exports, &failing);
        assert!(!map.contains_key(&mn("b")));
    }

    // ---- get_report tests ----

    #[test]
    fn test_get_report_format() {
        let analysis = LifeGuardAnalysis {
            output: LifeGuardOutput::new(true),
            summary: AnalysisSummary {
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
            },
        };

        let report = analysis.get_report();
        assert!(report.contains("66.67 %"));
        assert!(report.contains("Num of failing files: 1"));
        assert!(report.contains("Num of passing files: 2"));
    }
}
