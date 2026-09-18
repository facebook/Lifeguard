/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The two phases that turn merged facts into final verdicts.
//!
//! [`ReduceWorkspace`] holds facts that are merged but unresolved, and
//! [`ResolvedCache`] holds them once cross-library resolution has run. The
//! transition consumes the workspace, which is what keeps the states apart.
//!
//! The `LibraryCache` methods here settle import edges against the merged
//! module set, clear errors the merge has verified, and discharge obligations
//! the map could not close. They stay private to this module so callers cannot
//! resolve a `LibraryCache` in place while retaining its unresolved type.

use std::collections::HashMap;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use tracing::debug;

use crate::cache::artifact::CachedError;
#[cfg(test)]
use crate::cache::artifact::CachedExports;
use crate::cache::artifact::CachedModule;
#[cfg(test)]
use crate::cache::artifact::CachedModuleSafety;
use crate::cache::artifact::CachedReExport;
use crate::cache::artifact::CachedSafety;
use crate::cache::artifact::ConstructorCallees;
use crate::cache::artifact::LibraryCache;
use crate::cache::merge::dedupe_implicit_imports;
use crate::cache::merge::fold_constructor_callees;
use crate::cache::merge::fold_fqn_lists;
use crate::cache::merge::retain_unverified_errors;
use crate::errors::ErrorKind;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::FixedState;
#[cfg(test)]
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::hasher::union_larger;
use crate::imports::ImportGraph;
use crate::imports::resolve_to_known_module;
#[cfg(test)]
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::pyrefly::sys_info::PythonVersion;
use crate::resolution::ResolutionOutcome;
use crate::resolution::resolve_program;
use crate::resolution::unqualified_index_key;
use crate::safety_resolver::SafetyResolver;

/// Mutable reduce workspace decoded from one or more serialized library artifacts.
pub struct ReduceWorkspace {
    cache: LibraryCache,
    graph_only_stubs: AHashSet<ModuleName>,
    artifact_module_count: usize,
    merged: MergedClassFacts,
}

/// Class facts folded together as dependency caches are consumed, so the
/// un-deduplicated concatenation of every dep's entries never exists.
///
/// This is reduce state, not artifact state: a cache read off disk has none of
/// it, and nothing here is ever written back out.
#[derive(Default)]
pub struct MergedClassFacts {
    pub(super) class_bases: HashMap<ModuleName, Vec<ModuleName>>,
    pub(super) constructor_callees: HashMap<ModuleName, ConstructorCallees>,
}

/// A reduce workspace after all cross-library semantic resolution has completed.
///
/// This intentionally retains the workspace's cache and stub facts: the
/// distinct type prevents output construction before `resolve` has consumed
/// the mutable workspace and completed semantic resolution.
pub struct ResolvedCache {
    cache: LibraryCache,
    graph_only_stubs: AHashSet<ModuleName>,
}

impl ReduceWorkspace {
    /// Wrap an already merged cache and the graph-only stubs injected into it,
    /// bypassing the stub injection that `single` and `merge` perform. Only
    /// tests want that, so the public door is
    /// [`crate::test_lib::reduce_workspace_from_merged`]; this stays crate-private
    /// so no production caller can skip the stub-set invariant.
    pub(crate) fn from_merged(cache: LibraryCache, graph_only_stubs: AHashSet<ModuleName>) -> Self {
        let artifact_module_count = cache
            .modules
            .len()
            .checked_sub(graph_only_stubs.len())
            .expect("graph-only stub count should not exceed cached module count");
        Self {
            cache,
            graph_only_stubs,
            artifact_module_count,
            merged: MergedClassFacts::default(),
        }
    }

    /// Prepare a single serialized cache for reduction by injecting bundled stubs.
    pub fn single(cache: LibraryCache, python_version: PythonVersion) -> Self {
        Self::single_with(cache, python_version, MergedClassFacts::default())
    }

    /// `single`, for a cache whose dependencies have already been folded in.
    fn single_with(
        mut cache: LibraryCache,
        python_version: PythonVersion,
        merged: MergedClassFacts,
    ) -> Self {
        let artifact_module_count = cache.modules.len();
        let graph_only_stubs = cache.inject_bundled_stub_graph(python_version);
        Self {
            cache,
            graph_only_stubs,
            artifact_module_count,
            merged,
        }
    }

    /// Merge a nonempty set of serialized caches and inject bundled stubs.
    pub fn merge(
        mut caches: Vec<LibraryCache>,
        python_version: PythonVersion,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(!caches.is_empty(), "cannot reduce an empty cache set");
        // Preserve the historical merge base and remainder order: duplicate
        // module facts can contain order-sensitive mutation candidates.
        let mut cache = caches.swap_remove(0);
        let merged = if caches.is_empty() {
            MergedClassFacts::default()
        } else {
            cache.merge_dep_caches(caches)
        };
        Ok(Self::single_with(cache, python_version, merged))
    }

    /// Return the total number of modules, including injected bundled stubs.
    pub fn module_count(&self) -> usize {
        self.cache.modules.len()
    }

    /// Return the number of modules contributed by serialized cache artifacts.
    pub fn artifact_module_count(&self) -> usize {
        self.artifact_module_count
    }

    /// Resolve cross-library errors and consume the mutable reduce workspace.
    pub fn resolve(mut self) -> ResolvedCache {
        self.cache.resolve_cross_library_errors(self.merged);
        ResolvedCache {
            cache: self.cache,
            graph_only_stubs: self.graph_only_stubs,
        }
    }
}

impl ResolvedCache {
    pub(crate) fn resolved_cache(&self) -> &LibraryCache {
        &self.cache
    }

    pub(crate) fn modules(&self) -> &[CachedModule] {
        &self.cache.modules
    }

    pub(crate) fn graph_only_stubs(&self) -> &AHashSet<ModuleName> {
        &self.graph_only_stubs
    }

    pub(crate) fn re_exports(&self) -> &[CachedReExport] {
        &self.cache.exports.re_exports
    }

    pub(crate) fn build_import_graph(&self) -> ImportGraph {
        self.cache.to_import_graph()
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
        constructor_callees: &HashMap<ModuleName, ConstructorCallees>,
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
            .with_class_bases(class_bases)
            .with_constructor_callees(constructor_callees);
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
                    .with_class_bases(class_bases)
                    .with_constructor_callees(constructor_callees);
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
    pub fn resolve_cross_library_errors(&mut self, merged: MergedClassFacts) {
        let module_names: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let ambiguous_resolved = self.resolve_ambiguous_imports(&module_names);

        let mut class_bases = merged.class_bases;
        fold_fqn_lists(&mut class_bases, std::mem::take(&mut self.class_bases));
        let mut constructor_callees = merged.constructor_callees;
        fold_constructor_callees(
            &mut constructor_callees,
            std::mem::take(&mut self.constructor_callees),
        );

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

            let caller = module.name;
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                let resolver = SafetyResolver::new(&resolved_modules, &func_safety_by_module)
                    .with_class_bases(&class_bases)
                    .with_constructor_callees(&constructor_callees);
                // Same decision as the final clear: this pass has no promotion
                // evidence to gate on, but a recorded constructor callee still
                // has to outrank the class's aggregate verdict here, or the
                // error is gone before `finalize_resolution` ever sees it.
                retain_unverified_errors(safety, |error| {
                    resolver.clears_error(caller, error, |_| true)
                });
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
            &constructor_callees,
        );

        // Return the verdicts taken at the top; resolution needed them in one flat
        // map to do cross-module lookups while `self.modules` was borrowed mutably.
        for module in &mut self.modules {
            if let Some(fs) = func_safety_by_module.remove(&module.name) {
                module.function_safety = fs;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rayon::ThreadPoolBuilder;

    use super::*;
    use crate::effects::ImportedArgs;
    use crate::module_safety::MutationCandidate;
    use crate::module_safety::MutationCandidateSite;

    #[test]
    fn reduce_workspace_rejects_empty_cache_set() {
        assert!(ReduceWorkspace::merge(Vec::new(), PythonVersion::default()).is_err());
    }

    #[test]
    #[should_panic(expected = "graph-only stub count should not exceed cached module count")]
    fn reduce_workspace_rejects_inconsistent_stub_count() {
        let cache = LibraryCache {
            modules: Vec::new(),
            exports: CachedExports {
                re_exports: Vec::new(),
            },
            ..Default::default()
        };
        let graph_only_stubs = AHashSet::from_iter([ModuleName::from_str("missing_stub")]);

        ReduceWorkspace::from_merged(cache, graph_only_stubs);
    }

    #[test]
    fn reduce_workspace_merge_preserves_historical_cache_order() {
        fn cache_with_candidate(module: ModuleName, call: &str) -> LibraryCache {
            let mut cached_module = CachedModule::empty(module);
            cached_module.mutation_candidates.push(MutationCandidate {
                callee: ModuleName::from_str("dependency.mutate"),
                site: MutationCandidateSite::ModuleScope {
                    call: ModuleName::from_str(call),
                },
                arg_offset: 0,
                imported_args: ImportedArgs::default(),
            });
            LibraryCache {
                modules: vec![cached_module],
                exports: CachedExports {
                    re_exports: Vec::new(),
                },
                ..Default::default()
            }
        }

        let module = ModuleName::from_str("pkg.module");
        let workspace = ReduceWorkspace::merge(
            vec![
                cache_with_candidate(module, "first"),
                cache_with_candidate(module, "middle"),
                cache_with_candidate(module, "last"),
            ],
            PythonVersion::default(),
        )
        .expect("nonempty caches should merge");
        let merged = workspace
            .cache
            .modules
            .iter()
            .find(|cached| cached.name == module)
            .expect("merged cache should contain the input module");
        let calls: Vec<&str> = merged
            .mutation_candidates
            .iter()
            .map(|candidate| match &candidate.site {
                MutationCandidateSite::ModuleScope { call } => call.as_str(),
                _ => panic!("expected module-scope mutation candidate"),
            })
            .collect();

        assert_eq!(calls, ["first", "last", "middle"]);
    }

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
            ..Default::default()
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
