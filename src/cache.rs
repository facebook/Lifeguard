/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_text_size::TextRange;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;

use crate::config::AnalysisConfig;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::exports::Exports;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::FixedState;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::hasher::union_larger;
use crate::imports::ImportGraph;
use crate::imports::resolve_to_known_module;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::ModuleSafety;
use crate::module_safety::MutationCandidate;
use crate::module_safety::SafetyResult;
use crate::mro::c3_linearize;
use crate::project::SafetyMap;
use crate::project::SideEffectMap;
use crate::pyrefly::sys_info::PythonVersion;
use crate::resolution::ResolutionOutcome;
use crate::resolution::resolve_program;
use crate::resolution::unqualified_index_key;
use crate::source_map::bundled_stub_sources;
use crate::traits::ModuleNameExt;

/// Cached analysis results for a single Python library.
/// Contains all information needed to merge with other libraries
/// in a map-reduce analysis pipeline.
#[derive(Serialize, Deserialize)]
pub struct LibraryCache {
    pub modules: Vec<CachedModule>,
    pub exports: CachedExports,
    /// Class FQN -> base-class FQNs, for reduce-time MRO resolution of inherited
    /// `Class.method` calls.
    #[serde(default)]
    pub class_bases: Vec<(ModuleName, Vec<ModuleName>)>,
}

/// Cached analysis for a single module within a library.
#[derive(Serialize, Deserialize)]
pub struct CachedModule {
    pub name: ModuleName,
    pub safety: CachedSafety,
    /// Resolved imports (edges in the import graph).
    pub imports: AHashSet<ModuleName>,
    /// Imports that could not be resolved to modules in the source DB.
    pub missing_imports: AHashSet<ModuleName>,
    /// `from X import Y` where X is in the library but X.Y is not.
    /// May be a submodule in another library or an attribute of X.
    pub ambiguous_imports: AHashSet<ModuleName>,
    /// Module-level imports never accessed in any scope (side-effect imports).
    pub side_effect_imports: AHashSet<ModuleName>,
    /// Per-function safety info from call graph analysis.
    /// Keys are function-local names (e.g., "helper" for `mod.helper`).
    pub function_safety: AHashMap<String, FunctionSafetyInfo>,
    /// Calls passing imported objects to cross-library-unresolved callees,
    /// resolved against the merged cache in the reduce step.
    pub mutation_candidates: Vec<MutationCandidate>,
}

/// Safety analysis result for a cached module.
#[derive(Serialize, Deserialize)]
pub enum CachedSafety {
    Ok(CachedModuleSafety),
    AnalysisError { message: String },
}

/// Detailed safety information for a module.
#[derive(Default, Serialize, Deserialize)]
pub struct CachedModuleSafety {
    pub errors: Vec<CachedError>,
    pub force_imports_eager_overrides: Vec<CachedError>,
    pub implicit_imports: Vec<ModuleName>,
}

/// A serializable safety error (without source location).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct CachedError {
    pub kind: ErrorKind,
    pub metadata: String,
    pub parameterized_decorator: bool,
}

/// Cached re-export information for a library. Only re-exports are consumed by
/// the reduce (`analyze-binary`); the map phase's other export tables
/// (definitions/`__all__`/return types) are not, so they are not cached.
#[derive(Serialize, Deserialize)]
pub struct CachedExports {
    pub re_exports: Vec<CachedReExport>,
}

/// A cached re-export entry (module.attr -> source_module.source_attr).
#[derive(Serialize, Deserialize)]
pub struct CachedReExport {
    pub exported_module: ModuleName,
    pub exported_attr: String,
    pub imported_module: ModuleName,
    pub imported_attr: String,
}

/// Working map used to dedup re-exports during the reduce, keyed by the exported
/// `(module, attr)`; the value is one representative record per unique key.
type ReExportDedupMap = AHashMap<ModuleName, AHashMap<String, CachedReExport>>;

/// A module's import edges from the graph, partitioned by resolution status.
struct GraphEdgeSets {
    imports: AHashSet<ModuleName>,
    missing_imports: AHashSet<ModuleName>,
    ambiguous_imports: AHashSet<ModuleName>,
}

fn graph_edge_sets(graph: &ImportGraph, name: &ModuleName) -> GraphEdgeSets {
    let imports = graph.get_imports(name).copied().collect();
    let missing_imports = graph
        .get_missing_imports(name)
        .map(|m| m.iter().copied().collect())
        .unwrap_or_default();
    let ambiguous_imports = graph
        .get_ambiguous_imports(name)
        .map(|m| m.iter().copied().collect())
        .unwrap_or_default();
    GraphEdgeSets {
        imports,
        missing_imports,
        ambiguous_imports,
    }
}

/// Resolve `name` against the merged module set and, when it resolves to a module
/// other than `from`, add it to `imports` as a real edge. Self-edges are skipped
/// to mirror the whole-program builder's `try_add_edge` (which rejects them), so a
/// module resolving a missing or ambiguous submodule import to itself never
/// becomes its own dependency. Returns the resolved target — including a
/// self-resolution, which callers still record for cross-library error clearing —
/// or `None` when `name` does not resolve.
fn resolve_and_add_import_edge(
    imports: &mut AHashSet<ModuleName>,
    from: ModuleName,
    name: &ModuleName,
    module_names: &AHashSet<ModuleName>,
) -> Option<ModuleName> {
    let resolved = resolve_to_known_module(name, module_names)?;
    if resolved != from {
        imports.insert(resolved);
    }
    Some(resolved)
}

impl LibraryCache {
    pub fn empty() -> Self {
        LibraryCache {
            modules: Vec::new(),
            exports: CachedExports {
                re_exports: Vec::new(),
            },
            class_bases: Vec::new(),
        }
    }

    /// Build a cache from the analysis pipeline results.
    pub fn build(
        safety_map: &SafetyMap,
        import_graph: &ImportGraph,
        exports: &Exports,
        side_effect_imports: &SideEffectMap,
    ) -> Self {
        let mut modules: Vec<CachedModule> = safety_map
            .par_iter()
            .map(|entry| {
                let name = *entry.key();
                let safety_result = entry.value();

                let GraphEdgeSets {
                    imports,
                    missing_imports,
                    ambiguous_imports,
                } = graph_edge_sets(import_graph, &name);

                let se_imports: AHashSet<ModuleName> = side_effect_imports
                    .get(&name)
                    .map(|s| s.iter().copied().collect())
                    .unwrap_or_default();

                let (function_safety, mutation_candidates) = match safety_result {
                    SafetyResult::Ok(ms) => {
                        (ms.function_safety.clone(), ms.mutation_candidates.clone())
                    }
                    _ => (AHashMap::new(), Vec::new()),
                };

                let safety = CachedSafety::from_safety_result(safety_result);

                CachedModule {
                    name,
                    safety,
                    imports,
                    missing_imports,
                    ambiguous_imports,
                    side_effect_imports: se_imports,
                    function_safety,
                    mutation_candidates,
                }
            })
            .collect();

        modules.sort_by_key(|m| m.name);

        let own_modules: AHashSet<ModuleName> = modules.iter().map(|m| m.name).collect();
        let exports = CachedExports::from_exports(exports, &own_modules);

        LibraryCache {
            modules,
            exports,
            class_bases: Vec::new(),
        }
    }

    /// Attach class base edges (class FQN -> base FQNs) for MRO resolution during
    /// the reduce step. Populated by the map phase (`analyze-library`).
    pub fn set_class_bases(&mut self, class_bases: Vec<(ModuleName, Vec<ModuleName>)>) {
        self.class_bases = class_bases;
    }

    /// Write the cache using the indexed wire format.
    pub fn write_to_file(&self, path: &Path) -> anyhow::Result<()> {
        crate::cache_wire::write(self, path)
    }

    /// Read a cache using the indexed wire format.
    pub fn read_from_file(path: &Path) -> anyhow::Result<Self> {
        crate::cache_wire::read(path)
    }

    /// Merge dependency caches into this cache.
    /// When the same module appears in multiple caches (a .py file can belong
    /// to more than one python_library), module data is merged:
    /// - imports / side_effect_imports: union
    /// - missing_imports: intersection (only truly missing if unresolved everywhere)
    /// - safety: most conservative (most errors)
    pub fn merge_dep_caches(&mut self, dep_caches: Vec<LibraryCache>) {
        let extra_modules: usize = dep_caches.iter().map(|d| d.modules.len()).sum();
        self.modules.reserve(extra_modules);

        // Split each dep cache into its modules (appended serially — cheap) and
        // its re-export batch (deduped in parallel below).
        let mut re_export_batches: Vec<Vec<CachedReExport>> =
            Vec::with_capacity(dep_caches.len() + 1);
        re_export_batches.push(std::mem::take(&mut self.exports.re_exports));
        for dep in dep_caches {
            self.modules.extend(dep.modules);
            re_export_batches.push(dep.exports.re_exports);
            self.class_bases.extend(dep.class_bases);
        }

        // A module's re-exports recur across many caches, far outnumbering the
        // unique set. Dedup by exported `(module, attr)` in parallel: each task
        // folds a batch into a local map (cloning the attr only on first sight),
        // then the per-task maps are unioned. Duplicates for a key are identical,
        // so keeping any one is correct.
        let deduped_map = re_export_batches
            .into_par_iter()
            .fold(ReExportDedupMap::default, |mut map, batch| {
                for re in batch {
                    let attrs = map.entry(re.exported_module).or_default();
                    if !attrs.contains_key(re.exported_attr.as_str()) {
                        attrs.insert(re.exported_attr.clone(), re);
                    }
                }
                map
            })
            .reduce(ReExportDedupMap::default, |a, b| {
                // Union the smaller map into the larger to minimize rehashing.
                // Compare by total `(module, attr)` entries, not outer `len()` (the
                // module count), so a few-modules/many-attrs map isn't mistaken for
                // the smaller side.
                let entries =
                    |m: &ReExportDedupMap| m.values().map(|attrs| attrs.len()).sum::<usize>();
                let (mut large, small) = if entries(&a) >= entries(&b) {
                    (a, b)
                } else {
                    (b, a)
                };
                for (module, attrs) in small {
                    let dst = large.entry(module).or_default();
                    for (attr, re) in attrs {
                        dst.entry(attr).or_insert(re);
                    }
                }
                large
            });
        self.exports.re_exports = deduped_map
            .into_values()
            .flat_map(|attrs| attrs.into_values())
            .collect();

        // The module sort+coalesce and the re-export sort are independent (disjoint
        // fields), so run them concurrently instead of back to back.
        let modules = &mut self.modules;
        let exports = &mut self.exports;
        rayon::join(
            || {
                modules.par_sort_by_key(|m| m.name);
                Self::merge_duplicate_modules(modules);
            },
            // Sort the (already-deduped, much smaller) set for a stable output order.
            || exports.sort_and_dedup(),
        );
    }

    /// Merge consecutive modules with the same name (assumes sorted by name).
    fn merge_duplicate_modules(modules: &mut Vec<CachedModule>) {
        if modules.len() < 2 {
            return;
        }

        let mut write = 0;
        for read in 1..modules.len() {
            if modules[write].name == modules[read].name {
                let name = modules[read].name;
                let other = std::mem::replace(&mut modules[read], CachedModule::empty(name));
                modules[write].merge(other);
            } else {
                write += 1;
                if write != read {
                    modules.swap(write, read);
                }
            }
        }
        modules.truncate(write + 1);
    }

    /// Resolve ambiguous imports: `from X import Y` where X was in the library
    /// but X.Y was not. If X.Y resolves to a module in the merged set, it's a
    /// submodule — add it as a real import edge.
    /// Returns a map of module → newly resolved targets for downstream error clearing.
    fn resolve_ambiguous_imports(
        &mut self,
        module_names: &AHashSet<ModuleName>,
    ) -> AHashMap<ModuleName, AHashSet<ModuleName>> {
        self.modules
            .par_iter_mut()
            .filter_map(|module| {
                let mut resolved = AHashSet::new();
                for ambiguous in module.ambiguous_imports.drain() {
                    if let Some(target) = resolve_and_add_import_edge(
                        &mut module.imports,
                        module.name,
                        &ambiguous,
                        module_names,
                    ) {
                        resolved.insert(target);
                    }
                }
                (!resolved.is_empty()).then_some((module.name, resolved))
            })
            .collect()
    }

    /// Clear cached errors verified safe by the completed resolution outcome.
    /// General errors require positive resolution evidence; decorator errors
    /// can be verified from static verdicts alone.
    fn finalize_resolution(
        &mut self,
        module_names: &AHashSet<ModuleName>,
        func_safety_by_module: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
        outcome: &ResolutionOutcome,
        class_bases: &HashMap<ModuleName, Vec<ModuleName>>,
    ) {
        let decorator_scan_cache: DashMap<String, bool, FixedState> = DashMap::default();
        if !outcome.promoted.is_empty() || outcome.resolved_to_safe {
            // With positive evidence (a promotion or a mutation candidate now
            // `Safe`), clear every verified-safe error kind.
            let resolver = SafetyResolver::with_safe_index(
                module_names,
                func_safety_by_module,
                &outcome.globally_safe,
            )
            .with_decorator_cache(&decorator_scan_cache)
            .with_class_bases(class_bases);
            self.clear_errors_where(|caller, error| resolver.clears_error(caller, error, |_| true));
        } else {
            // Without promotion evidence, clear only static-safe kinds:
            // `UnsafeDecoratorCall` via the general verdict, plus (always, inside
            // `clears_error`) constructor-shaped `UnsafeFunctionCall` and
            // class-decorator calls, whose safety follows from static verdicts alone.
            // These checks ignore the globally-safe index, so an empty one suffices.
            let empty = AHashSet::new();
            let resolver =
                SafetyResolver::with_safe_index(module_names, func_safety_by_module, &empty)
                    .with_decorator_cache(&decorator_scan_cache)
                    .with_class_bases(class_bases);
            self.clear_errors_where(|caller, error| {
                resolver.clears_error(caller, error, |kind| kind == ErrorKind::UnsafeDecoratorCall)
            });
        }
        debug!("{} functions promoted", outcome.promoted.len());
    }

    /// Collect error names that can use the global unqualified fallback; qualified
    /// names resolve through module-specific safety maps instead.
    fn unqualified_error_names(&self) -> AHashSet<String> {
        self.modules
            .par_iter()
            .filter_map(|module| match &module.safety {
                CachedSafety::Ok(safety) => Some(safety),
                CachedSafety::AnalysisError { .. } => None,
            })
            .fold(AHashSet::new, |mut names, safety| {
                for error in &safety.errors {
                    let Some(name) = unqualified_index_key(&error.metadata) else {
                        continue;
                    };
                    if !names.contains(name) {
                        names.insert(name.to_owned());
                    }
                }
                names
            })
            .reduce(AHashSet::new, union_larger)
    }

    /// Drop every error `should_clear` admits, in parallel. Returns whether any
    /// error was removed.
    fn clear_errors_where(
        &mut self,
        should_clear: impl Fn(ModuleName, &CachedError) -> bool + Sync,
    ) -> bool {
        self.modules
            .par_iter_mut()
            .map(|module| {
                let caller = module.name;
                let CachedSafety::Ok(ref mut safety) = module.safety else {
                    return false;
                };
                retain_unverified_errors(safety, |error| should_clear(caller, error))
            })
            .reduce(|| false, |any_cleared, cleared| any_cleared || cleared)
    }

    /// Resolve missing imports against the merged cache and selectively clear
    /// false errors using per-function safety verdicts.
    pub fn resolve_cross_library_errors(&mut self) {
        let module_names: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let ambiguous_resolved = self.resolve_ambiguous_imports(&module_names);

        let class_bases = merge_class_bases(std::mem::take(&mut self.class_bases));

        self.propagate_re_export_safety();

        let mut func_safety_by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            self.modules
                .iter_mut()
                .map(|m| (m.name, std::mem::take(&mut m.function_safety)))
                .collect();

        self.modules.par_iter_mut().for_each(|module| {
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                dedupe_implicit_imports(&mut safety.implicit_imports);
            }

            let from_ambiguous = ambiguous_resolved.get(&module.name);

            if module.missing_imports.is_empty() && from_ambiguous.is_none() {
                return;
            }

            let mut still_missing: AHashSet<ModuleName> =
                AHashSet::with_capacity(module.missing_imports.len());
            let mut resolved_modules: AHashSet<ModuleName> =
                AHashSet::with_capacity(module.missing_imports.len());

            if let Some(from_ambiguous) = from_ambiguous {
                resolved_modules.extend(from_ambiguous.iter().copied());
            }

            for missing in module.missing_imports.drain() {
                match resolve_and_add_import_edge(
                    &mut module.imports,
                    module.name,
                    &missing,
                    &module_names,
                ) {
                    Some(resolved) => {
                        resolved_modules.insert(resolved);
                    }
                    None => {
                        still_missing.insert(missing);
                    }
                }
            }

            module.missing_imports = still_missing;

            if let CachedSafety::Ok(ref mut safety) = module.safety {
                let resolver = SafetyResolver::new(&resolved_modules, &func_safety_by_module)
                    .with_class_bases(&class_bases);
                retain_unverified_errors(safety, |error| resolver.is_error_verified_safe(error));
            }
        });

        let needed_unqualified = self.unqualified_error_names();
        let mut module_errors: HashMap<ModuleName, Vec<String>> = HashMap::new();
        let outcome = resolve_program(
            &module_names,
            &mut func_safety_by_module,
            self.modules
                .iter()
                .map(|module| (module.name, module.mutation_candidates.as_slice())),
            needed_unqualified,
            |module_name, metadata| {
                module_errors.entry(module_name).or_default().push(metadata);
            },
        );
        for module in &mut self.modules {
            let Some(errors) = module_errors.get(&module.name) else {
                continue;
            };
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                safety
                    .errors
                    .extend(errors.iter().map(|metadata| CachedError {
                        kind: ErrorKind::ImportedVarArgument,
                        metadata: metadata.clone(),
                        parameterized_decorator: false,
                    }));
            }
        }

        self.finalize_resolution(
            &module_names,
            &func_safety_by_module,
            &outcome,
            &class_bases,
        );

        // Return the verdicts taken at the top; resolution needed them in one flat
        // map to do cross-module lookups while `self.modules` was borrowed mutably.
        for module in &mut self.modules {
            if let Some(fs) = func_safety_by_module.remove(&module.name) {
                module.function_safety = fs;
            }
        }
    }

    /// Propagate function_safety entries through re-exports.
    /// If module B re-exports `foo` from module C, and C has
    /// function_safety["foo"] = Safe, then B should also get that entry.
    #[doc(hidden)]
    pub fn propagate_re_export_safety(&mut self) {
        let module_index: AHashMap<ModuleName, usize> = self
            .modules
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name, i))
            .collect();

        // Worklist over the re-export edges: when an edge changes its
        // destination verdict, only edges reading from that destination can
        // need reprocessing. Merge is monotone, so this reaches the same
        // fixpoint as rescanning every edge each round while revisiting far
        // fewer. Edges are moved out so they can be read while `self.modules` is
        // mutated (disjoint fields), then restored at the end.
        let re_exports = std::mem::take(&mut self.exports.re_exports);

        // Maps a source `(module, attr)` to the edges that read from it.
        let mut dependents: AHashMap<(ModuleName, &str), Vec<u32>> =
            AHashMap::with_capacity(re_exports.len());
        for (i, re) in re_exports.iter().enumerate() {
            dependents
                .entry((re.imported_module, re.imported_attr.as_str()))
                .or_default()
                .push(i as u32);
        }

        let mut queued = vec![true; re_exports.len()];
        let mut worklist: Vec<u32> = (0..re_exports.len() as u32).collect();

        while let Some(i) = worklist.pop() {
            queued[i as usize] = false;
            let re = &re_exports[i as usize];

            let Some(&src_idx) = module_index.get(&re.imported_module) else {
                continue;
            };
            let Some(&dst_idx) = module_index.get(&re.exported_module) else {
                continue;
            };

            // The re-exported symbol inherits the union of its source's concerns.
            // Cross-module edges borrow source and destination disjointly and merge
            // by reference; a rare self-re-export clones to avoid the aliasing.
            let dest_changed = if src_idx == dst_idx {
                let Some(safety) = self.modules[src_idx]
                    .function_safety
                    .get(re.imported_attr.as_str())
                    .cloned()
                else {
                    continue;
                };
                merge_function_safety_entry_ref(
                    &mut self.modules[dst_idx].function_safety,
                    &re.exported_attr,
                    &safety,
                )
            } else {
                let [src_mod, dst_mod] = self
                    .modules
                    .get_disjoint_mut([src_idx, dst_idx])
                    .expect("src_idx and dst_idx are distinct, in-bounds module indices");
                let Some(src_info) = src_mod.function_safety.get(re.imported_attr.as_str()) else {
                    continue;
                };
                merge_function_safety_entry_ref(
                    &mut dst_mod.function_safety,
                    &re.exported_attr,
                    src_info,
                )
            };

            // Only edges reading from this destination can now change; re-queue them.
            if dest_changed
                && let Some(deps) = dependents.get(&(re.exported_module, re.exported_attr.as_str()))
            {
                for &j in deps {
                    if !queued[j as usize] {
                        queued[j as usize] = true;
                        worklist.push(j);
                    }
                }
            }
        }

        // `dependents` borrows `re_exports`; drop it before moving them back.
        drop(dependents);
        self.exports.re_exports = re_exports;
    }

    /// Inject the bundled stdlib stubs as graph-only nodes so the merged graph
    /// matches the e2e graph: per-library caches drop stub-only modules, losing
    /// the typeshed import cycle. Skips names a real library already provides.
    /// Returns the injected names so the caller can keep them out of the safety map.
    pub fn inject_bundled_stub_graph(
        &mut self,
        python_version: PythonVersion,
    ) -> AHashSet<ModuleName> {
        let sources = bundled_stub_sources(python_version);
        let config = AnalysisConfig::with_python_version(python_version, None);
        let graph = ImportGraph::make(&sources, &config);

        let existing: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let mut added = AHashSet::new();

        for name in graph.graph.node_names() {
            let name = *name;
            if existing.contains(&name) {
                continue;
            }
            let GraphEdgeSets {
                imports,
                missing_imports,
                ambiguous_imports,
            } = graph_edge_sets(&graph, &name);
            self.modules.push(CachedModule {
                imports,
                missing_imports,
                ambiguous_imports,
                ..CachedModule::empty(name)
            });
            added.insert(name);
        }
        added
    }

    /// Reconstruct an ImportGraph from cached module import edges.
    pub fn to_import_graph(&self) -> ImportGraph {
        let mut graph = ImportGraph::new();
        for module in &self.modules {
            graph.graph.add_node(&module.name);
        }
        for module in &self.modules {
            for imported in &module.imports {
                graph.graph.add_edge(&module.name, imported);
            }
            for missing in &module.missing_imports {
                graph.add_missing(&module.name, *missing);
            }
        }
        graph
    }
}

/// Drop errors on `safety` that `is_verified_safe` confirms are safe, leaving
/// the rest. Returns whether any error was removed.
fn retain_unverified_errors(
    safety: &mut CachedModuleSafety,
    mut is_verified_safe: impl FnMut(&CachedError) -> bool,
) -> bool {
    let before = safety.errors.len();
    safety
        .errors
        .retain(|e| !e.kind.could_be_caused_by_missing_import() || !is_verified_safe(e));
    safety.errors.len() < before
}

/// Fold accumulated `(class FQN, bases)` pairs into a lookup map, merging any
/// FQN contributed by more than one library (e.g. a stub and the real module,
/// or overlapping targets). Bases are unioned preserving first-seen order so the
/// result is independent of dep-cache append order — a bare `collect()` would
/// instead keep whichever tuple landed last.
fn merge_class_bases(
    entries: Vec<(ModuleName, Vec<ModuleName>)>,
) -> HashMap<ModuleName, Vec<ModuleName>> {
    let mut merged: HashMap<ModuleName, Vec<ModuleName>> = HashMap::with_capacity(entries.len());
    for (class_fqn, bases) in entries {
        let existing = merged.entry(class_fqn).or_default();
        for base in bases {
            if !existing.contains(&base) {
                existing.push(base);
            }
        }
    }
    merged
}

/// Whether `local_name` is cached `Safe` in `fs`.
/// Inherited methods resolve up the class MRO in the resolver
/// (`SafetyResolver::mro_method_verdict`).
fn lookup_in_safety_map(local_name: &str, fs: &AHashMap<String, FunctionSafetyInfo>) -> bool {
    fs.get(local_name)
        .is_some_and(|info| info.verdict.is_safe())
}

/// The merged per-function verdicts plus the module set they resolve against —
/// shared context for reduce-time error clearing and promotion.
///
/// A qualified name (`mod.sub.func`) is split at its longest module prefix and
/// looked up there; an unqualified name (`helper`) uses `globally_safe` if
/// present, else scans `modules`.
#[derive(Clone, Copy)]
struct SafetyResolver<'a> {
    modules: &'a AHashSet<ModuleName>,
    by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    /// `Some` enables O(1) unqualified lookups; `None` scans `modules`.
    globally_safe: Option<&'a AHashSet<String>>,
    /// Caches `scan_unqualified_decorator_safe` by name, so its O(modules) scan
    /// runs once per distinct decorator instead of once per call site.
    decorator_scan_cache: Option<&'a DashMap<String, bool, FixedState>>,
    /// Class FQN -> base FQNs, enabling MRO resolution of inherited
    /// `Class.method` calls when there is no exact method verdict.
    class_bases: Option<&'a HashMap<ModuleName, Vec<ModuleName>>>,
}

impl<'a> SafetyResolver<'a> {
    /// No prebuilt indices — the unqualified fallback scans `modules`.
    fn new(
        modules: &'a AHashSet<ModuleName>,
        by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    ) -> Self {
        SafetyResolver {
            modules,
            by_module,
            globally_safe: None,
            decorator_scan_cache: None,
            class_bases: None,
        }
    }

    /// Backed by the prebuilt globally-safe index for O(1) unqualified lookups.
    fn with_safe_index(
        modules: &'a AHashSet<ModuleName>,
        by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
        globally_safe: &'a AHashSet<String>,
    ) -> Self {
        SafetyResolver {
            modules,
            by_module,
            globally_safe: Some(globally_safe),
            decorator_scan_cache: None,
            class_bases: None,
        }
    }

    fn with_decorator_cache(mut self, cache: &'a DashMap<String, bool, FixedState>) -> Self {
        self.decorator_scan_cache = Some(cache);
        self
    }

    /// Attach class base edges.
    fn with_class_bases(mut self, class_bases: &'a HashMap<ModuleName, Vec<ModuleName>>) -> Self {
        self.class_bases = Some(class_bases);
        self
    }

    /// Resolve `local` = `Class.method` (or `Outer.Inner.method`) up the MRO of
    /// `module`.`Class`: return the verdict of the first ancestor, in C3 method
    /// resolution order, that defines an exact `Base.method` entry, or `None` if
    /// no reachable ancestor defines it. `Class` itself is skipped — its own
    /// method is checked by the caller before falling back to the MRO.
    fn mro_method_verdict(&self, module: &ModuleName, local: &str) -> Option<FunctionSafety> {
        let class_bases = self.class_bases?;
        let (class_local, method) = local.rsplit_once('.')?;
        let class_fqn = module.append_str(class_local);
        for ancestor in c3_linearize(class_bases, &class_fqn).iter().skip(1) {
            let candidate = ancestor.append_str(method);
            if let Some((bmod, blocal)) = self.split_at_module(candidate.as_str()) {
                if let Some(info) = self.by_module.get(&bmod).and_then(|fs| fs.get(blocal)) {
                    return Some(info.verdict);
                }
            }
        }
        None
    }

    /// The longest prefix of `func_name` naming a module in `self.modules`,
    /// paired with the remaining local name; `None` if unqualified.
    fn split_at_module<'n>(&self, func_name: &'n str) -> Option<(ModuleName, &'n str)> {
        let fqn = ModuleName::from_str(func_name);
        fqn.iter_parents()
            .find(|(parent, _)| self.modules.contains(parent))
            .map(|(parent, dot_pos)| (parent, &func_name[dot_pos + 1..]))
    }

    /// Whether an unqualified name is verified safe: the index when present,
    /// else a scan of `modules`.
    fn unqualified_safe(&self, func_name: &str) -> bool {
        if let Some(index) = self.globally_safe {
            return index.contains(func_name);
        }
        self.modules
            .iter()
            .filter_map(|m| self.by_module.get(m))
            .filter_map(|fs| fs.get(func_name))
            .any(|info| info.verdict.is_safe())
    }

    /// `module`'s own verdict for `local`, or `None` when it has no such entry.
    /// The MRO must only be walked when the class itself does not define the method.
    fn own_verdict(&self, module: &ModuleName, local: &str) -> Option<FunctionSafety> {
        Some(self.by_module.get(module)?.get(local)?.verdict)
    }

    /// Whether `module` has an own entry for `local` verified `Safe`.
    fn own_call_safe(&self, module: &ModuleName, local: &str) -> bool {
        self.own_verdict(module, local).is_some_and(|v| v.is_safe())
    }

    /// Whether a plain function call is found and verified `Safe`.
    fn is_call_verified_safe(&self, func_name: &str) -> bool {
        match self.split_at_module(func_name) {
            Some((module, local)) => match self.own_verdict(&module, local) {
                Some(verdict) => verdict.is_safe(),
                None => self
                    .mro_method_verdict(&module, local)
                    .is_some_and(|v| v.is_safe()),
            },
            None => self.unqualified_safe(func_name),
        }
    }

    /// Like `is_call_verified_safe`, but only an own entry under a
    /// module-qualified name clears. Both restrictions follow from an `Unknown*`
    /// call target meaning `func_name` is a best-effort textual name rather than a
    /// proven callee:
    /// - an unqualified name must not clear on a same-named safe function in
    ///   some resolved module (or in the global index);
    /// - the MRO fallback does not apply, since walking a class hierarchy for a
    ///   name that was never bound to that class is speculative.
    fn is_call_verified_safe_no_unqualified(&self, func_name: &str) -> bool {
        self.split_at_module(func_name)
            .is_some_and(|(module, local)| self.own_call_safe(&module, local))
    }

    /// Whether a parameterized-decorator call is safe: the factory AND every
    /// immediate nested function must be `Safe`, since the factory runs its
    /// returned wrapper at decoration time. Never consults `globally_safe`
    /// (own-verdict only).
    fn is_decorator_call_verified_safe(&self, func_name: &str) -> bool {
        if let Some((module, local)) = self.split_at_module(func_name) {
            return self
                .by_module
                .get(&module)
                .is_some_and(|fs| lookup_decorator_in_safety_map(local, fs));
        }
        let Some(cache) = self.decorator_scan_cache else {
            return self.scan_unqualified_decorator_safe(func_name);
        };
        if let Some(cached) = cache.get(func_name) {
            return *cached;
        }
        let result = self.scan_unqualified_decorator_safe(func_name);
        cache.insert(func_name.to_owned(), result);
        result
    }

    /// Whether any module has `func_name` as a decorator-verified-safe function.
    /// O(modules); callers should memoize by name (see `decorator_scan_cache`).
    fn scan_unqualified_decorator_safe(&self, func_name: &str) -> bool {
        self.modules
            .iter()
            .filter_map(|m| self.by_module.get(m))
            .any(|fs| lookup_decorator_in_safety_map(func_name, fs))
    }

    /// Whether a constructor-shaped call is verified safe for `caller_module`,
    /// using the class's `__new__`/`__init__` verdicts rather than its aggregate
    /// verdict. Walks past package parents to the concrete module entry. An
    /// `UnsafeIfImported` constructor only clears within its own module.
    fn is_constructor_call_verified_safe_for_caller(
        &self,
        caller_module: &ModuleName,
        func_name: &str,
    ) -> bool {
        let fqn = ModuleName::from_str(func_name);
        for (parent, dot_pos) in fqn.iter_parents() {
            if self.modules.contains(&parent) {
                let local_name = &func_name[dot_pos + 1..];
                if let Some(verdict) = self
                    .by_module
                    .get(&parent)
                    .and_then(|fs| constructor_verdict(local_name, fs))
                {
                    return verdict == FunctionSafety::Safe
                        || (verdict == FunctionSafety::UnsafeIfImported
                            && caller_module == &parent);
                }
            }
        }
        false
    }

    /// A class-decorator call verifies safe exactly when the decorated class's
    /// constructor does.
    fn is_class_decorator_call_verified_safe_for_caller(
        &self,
        caller_module: &ModuleName,
        func_name: &str,
    ) -> bool {
        self.is_constructor_call_verified_safe_for_caller(caller_module, func_name)
    }

    /// Dispatch a cached error to the right verified-safe check by kind. The
    /// callee `metadata` may render with trailing `()` suffixes; strip them here.
    ///
    /// `UnknownFunctionCall` / `UnknownMethodCall` couldn't bind the call target,
    /// so they additionally skip the unqualified fallback: an unbound short name
    /// must not clear on a same-named safe function elsewhere.
    fn is_error_verified_safe(&self, error: &CachedError) -> bool {
        let func_name = error.metadata.trim_end_matches("()");
        match error.kind {
            ErrorKind::UnsafeDecoratorCall | ErrorKind::UnknownDecoratorCall
                if error.parameterized_decorator =>
            {
                self.is_decorator_call_verified_safe(func_name)
            }
            ErrorKind::UnknownFunctionCall | ErrorKind::UnknownMethodCall => {
                self.is_call_verified_safe_no_unqualified(func_name)
            }
            _ => self.is_call_verified_safe(func_name),
        }
    }

    /// Whether `error` in `caller` may be dropped. Constructor-shaped
    /// `UnsafeFunctionCall` and class-decorator calls clear on their per-caller
    /// static verdict, so they do not consult `kinds`; every other kind clears
    /// only when `kinds` admits it and the general verdict verifies it.
    fn clears_error(
        &self,
        caller: ModuleName,
        error: &CachedError,
        kinds: impl Fn(ErrorKind) -> bool,
    ) -> bool {
        let func_name = error.metadata.trim_end_matches("()");
        match error.kind {
            ErrorKind::UnsafeFunctionCall
                if self.is_constructor_call_verified_safe_for_caller(&caller, func_name) =>
            {
                true
            }
            ErrorKind::UnsafeDecoratorCall
                if self.is_class_decorator_call_verified_safe_for_caller(&caller, func_name) =>
            {
                true
            }
            kind => kinds(kind) && self.is_error_verified_safe(error),
        }
    }
}

/// Whether a plain function call can be verified as safe using cached
/// per-function safety verdicts from the resolved modules.
#[doc(hidden)]
pub fn is_call_verified_safe(
    func_name: &str,
    resolved_modules: &AHashSet<ModuleName>,
    func_safety_by_module: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> bool {
    SafetyResolver::new(resolved_modules, func_safety_by_module).is_call_verified_safe(func_name)
}

/// Whether a decorator is safe: safe itself AND every immediate (one level deep)
/// nested function is safe. For `deco`, `deco.builder` is checked; `deco.b.inner`
/// and `deco_helper` are not.
fn lookup_decorator_in_safety_map(
    local_name: &str,
    fs: &AHashMap<String, FunctionSafetyInfo>,
) -> bool {
    if !lookup_in_safety_map(local_name, fs) {
        return false;
    }
    // A class decorator returns the class, so its constructor methods (not
    // arbitrary nested defs) govern import-time safety; the aggregate-safe
    // factory verdict already reflects them.
    if is_class_like_entry(local_name, fs) {
        return true;
    }
    fs.iter().all(|(name, info)| {
        let is_immediate_child = name
            .strip_prefix(local_name)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|child| !child.contains('.'));
        !is_immediate_child || info.verdict == FunctionSafety::Safe
    })
}

/// The `function_safety` entry names of `local_name`'s constructor methods.
fn constructors(local_name: &str) -> impl Iterator<Item = String> + '_ {
    ["__new__", "__init__"]
        .into_iter()
        .map(move |method| format!("{local_name}.{method}"))
}

/// Whether `local_name` names a class: it has a cached `__init__`/`__new__`.
fn is_class_like_entry(local_name: &str, fs: &AHashMap<String, FunctionSafetyInfo>) -> bool {
    constructors(local_name).any(|method| fs.contains_key(&method))
}

/// The combined `__new__`/`__init__` verdict of a class, or `None` if neither is
/// cached (i.e. `local_name` is not a resolvable constructor).
fn constructor_verdict(
    local_name: &str,
    fs: &AHashMap<String, FunctionSafetyInfo>,
) -> Option<FunctionSafety> {
    constructors(local_name)
        .filter_map(|method| fs.get(&method).map(|info| info.verdict))
        .reduce(|acc, verdict| acc | verdict)
}

/// Merge `incoming` into `fs[attr]`, inserting a clone if absent. Returns whether
/// the entry changed (so callers can decide whether to reprocess dependents).
/// Borrows `incoming` so the caller need not clone it before a merge that only
/// updates an existing entry (the common re-processing case).
fn merge_function_safety_entry_ref(
    fs: &mut AHashMap<String, FunctionSafetyInfo>,
    attr: &str,
    incoming: &FunctionSafetyInfo,
) -> bool {
    match fs.get_mut(attr) {
        Some(existing) => existing.merge_ref(incoming),
        None => {
            fs.insert(attr.to_owned(), incoming.clone());
            true
        }
    }
}

#[doc(hidden)]
/// Keep cached implicit import guards exact. Unlike missing import graph edges,
/// these output values name the submodule access that must be loaded eagerly.
pub fn dedupe_implicit_imports(implicit_imports: &mut Vec<ModuleName>) {
    let mut seen = AHashSet::with_capacity(implicit_imports.len());
    implicit_imports.retain(|imp| seen.insert(*imp));
}

impl CachedModule {
    fn empty(name: ModuleName) -> Self {
        CachedModule {
            name,
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: AHashSet::new(),
            missing_imports: AHashSet::new(),
            ambiguous_imports: AHashSet::new(),
            side_effect_imports: AHashSet::new(),
            function_safety: AHashMap::new(),
            mutation_candidates: Vec::new(),
        }
    }

    pub fn is_safe(&self) -> bool {
        matches!(&self.safety, CachedSafety::Ok(s) if s.is_safe())
    }

    /// Merge another CachedModule (same name) into this one.
    fn merge(&mut self, other: CachedModule) {
        self.imports.extend(other.imports);
        self.missing_imports
            .retain(|m| other.missing_imports.contains(m));
        self.ambiguous_imports.extend(other.ambiguous_imports);
        self.side_effect_imports.extend(other.side_effect_imports);
        self.safety.merge(other.safety);
        for (name, info) in other.function_safety {
            match self.function_safety.entry(name) {
                Entry::Occupied(mut entry) => {
                    entry.get_mut().merge(info);
                }
                Entry::Vacant(entry) => {
                    entry.insert(info);
                }
            }
        }
        let mut seen: AHashSet<&MutationCandidate> = self.mutation_candidates.iter().collect();
        let keep: Vec<bool> = other
            .mutation_candidates
            .iter()
            .map(|candidate| seen.insert(candidate))
            .collect();
        // Release the borrowed candidates before moving the retained values.
        drop(seen);
        self.mutation_candidates.extend(
            other
                .mutation_candidates
                .into_iter()
                .zip(keep)
                .filter_map(|(candidate, keep)| keep.then_some(candidate)),
        );
    }
}

impl CachedSafety {
    /// Merge another safety result, keeping the more conservative outcome.
    /// AnalysisError always wins. Between two Ok results, keep the union of errors.
    fn merge(&mut self, other: CachedSafety) {
        match (&mut *self, other) {
            // AnalysisError is the most conservative — keep it
            (CachedSafety::AnalysisError { .. }, _) => {}
            (_, other @ CachedSafety::AnalysisError { .. }) => *self = other,
            // Both Ok: merge errors and overrides
            (CachedSafety::Ok(this), CachedSafety::Ok(other)) => {
                merge_errors(&mut this.errors, other.errors);
                merge_errors(
                    &mut this.force_imports_eager_overrides,
                    other.force_imports_eager_overrides,
                );

                this.implicit_imports.extend(other.implicit_imports);
                this.implicit_imports.sort();
                this.implicit_imports.dedup();
            }
        }
    }

    /// Convert back to a SafetyResult for pipeline reconstruction.
    pub fn to_safety_result(&self) -> SafetyResult {
        match self {
            CachedSafety::Ok(safety) => {
                let mut module_safety = ModuleSafety::new();
                for error in &safety.errors {
                    module_safety.add_error(error.to_safety_error());
                }
                for override_err in &safety.force_imports_eager_overrides {
                    module_safety.add_force_import_override(override_err.to_safety_error());
                }
                module_safety.implicit_imports = safety.implicit_imports.clone();
                SafetyResult::Ok(module_safety)
            }
            CachedSafety::AnalysisError { message } => {
                SafetyResult::AnalysisError(anyhow::anyhow!("{}", message))
            }
        }
    }

    fn from_safety_result(result: &SafetyResult) -> Self {
        match result {
            SafetyResult::Ok(safety) => CachedSafety::Ok(CachedModuleSafety {
                errors: safety
                    .errors
                    .iter()
                    .map(CachedError::from_safety_error)
                    .collect(),
                force_imports_eager_overrides: safety
                    .force_imports_eager_overrides
                    .iter()
                    .map(CachedError::from_safety_error)
                    .collect(),
                implicit_imports: {
                    let mut v = safety.implicit_imports.clone();
                    v.sort();
                    v
                },
            }),
            SafetyResult::AnalysisError(e) => CachedSafety::AnalysisError {
                message: e.to_string(),
            },
        }
    }
}

impl CachedModuleSafety {
    pub fn is_safe(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn should_load_imports_eagerly(&self) -> bool {
        !self.force_imports_eager_overrides.is_empty()
    }
}

fn merge_errors(target: &mut Vec<CachedError>, other: Vec<CachedError>) {
    target.extend(other);
    target.sort();
    target.dedup();
}

impl CachedError {
    fn from_safety_error(error: &SafetyError) -> Self {
        CachedError {
            kind: error.kind,
            metadata: error.metadata.as_str().to_string(),
            parameterized_decorator: error.parameterized_decorator,
        }
    }

    fn to_safety_error(&self) -> SafetyError {
        let mut error = SafetyError::new(self.kind, self.metadata.clone(), TextRange::default());
        error.parameterized_decorator = self.parameterized_decorator;
        error
    }
}

impl CachedExports {
    /// Build the cached re-exports for a library, keeping only those exported by
    /// one of the library's own modules. `get_re_exports()` also yields the bundled
    /// stubs' re-exports, identical across every cache; dropping them is safe because
    /// each re-export is owned by exactly one module's cache and the reduce rebuilds
    /// stub chains from the bundled stub graph.
    fn from_exports(exports: &Exports, own_modules: &AHashSet<ModuleName>) -> Self {
        let re_exports: Vec<CachedReExport> = exports
            .get_re_exports()
            .filter(|(exported, _)| own_modules.contains(&exported.module))
            .map(|(exported, (imported, _range))| CachedReExport {
                exported_module: exported.module,
                exported_attr: exported.attr.to_string(),
                imported_module: imported.module,
                imported_attr: imported.attr.to_string(),
            })
            .collect();

        let mut result = CachedExports { re_exports };
        result.sort_and_dedup();
        result
    }

    fn sort_and_dedup(&mut self) {
        self.re_exports.par_sort_by(|a, b| {
            (&a.exported_module, &a.exported_attr).cmp(&(&b.exported_module, &b.exported_attr))
        });
        self.re_exports.dedup_by(|a, b| {
            a.exported_module == b.exported_module && a.exported_attr == b.exported_attr
        });
    }
}

#[cfg(test)]
mod tests {
    use rayon::ThreadPoolBuilder;

    use super::*;

    #[test]
    fn mro_resolves_inherited_method_to_base_verdict() {
        let modules: AHashSet<ModuleName> =
            [ModuleName::from_str("base"), ModuleName::from_str("sub")]
                .into_iter()
                .collect();
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::new();
        by_module.insert(
            ModuleName::from_str("base"),
            [(
                "Base.method".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
        );
        // `sub.Sub` inherits `method` from `base.Base`; it has no own entry.
        by_module.insert(ModuleName::from_str("sub"), AHashMap::new());
        let class_bases: HashMap<ModuleName, Vec<ModuleName>> = [(
            ModuleName::from_str("sub.Sub"),
            vec![ModuleName::from_str("base.Base")],
        )]
        .into_iter()
        .collect();

        let resolver = SafetyResolver::new(&modules, &by_module).with_class_bases(&class_bases);
        assert!(
            resolver.is_call_verified_safe("sub.Sub.method"),
            "an inherited method resolves to the defining base's Safe verdict via the MRO",
        );

        let no_mro = SafetyResolver::new(&modules, &by_module);
        assert!(
            !no_mro.is_call_verified_safe("sub.Sub.method"),
            "without MRO data an inherited method is not verified (no class fallback)",
        );
    }

    #[test]
    fn mro_own_unsafe_override_shadows_safe_base() {
        let modules: AHashSet<ModuleName> =
            [ModuleName::from_str("base"), ModuleName::from_str("sub")]
                .into_iter()
                .collect();
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::new();
        by_module.insert(
            ModuleName::from_str("base"),
            [(
                "Base.method".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
        );
        // `sub.Sub` overrides the inherited `method` with an `Unsafe` one.
        by_module.insert(
            ModuleName::from_str("sub"),
            [(
                "Sub.method".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Unsafe),
            )]
            .into_iter()
            .collect(),
        );
        let class_bases: HashMap<ModuleName, Vec<ModuleName>> = [(
            ModuleName::from_str("sub.Sub"),
            vec![ModuleName::from_str("base.Base")],
        )]
        .into_iter()
        .collect();

        let resolver = SafetyResolver::new(&modules, &by_module).with_class_bases(&class_bases);
        assert!(
            !resolver.is_call_verified_safe("sub.Sub.method"),
            "an own Unsafe override shadows the base's Safe verdict; the MRO must not be walked",
        );
    }

    #[test]
    fn mro_diamond_prefers_right_branch_override_over_shared_ancestor() {
        let module = ModuleName::from_str("m");
        let modules: AHashSet<ModuleName> = [module].into_iter().collect();
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::new();
        by_module.insert(
            module,
            [
                (
                    "A.method".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                ),
                (
                    "C.method".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Unsafe),
                ),
            ]
            .into_iter()
            .collect(),
        );
        // D(B, C); B(A); C(A). `method` is Safe on the shared ancestor A and
        // overridden Unsafe on the right branch C. C3 MRO of D is [D, B, C, A],
        // so D.method resolves to C (Unsafe); a depth-first walk would wrongly
        // reach A (Safe) first and clear the call.
        let class_bases: HashMap<ModuleName, Vec<ModuleName>> = [
            (
                ModuleName::from_str("m.D"),
                vec![ModuleName::from_str("m.B"), ModuleName::from_str("m.C")],
            ),
            (
                ModuleName::from_str("m.B"),
                vec![ModuleName::from_str("m.A")],
            ),
            (
                ModuleName::from_str("m.C"),
                vec![ModuleName::from_str("m.A")],
            ),
        ]
        .into_iter()
        .collect();

        let resolver = SafetyResolver::new(&modules, &by_module).with_class_bases(&class_bases);
        assert!(
            !resolver.is_call_verified_safe("m.D.method"),
            "diamond method resolves via C3 to the Unsafe right-branch override, not the Safe ancestor",
        );
    }

    #[test]
    fn merge_class_bases_unions_duplicate_class_fqns() {
        let c = ModuleName::from_str("pkg.mod.C");
        let base_a = ModuleName::from_str("pkg.mod.A");
        let base_b = ModuleName::from_str("pkg.mod.B");

        // Two libraries contribute `pkg.mod.C`: a duplicate base and a new one.
        let merged = merge_class_bases(vec![(c, vec![base_a]), (c, vec![base_a, base_b])]);

        assert_eq!(
            merged.get(&c),
            Some(&vec![base_a, base_b]),
            "duplicate FQN base lists union preserving first-seen order, not last-wins",
        );
    }

    #[test]
    fn clear_verified_errors_processes_every_module() {
        // Callees are module-qualified so the conservative `Unknown*` path can
        // bind them per module; an unqualified short name is never cleared.
        let module_a = ModuleName::from_str("test.module_a");
        let module_b = ModuleName::from_str("test.module_b");

        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: module_a,
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "test.module_a.helper()".to_owned(),
                            parameterized_decorator: false,
                        }],
                        force_imports_eager_overrides: Vec::new(),
                        implicit_imports: Vec::new(),
                    }),
                    imports: AHashSet::new(),
                    missing_imports: AHashSet::new(),
                    ambiguous_imports: AHashSet::new(),
                    side_effect_imports: AHashSet::new(),
                    function_safety: AHashMap::new(),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: module_b,
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "test.module_b.helper()".to_owned(),
                            parameterized_decorator: false,
                        }],
                        force_imports_eager_overrides: Vec::new(),
                        implicit_imports: Vec::new(),
                    }),
                    imports: AHashSet::new(),
                    missing_imports: AHashSet::new(),
                    ambiguous_imports: AHashSet::new(),
                    side_effect_imports: AHashSet::new(),
                    function_safety: AHashMap::new(),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: CachedExports {
                re_exports: Vec::new(),
            },
            class_bases: Vec::new(),
        };

        let module_names: AHashSet<ModuleName> = [module_a, module_b].into_iter().collect();
        let func_safety_by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> = [
            (
                module_a,
                [(
                    "helper".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                )]
                .into_iter()
                .collect(),
            ),
            (
                module_b,
                [(
                    "helper".to_owned(),
                    FunctionSafetyInfo::new(FunctionSafety::Safe),
                )]
                .into_iter()
                .collect(),
            ),
        ]
        .into_iter()
        .collect();
        let globally_safe_funcs: AHashSet<String> = ["helper".to_owned()].into_iter().collect();

        let resolver = SafetyResolver::with_safe_index(
            &module_names,
            &func_safety_by_module,
            &globally_safe_funcs,
        );
        let cleared = ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("should build test thread pool")
            .install(|| {
                cache.clear_errors_where(|caller, error| {
                    resolver.clears_error(caller, error, |_| true)
                })
            });

        assert!(
            cleared,
            "expected at least one verified error to be removed"
        );
        assert!(
            cache.modules.iter().all(CachedModule::is_safe),
            "all modules should have their verified errors cleared",
        );
    }
}
