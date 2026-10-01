/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Combining artifacts.
//!
//! One module can be compiled into several libraries, so a reduce over N caches
//! sees N records for it. This is where those records are coalesced, and where
//! records a library does not own are dropped. Nothing here resolves anything:
//! a merge answers "what do the caches jointly say", not "is it safe".

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::cache::CachedModule;
use crate::cache::CachedModuleSafety;
use crate::cache::CachedReExport;
use crate::cache::CachedSafety;
use crate::cache::ConstructorCallees;
use crate::cache::LibraryCache;
use crate::cache::reduce::MergedClassFacts;
use crate::errors::SafetyError;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::ModuleSafety;
use crate::module_safety::MutationCandidate;
use crate::module_safety::SafetyResult;

/// Working map used to dedup re-exports during the reduce, keyed by the exported
/// `(module, attr)`; the value is one representative record per unique key.
type ReExportDedupMap = AHashMap<ModuleName, AHashMap<String, CachedReExport>>;

impl LibraryCache {
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
}

impl LibraryCache {
    /// Merge dependency caches into this cache.
    /// When the same module appears in multiple caches (a .py file can belong
    /// to more than one python_library), module data is merged:
    /// - imports / side_effect_imports: union
    /// - missing_imports: intersection (only truly missing if unresolved everywhere)
    /// - safety: most conservative (most errors)
    pub fn merge_dep_caches(&mut self, dep_caches: Vec<LibraryCache>) -> MergedClassFacts {
        let mut merged = MergedClassFacts::default();
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
            fold_fqn_lists(&mut merged.class_bases, dep.class_bases);
            fold_constructor_callees(&mut merged.constructor_callees, dep.constructor_callees);
            self.class_properties.extend(dep.class_properties);
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

        merged
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
}

/// Merge `incoming` into `fs[attr]`, inserting a clone if absent. Returns whether
/// the entry changed (so callers can decide whether to reprocess dependents).
/// Borrows `incoming` so the caller need not clone it before a merge that only
/// updates an existing entry (the common re-processing case).
pub(super) fn merge_function_safety_entry_ref(
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
    /// Merge another CachedModule (same name) into this one.
    pub(crate) fn merge(&mut self, other: CachedModule) {
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
        // Unlike mutation candidates, property candidates are resolved independently of
        // each other, so they can simply be sorted and deduped.
        self.property_candidates.extend(other.property_candidates);
        self.property_candidates.sort_unstable();
        self.property_candidates.dedup();
    }
}
impl CachedSafety {
    /// Merge another safety result, keeping the more conservative outcome.
    /// AnalysisError always wins. Between two Ok results, keep the union of errors.
    pub(crate) fn merge(&mut self, other: CachedSafety) {
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
                    module_safety.add_error(*error);
                }
                for override_err in &safety.force_imports_eager_overrides {
                    module_safety.add_force_import_override(*override_err);
                }
                module_safety.implicit_imports = safety.implicit_imports.clone();
                SafetyResult::Ok(module_safety)
            }
            CachedSafety::AnalysisError { message } => {
                SafetyResult::AnalysisError(anyhow::anyhow!("{}", message))
            }
        }
    }

    pub(crate) fn from_safety_result(result: &SafetyResult) -> Self {
        match result {
            SafetyResult::Ok(safety) => CachedSafety::Ok(CachedModuleSafety {
                errors: safety.errors.clone(),
                force_imports_eager_overrides: safety.force_imports_eager_overrides.clone(),
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
pub(crate) fn merge_errors(target: &mut Vec<SafetyError>, other: Vec<SafetyError>) {
    target.extend(other);
    target.sort();
    target.dedup();
}

/// Drop errors on `safety` that `is_verified_safe` confirms are safe, leaving
/// the rest. Returns whether any error was removed.
pub(super) fn retain_unverified_errors(
    safety: &mut CachedModuleSafety,
    mut is_verified_safe: impl FnMut(&SafetyError) -> bool,
) -> bool {
    let before = safety.errors.len();
    safety
        .errors
        .retain(|e| !e.kind.could_be_caused_by_missing_import() || !is_verified_safe(e));
    safety.errors.len() < before
}
/// Fold one library's `(class FQN, bases)` pairs into a lookup map, merging any
/// FQN contributed by more than one library (e.g. a stub and the real module,
/// or overlapping targets). Bases are unioned preserving first-seen order so the
/// result is independent of dep-cache fold order — a bare `collect()` would
/// instead keep whichever tuple landed last.
pub(super) fn fold_fqn_lists(
    merged: &mut HashMap<ModuleName, Vec<ModuleName>>,
    entries: Vec<(ModuleName, Vec<ModuleName>)>,
) {
    for (class_fqn, bases) in entries {
        let existing = merged.entry(class_fqn).or_default();
        for base in bases {
            if !existing.contains(&base) {
                existing.push(base);
            }
        }
    }
}

/// Fold one library's constructor-callee records into a lookup map, unioning any
/// class recorded by more than one library.
pub(super) fn fold_constructor_callees(
    merged: &mut HashMap<ModuleName, ConstructorCallees>,
    entries: Vec<(ModuleName, ConstructorCallees)>,
) {
    for (class_fqn, recorded) in entries {
        match merged.entry(class_fqn) {
            Entry::Occupied(mut slot) => {
                let combined = std::mem::take(slot.get_mut()).merged_with(recorded);
                if combined.is_empty() {
                    slot.remove();
                } else {
                    slot.insert(combined);
                }
            }
            Entry::Vacant(slot) => {
                if !recorded.is_empty() {
                    slot.insert(recorded);
                }
            }
        }
    }
}
/// Fold accumulated `(class FQN, property names)` pairs into a lookup set,
/// unioning FQNs contributed by more than one library for the same reason
/// [`merge_class_bases`] does.
pub(super) fn merge_class_properties(
    entries: Vec<(ModuleName, Vec<String>)>,
) -> HashMap<ModuleName, AHashSet<String>> {
    let mut merged: HashMap<ModuleName, AHashSet<String>> = HashMap::with_capacity(entries.len());
    for (class_fqn, properties) in entries {
        merged.entry(class_fqn).or_default().extend(properties);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_class_bases_unions_duplicate_class_fqns() {
        let c = ModuleName::from_str("pkg.mod.C");
        let base_a = ModuleName::from_str("pkg.mod.A");
        let base_b = ModuleName::from_str("pkg.mod.B");

        // Two libraries contribute `pkg.mod.C`: a duplicate base and a new one.
        let mut merged = HashMap::new();
        fold_fqn_lists(&mut merged, vec![(c, vec![base_a])]);
        fold_fqn_lists(&mut merged, vec![(c, vec![base_a, base_b])]);

        assert_eq!(
            merged.get(&c),
            Some(&vec![base_a, base_b]),
            "duplicate FQN base lists union preserving first-seen order, not last-wins",
        );
    }
}
