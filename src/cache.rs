/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

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
use crate::imports::ImportGraph;
use crate::imports::resolve_to_known_module;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::ModuleSafety;
use crate::module_safety::MutationCandidate;
use crate::module_safety::MutationCandidateSite;
use crate::module_safety::SafetyResult;
use crate::mro::c3_linearize;
use crate::project::SafetyMap;
use crate::project::SideEffectMap;
use crate::pyrefly::sys_info::PythonVersion;
use crate::source_map::SourceMap;
use crate::source_map::Sources;
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
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
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

    /// Write the cache to a binary file using postcard. Modules are serialized
    /// into independent per-module blobs so the read side can decode them in
    /// parallel (the per-module structs dominate (de)serialization cost).
    pub fn write_to_file(&self, path: &Path) -> anyhow::Result<()> {
        let module_blobs: Vec<Vec<u8>> = self
            .modules
            .par_iter()
            .map(postcard::to_allocvec)
            .collect::<Result<_, _>>()?;
        let bytes = postcard::to_allocvec(&(&self.exports, &self.class_bases, &module_blobs))?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Read a cache from a binary file using postcard. The outer decode just
    /// splits the file into per-module byte blobs; the expensive per-module
    /// struct deserialization then runs in parallel.
    pub fn read_from_file(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path)?;
        let (exports, class_bases, module_blobs): (
            CachedExports,
            Vec<(ModuleName, Vec<ModuleName>)>,
            Vec<Vec<u8>>,
        ) = postcard::from_bytes(&bytes)?;
        let modules = module_blobs
            .par_iter()
            .map(|blob| postcard::from_bytes(blob))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LibraryCache {
            modules,
            exports,
            class_bases,
        })
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
                modules.sort_by_key(|m| m.name);
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
    ) -> HashMap<ModuleName, AHashSet<ModuleName>> {
        let mut resolved: HashMap<ModuleName, AHashSet<ModuleName>> = HashMap::new();
        for module in &mut self.modules {
            for ambiguous in module.ambiguous_imports.drain() {
                if let Some(target) = resolve_to_known_module(&ambiguous, module_names) {
                    module.imports.insert(target);
                    resolved.entry(module.name).or_default().insert(target);
                }
            }
        }
        resolved
    }

    /// Iteratively clear false errors: promoting the functions of a module
    /// whose missing imports are all resolved to `Safe` can make a caller
    /// error-free, which in turn promotes its functions. Repeat until a round
    /// promotes nothing or clears nothing.
    fn upgrade_missing_dep_functions(
        &mut self,
        module_names: &AHashSet<ModuleName>,
        func_safety_by_module: &mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
        clear_errors: bool,
        class_bases: &HashMap<ModuleName, Vec<ModuleName>>,
    ) {
        let (promoted, globally_safe_funcs) = promote_fixpoint(module_names, func_safety_by_module);
        let decorator_scan_cache: DashMap<String, bool, FixedState> = DashMap::default();
        if !promoted.is_empty() || clear_errors {
            // With positive evidence (a promotion or a mutation candidate now
            // `Safe`), clear every verified-safe error kind.
            let resolver = SafetyResolver::with_safe_index(
                module_names,
                func_safety_by_module,
                &globally_safe_funcs,
            )
            .with_decorator_cache(&decorator_scan_cache)
            .with_class_bases(class_bases);
            self.clear_errors_where(&resolver, |_| true);
        } else {
            // Without promotion evidence, clear only `UnsafeDecoratorCall`: its
            // safety follows from static verdicts alone. Decorator checks ignore
            // the globally-safe index, so an empty one suffices.
            let empty = AHashSet::new();
            let resolver =
                SafetyResolver::with_safe_index(module_names, func_safety_by_module, &empty)
                    .with_decorator_cache(&decorator_scan_cache)
                    .with_class_bases(class_bases);
            self.clear_errors_where(&resolver, |kind| kind == ErrorKind::UnsafeDecoratorCall);
        }
        debug!("{} functions promoted", promoted.len());
    }

    /// Drop every error whose kind `should_clear` selects and that `resolver`
    /// verifies as safe, in parallel. Returns whether any error was removed.
    fn clear_errors_where(
        &mut self,
        resolver: &SafetyResolver,
        should_clear: impl Fn(ErrorKind) -> bool + Sync,
    ) -> bool {
        self.modules
            .par_iter_mut()
            .map(|module| {
                let CachedSafety::Ok(ref mut safety) = module.safety else {
                    return false;
                };
                retain_unverified_errors(safety, |error| {
                    should_clear(error.kind) && resolver.is_error_verified_safe(error)
                })
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
                if let Some(resolved) = resolve_to_known_module(&missing, &module_names) {
                    module.imports.insert(resolved);
                    resolved_modules.insert(resolved);
                } else {
                    still_missing.insert(missing);
                }
            }

            module.missing_imports = still_missing;

            if let CachedSafety::Ok(ref mut safety) = module.safety {
                let resolver = SafetyResolver::new(&resolved_modules, &func_safety_by_module)
                    .with_class_bases(&class_bases);
                retain_unverified_errors(safety, |error| resolver.is_error_verified_safe(error));
            }
        });

        let resolved = self.resolve_mutation_candidates(&module_names, &mut func_safety_by_module);

        self.upgrade_missing_dep_functions(
            &module_names,
            &mut func_safety_by_module,
            resolved,
            &class_bases,
        );

        for module in &mut self.modules {
            if let Some(fs) = func_safety_by_module.remove(&module.name) {
                module.function_safety = fs;
            }
        }
    }

    /// Resolve the cross-library mutation candidates cached by the map step against
    /// the now-merged function verdicts. Returns whether any function was resolved
    /// to `Safe`, so the caller can run a verified-error clear even when the
    /// promotion fixpoint promotes nothing.
    fn resolve_mutation_candidates(
        &mut self,
        module_names: &AHashSet<ModuleName>,
        func_safety_by_module: &mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    ) -> bool {
        let mut module_errors: HashMap<ModuleName, Vec<String>> = HashMap::new();
        let resolved_to_safe = apply_mutation_candidates(
            self.modules
                .iter()
                .map(|m| (m.name, m.mutation_candidates.as_slice())),
            module_names,
            func_safety_by_module,
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
        resolved_to_safe
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

            // Pull the source symbol's current safety; skip until it resolves.
            let Some(safety) = lookup_function_safety(
                &self.modules,
                &module_index,
                re.imported_module,
                &re.imported_attr,
            ) else {
                continue;
            };
            let Some(&dst_idx) = module_index.get(&re.exported_module) else {
                continue;
            };

            // The re-exported symbol inherits the union of its source's concerns.
            let dest_changed = merge_function_safety_entry(
                &mut self.modules[dst_idx].function_safety,
                &re.exported_attr,
                safety,
            );

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

    /// Reconstruct a SafetyMap from cached module data.
    pub fn to_safety_map(&self) -> SafetyMap {
        let map = SafetyMap::with_capacity(self.modules.len());
        for module in &self.modules {
            map.insert(module.name, module.safety.to_safety_result());
        }
        map
    }

    /// Inject the bundled stdlib stubs as graph-only nodes so the merged graph
    /// matches the e2e graph: per-library caches drop stub-only modules, losing
    /// the typeshed import cycle. Skips names a real library already provides.
    /// Returns the injected names so the caller can keep them out of the safety map.
    pub fn inject_bundled_stub_graph(
        &mut self,
        python_version: PythonVersion,
    ) -> AHashSet<ModuleName> {
        let sources =
            Sources::new_with_version(SourceMap::default(), PathBuf::new(), python_version);
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

    /// Reconstruct a SideEffectMap from cached module data.
    pub fn to_side_effect_map(&self) -> SideEffectMap {
        let mut map = SideEffectMap::with_capacity(self.modules.len());
        for m in &self.modules {
            if !m.side_effect_imports.is_empty() {
                map.insert(m.name, m.side_effect_imports.iter().copied().collect());
            }
        }
        map
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

    /// Whether `module` has an own entry for `local` verified `Safe`.
    fn own_call_safe(&self, module: &ModuleName, local: &str) -> bool {
        self.by_module
            .get(module)
            .is_some_and(|fs| lookup_in_safety_map(local, fs))
    }

    /// Whether a plain function call is found and verified `Safe`.
    fn is_call_verified_safe(&self, func_name: &str) -> bool {
        match self.split_at_module(func_name) {
            Some((module, local)) => {
                self.own_call_safe(&module, local)
                    || self
                        .mro_method_verdict(&module, local)
                        .is_some_and(|v| v.is_safe())
            }
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

    /// Dispatch a cached error to the right verified-safe check by kind. The
    /// callee `metadata` may render with a trailing `()`; strip it once here.
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
    fs.iter().all(|(name, info)| {
        let is_immediate_child = name
            .strip_prefix(local_name)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|child| !child.contains('.'));
        !is_immediate_child || info.verdict == FunctionSafety::Safe
    })
}

/// Look up the cached safety info of a mutation candidate's callee, resolving its FQN
/// against the merged module set the same way `is_call_verified_safe` does.
fn lookup_callee_info<'a>(
    callee: &ModuleName,
    module_names: &AHashSet<ModuleName>,
    func_safety_by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> Option<&'a FunctionSafetyInfo> {
    for (parent, dot_pos) in callee.iter_parents() {
        if module_names.contains(&parent) {
            let local_name = &callee.as_str()[dot_pos + 1..];
            return get_function_safety(func_safety_by_module, &parent, local_name);
        }
    }
    None
}

/// Get a function's safety info from the nested module -> name map.
pub(crate) fn get_function_safety<'a>(
    map: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    module: &ModuleName,
    name: &str,
) -> Option<&'a FunctionSafetyInfo> {
    map.get(module)?.get(name)
}

/// Mutable version for updating verdicts in place.
pub(crate) fn get_function_safety_mut<'a>(
    map: &'a mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    module: &ModuleName,
    name: &str,
) -> Option<&'a mut FunctionSafetyInfo> {
    map.get_mut(module)?.get_mut(name)
}

/// The current safety of `module.attr` in the merged modules, if both the module
/// and the attribute are present. Cloned so the caller can merge it into another
/// module without holding a borrow on the source.
fn lookup_function_safety(
    modules: &[CachedModule],
    module_index: &AHashMap<ModuleName, usize>,
    module: ModuleName,
    attr: &str,
) -> Option<FunctionSafetyInfo> {
    let idx = *module_index.get(&module)?;
    modules[idx].function_safety.get(attr).cloned()
}

/// Merge `incoming` into `fs[attr]`, inserting it if absent. Returns whether the
/// entry changed (so callers can decide whether to reprocess dependents).
fn merge_function_safety_entry(
    fs: &mut AHashMap<String, FunctionSafetyInfo>,
    attr: &str,
    incoming: FunctionSafetyInfo,
) -> bool {
    match fs.get_mut(attr) {
        Some(existing) => existing.merge(incoming),
        None => {
            fs.insert(attr.to_owned(), incoming);
            true
        }
    }
}

/// Resolve mutation candidates against per-function safety verdicts.
///
/// For each `(module, mutation_candidates)` pair: a confirmed mutation candidate
/// (its callee mutates a parameter fed an imported argument) either records a
/// module-scope `ImportedVarArgument` error (via `module_scope_error`) or makes
/// the in-function caller hard `Unsafe`; an unconfirmed one drops the callee
/// from the caller's missing-dep set, promoting a caller with no remaining
/// missing dep back to `Safe`. A callee that resolved to a non-`Safe` verdict is
/// left in the missing-dep set, so the promotion fixpoint's verified-safe check
/// keeps the caller unsafe instead of prematurely promoting it to `Safe`. Returns
/// whether any function was resolved to `Safe`.
///
/// Shared by the cache reduce and the single-pass (whole-program) resolution:
/// the former feeds cache modules and collects errors onto `CachedModuleSafety`,
/// the latter feeds its in-memory state.
pub(crate) fn apply_mutation_candidates<'a>(
    modules: impl Iterator<Item = (ModuleName, &'a [MutationCandidate])>,
    module_names: &AHashSet<ModuleName>,
    func_safety_by_module: &mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    mut module_scope_error: impl FnMut(ModuleName, String),
) -> bool {
    let pairs: Vec<(ModuleName, &MutationCandidate)> = modules
        .flat_map(|(module, candidates)| candidates.iter().map(move |c| (module, c)))
        .collect();
    // `candidate_mutates` reads only `mutated_params` (never written below), so
    // confirming every candidate in parallel matches the serial pass.
    let confirmed: Vec<bool> = pairs
        .par_iter()
        .map(|(_, candidate)| candidate_mutates(candidate, module_names, func_safety_by_module))
        .collect();

    // Apply serially in original order: the verdict writes and the order-dependent
    // `callee_resolves_unsafe` read must not be reordered.
    let mut resolved_to_safe = false;
    for (&(module_name, candidate), &confirmed) in pairs.iter().zip(&confirmed) {
        match (&candidate.site, confirmed) {
            (MutationCandidateSite::ModuleScope { call }, true) => {
                module_scope_error(module_name, call.as_str().to_owned());
            }
            (MutationCandidateSite::ModuleScope { .. }, false) => {}
            (MutationCandidateSite::Function { name }, true) => {
                if let Some(info) =
                    get_function_safety_mut(func_safety_by_module, &module_name, name.as_str())
                {
                    info.verdict.insert(FunctionSafety::Unsafe);
                    // The callee is now resolved (it mutates the imported arg), so discharge
                    // its missing-dep concern; the `Unsafe` bit it just set stands.
                    info.missing_dep_callees.remove(&candidate.callee);
                    if info.missing_dep_callees.is_empty() {
                        info.verdict.remove(FunctionSafety::UnsafeMissingDep);
                    }
                }
            }
            (MutationCandidateSite::Function { name }, false) => {
                // A callee that resolved to a non-`Safe` verdict must keep its caller
                // unsafe even though it doesn't mutate the imported arg. Leaving it in
                // `missing_dep_callees` defers to the promotion fixpoint's verified-safe
                // check; only unresolved callees (treated as safe, like the single-pass
                // analyzer) or verified-safe callees resolve here.
                if callee_resolves_unsafe(&candidate.callee, module_names, func_safety_by_module) {
                    continue;
                }
                if let Some(info) =
                    get_function_safety_mut(func_safety_by_module, &module_name, name.as_str())
                {
                    info.missing_dep_callees.remove(&candidate.callee);
                    if info.verdict.has(FunctionSafety::UnsafeMissingDep)
                        && info.missing_dep_callees.is_empty()
                    {
                        info.verdict.remove(FunctionSafety::UnsafeMissingDep);
                        // Only clearing all the way to `Safe` verifies callers' errors; any
                        // remaining concern (e.g. `UnsafeIfImported`) leaves cross-module
                        // calls unsafe.
                        if info.verdict.is_safe() {
                            resolved_to_safe = true;
                        }
                    }
                }
            }
        }
    }
    resolved_to_safe
}

/// Whether a cached mutation candidate is confirmed: its callee resolves in the
/// merged set and mutates a parameter that the call feeds an imported argument.
///
/// A cross-library constructor call records the class FQN as the callee (the
/// dependency's class table is unavailable at map time), but its parameter
/// mutations live on the constructor methods, which take an implicit receiver
/// absent from the class-level call — so those are probed at the next arg offset.
fn candidate_mutates(
    candidate: &MutationCandidate,
    module_names: &AHashSet<ModuleName>,
    func_safety_by_module: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> bool {
    let callee_mutates = |callee: &ModuleName, arg_offset: usize| {
        lookup_callee_info(callee, module_names, func_safety_by_module).is_some_and(|info| {
            candidate.imported_args.hits_any_param(
                info.mutated_params
                    .iter()
                    .map(|param| (param.name.as_str(), param.position)),
                arg_offset,
            )
        })
    };
    if callee_mutates(&candidate.callee, candidate.arg_offset) {
        return true;
    }
    ["__init__", "__new__"].into_iter().any(|method| {
        let ctor = candidate.callee.append_str(method);
        callee_mutates(&ctor, candidate.arg_offset + 1)
    })
}

/// Whether `callee` resolves in the merged set to a verdict other than `Safe`.
/// Such a callee keeps its caller unsafe, so its missing-dep entry must not be
/// resolved just because it does not mutate the imported argument. An
/// unresolved callee returns `false` (treated as safe, like the single-pass
/// analyzer's handling of an unresolved call).
fn callee_resolves_unsafe(
    callee: &ModuleName,
    module_names: &AHashSet<ModuleName>,
    func_safety_by_module: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> bool {
    lookup_callee_info(callee, module_names, func_safety_by_module)
        .is_some_and(|info| !info.verdict.is_safe())
}

/// Union two name sets, extending the larger with the smaller to minimize rehashing.
fn union_larger(a: AHashSet<String>, b: AHashSet<String>) -> AHashSet<String> {
    let (mut large, small) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    large.extend(small);
    large
}

/// A promotion candidate's callee, FQN pre-resolved so the fixpoint never re-splits it.
enum ResolvedCallee {
    Qualified { module: ModuleName, local: String },
    Unqualified { name: String },
}

/// A promotion candidate with its callees pre-resolved and base verdict precomputed.
struct PromotionCandidate {
    module: ModuleName,
    name: String,
    /// `verdict.without(UnsafeMissingDep)` — stable across rounds.
    base_verdict: FunctionSafety,
    callees: Vec<ResolvedCallee>,
}

/// Split a callee FQN at its longest module prefix, like `SafetyResolver::split_at_module`.
fn resolve_callee(func_name: &str, module_names: &AHashSet<ModuleName>) -> ResolvedCallee {
    match ModuleName::from_str(func_name)
        .iter_parents()
        .find(|(parent, _)| module_names.contains(parent))
    {
        Some((module, dot_pos)) => ResolvedCallee::Qualified {
            module,
            local: func_name[dot_pos + 1..].to_owned(),
        },
        None => ResolvedCallee::Unqualified {
            name: func_name.to_owned(),
        },
    }
}

/// The verdict a callee resolves to (`Safe`/`UnsafeIfImported`), or `None` if
/// unresolved: qualified reads the exact module safety-map entry, unqualified the
/// global name indices (safe wins over if-imported).
fn resolve_callee_verdict(
    callee: &ResolvedCallee,
    func_safety_by_module: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    globally_safe: &AHashSet<String>,
    globally_if_imported: &AHashSet<String>,
) -> Option<FunctionSafety> {
    match callee {
        ResolvedCallee::Qualified { module, local } => func_safety_by_module
            .get(module)
            .and_then(|fs| lookup_verdict_in_safety_map(local, fs)),
        ResolvedCallee::Unqualified { name } => {
            if globally_safe.contains(name.as_str()) {
                Some(FunctionSafety::Safe)
            } else if globally_if_imported.contains(name.as_str()) {
                Some(FunctionSafety::UnsafeIfImported)
            } else {
                None
            }
        }
    }
}

/// Promote every `UnsafeMissingDep` function whose missing-dep callees now all resolve to `Safe`,
/// iterating to a fixpoint (one promotion can unblock a caller the next round).
///
/// Returns the promoted functions as `(module, local-name)` pairs, as well as the
/// globally-safe-name index.
pub(crate) fn promote_fixpoint(
    module_names: &AHashSet<ModuleName>,
    func_safety_by_module: &mut AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
) -> (Vec<(ModuleName, String)>, AHashSet<String>) {
    // Globally-safe / -`UnsafeIfImported` function-name indices, for the
    // unqualified-callee fallback in `resolve_callee_verdict`. Built in parallel
    // (collect the per-module maps to a Vec first — `HashMap::par_iter` splits
    // poorly), cloning a name only on first sight.
    let module_fs: Vec<&AHashMap<String, FunctionSafetyInfo>> = func_safety_by_module
        .iter()
        .filter(|(module, _)| module_names.contains(module))
        .map(|(_, fs)| fs)
        .collect();
    let (mut globally_safe_funcs, mut globally_if_imported_funcs): (
        AHashSet<String>,
        AHashSet<String>,
    ) = module_fs
        .par_iter()
        .fold(
            || (AHashSet::new(), AHashSet::new()),
            |(mut safe, mut if_imported), fs| {
                for (name, info) in *fs {
                    match info.verdict {
                        FunctionSafety::Safe if !safe.contains(name.as_str()) => {
                            safe.insert(name.clone());
                        }
                        FunctionSafety::UnsafeIfImported
                            if !if_imported.contains(name.as_str()) =>
                        {
                            if_imported.insert(name.clone());
                        }
                        _ => {}
                    }
                }
                (safe, if_imported)
            },
        )
        .reduce(
            || (AHashSet::new(), AHashSet::new()),
            |(safe_a, if_a), (safe_b, if_b)| {
                (union_larger(safe_a, safe_b), union_larger(if_a, if_b))
            },
        );
    drop(module_fs);

    // Collect the promotion guard-passers once — the guard is stable across rounds
    // (promotion only clears the missing-dep bit) — resolving each callee FQN here
    // so the fixpoint never re-splits it.
    let candidates: Vec<PromotionCandidate> = func_safety_by_module
        .par_iter()
        .filter(|(module, _)| module_names.contains(module))
        .flat_map_iter(|(module, fs)| {
            fs.iter().filter_map(move |(name, info)| {
                if info.verdict.has(FunctionSafety::UnsafeMissingDep)
                    && !info.verdict.has(FunctionSafety::Unsafe)
                    && !info.missing_dep_callees.is_empty()
                {
                    Some(PromotionCandidate {
                        module: *module,
                        name: name.clone(),
                        base_verdict: info.verdict.without(FunctionSafety::UnsafeMissingDep),
                        callees: info
                            .missing_dep_callees
                            .iter()
                            .map(|callee| resolve_callee(callee.as_str(), module_names))
                            .collect(),
                    })
                } else {
                    None
                }
            })
        })
        .collect();
    let n = candidates.len();

    // Reverse index (source symbol -> dependent candidates) so a round re-checks
    // only the previous round's promotions' dependents. A qualified callee is
    // watched under its exact `(module, local)`, an unqualified one by bare name.
    let mut qualified_deps: AHashMap<ModuleName, AHashMap<String, Vec<u32>>> = AHashMap::new();
    let mut unqualified_deps: AHashMap<String, Vec<u32>> = AHashMap::new();
    fn watch(deps: &mut AHashMap<String, Vec<u32>>, key: &str, i: u32) {
        match deps.get_mut(key) {
            Some(watchers) => watchers.push(i),
            None => {
                deps.insert(key.to_owned(), vec![i]);
            }
        }
    }
    for (i, cand) in candidates.iter().enumerate() {
        for callee in &cand.callees {
            match callee {
                ResolvedCallee::Qualified { module, local } => {
                    watch(qualified_deps.entry(*module).or_default(), local, i as u32);
                }
                ResolvedCallee::Unqualified { name } => {
                    watch(&mut unqualified_deps, name, i as u32);
                }
            }
        }
    }

    // Round-synchronized worklist: each round's promotions are computed against the
    // frozen start-of-round state (in parallel) and applied together, so promotions
    // within a round never observe one another — the fixpoint result is identical to
    // a full per-round rescan. Later rounds shrink to the previous round's dependents.
    let mut all_promoted: Vec<(ModuleName, String)> = Vec::new();
    let mut promoted_flag = vec![false; n];
    let mut in_dirty = vec![true; n];
    let mut dirty: Vec<u32> = (0..n as u32).collect();
    while !dirty.is_empty() {
        let this_round = std::mem::take(&mut dirty);
        for &i in &this_round {
            in_dirty[i as usize] = false;
        }
        // Frozen phase: read only, so every candidate sees end-of-previous-round state.
        let fsbm: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            func_safety_by_module;
        let to_promote: Vec<(u32, FunctionSafety)> = this_round
            .par_iter()
            .filter_map(|&i| {
                if promoted_flag[i as usize] {
                    return None;
                }
                let cand = &candidates[i as usize];
                let mut target = cand.base_verdict;
                for callee in &cand.callees {
                    target.insert(resolve_callee_verdict(
                        callee,
                        fsbm,
                        &globally_safe_funcs,
                        &globally_if_imported_funcs,
                    )?);
                }
                Some((i, target))
            })
            .collect();
        if to_promote.is_empty() {
            break;
        }
        // Apply phase: commit verdicts and seed the indices for the next round.
        // Seed/record only when the verdict write lands, matching the whole-program
        // path and avoiding claiming a promotion for a function that isn't present.
        for &(i, target) in &to_promote {
            promoted_flag[i as usize] = true;
            let cand = &candidates[i as usize];
            if let Some(info) =
                get_function_safety_mut(func_safety_by_module, &cand.module, &cand.name)
            {
                info.verdict = target;
                if target.is_safe() {
                    globally_safe_funcs.insert(cand.name.clone());
                } else if target == FunctionSafety::UnsafeIfImported {
                    // Not globally safe; must not seed the safe index.
                    globally_if_imported_funcs.insert(cand.name.clone());
                }
                all_promoted.push((cand.module, cand.name.clone()));
            }
        }
        // Enqueue the dependents of everything promoted this round.
        let mut enqueue = |j: u32| {
            if !promoted_flag[j as usize] && !in_dirty[j as usize] {
                in_dirty[j as usize] = true;
                dirty.push(j);
            }
        };
        for &(i, _) in &to_promote {
            let cand = &candidates[i as usize];
            if let Some(deps) = qualified_deps
                .get(&cand.module)
                .and_then(|by_local| by_local.get(cand.name.as_str()))
            {
                deps.iter().for_each(|&j| enqueue(j));
            }
            if let Some(deps) = unqualified_deps.get(cand.name.as_str()) {
                deps.iter().for_each(|&j| enqueue(j));
            }
        }
    }
    (all_promoted, globally_safe_funcs)
}

/// Like `lookup_in_safety_map` but returns the resolved verdict when it is
/// non-blocking (`Safe` or `UnsafeIfImported`), else `None`.
fn lookup_verdict_in_safety_map(
    local_name: &str,
    fs: &AHashMap<String, FunctionSafetyInfo>,
) -> Option<FunctionSafety> {
    match fs.get(local_name)?.verdict {
        v @ (FunctionSafety::Safe | FunctionSafety::UnsafeIfImported) => Some(v),
        _ => None,
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
            self.function_safety
                .entry(name)
                .and_modify(|existing| {
                    existing.merge(info.clone());
                })
                .or_insert(info);
        }
        // Keep mutation candidates from every duplicate
        self.mutation_candidates.extend(other.mutation_candidates);
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
            .install(|| cache.clear_errors_where(&resolver, |_| true));

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
