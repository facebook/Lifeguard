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

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_text_size::TextRange;
use tracing::debug;

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
use crate::cache::merge::merge_class_properties;
use crate::cache::merge::retain_unverified_errors;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
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
use crate::safety_resolver::DecoratorVerdictMap;
use crate::safety_resolver::SafetyResolver;
use crate::traits::ModuleNameExt;

/// One or more libraries merged into a single module universe, with the bundled
/// stub graph injected -- the input to cross-library resolution.
///
/// Holding this type means the facts are merged but *not* resolved.
/// [`Self::resolve`] consumes it, which is what keeps the two states apart.
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
    /// Wrap an already merged cache, the graph-only stubs injected into it, and
    /// whatever `merge_dep_caches` folded together on the way. Passing
    /// `MergedClassFacts::default()` is only correct when nothing was merged --
    /// otherwise the folded class facts are silently dropped.
    ///
    /// Only tests want this, so the public door is
    /// [`crate::test_lib::reduce_workspace_from_merged`]; this stays crate-private
    /// so no production caller can skip the stub-set invariant.
    pub(crate) fn from_merged(
        cache: LibraryCache,
        graph_only_stubs: AHashSet<ModuleName>,
        merged: MergedClassFacts,
    ) -> Self {
        let artifact_module_count = cache
            .modules
            .len()
            .checked_sub(graph_only_stubs.len())
            .expect("graph-only stub count should not exceed cached module count");
        Self {
            cache,
            graph_only_stubs,
            artifact_module_count,
            merged,
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

    /// The bundled stubs injected as graph-only nodes when this workspace was
    /// built. They are part of the merged graph but carry no verdict.
    pub fn graph_only_stubs(&self) -> &AHashSet<ModuleName> {
        &self.graph_only_stubs
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

    pub fn modules(&self) -> &[CachedModule] {
        &self.cache.modules
    }

    /// Convenience lookup for tests that inspect resolved cache contents.
    pub fn find_module(&self, name: ModuleName) -> Option<&CachedModule> {
        self.cache.modules.iter().find(|module| module.name == name)
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

/// The merged facts a resolver is built from, threaded together so the two
/// `finalize_resolution` builds differ only in the module set they answer for.
struct ResolutionContext<'a> {
    /// Every module in the merge.
    module_names: &'a AHashSet<ModuleName>,
    /// Module -> the targets its missing and ambiguous imports newly resolved to.
    resolved_by_module: &'a AHashMap<ModuleName, AHashSet<ModuleName>>,
    func_safety_by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    class_bases: &'a HashMap<ModuleName, Vec<ModuleName>>,
    constructor_callees: &'a HashMap<ModuleName, ConstructorCallees>,
}

impl<'a> ResolutionContext<'a> {
    /// A resolver over `modules`, carrying the class facts every build shares.
    fn resolver(
        &self,
        modules: &'a AHashSet<ModuleName>,
        globally_safe: &'a AHashSet<String>,
    ) -> SafetyResolver<'a> {
        SafetyResolver::with_safe_index(modules, self.func_safety_by_module, globally_safe)
            .with_class_bases(self.class_bases)
            .with_constructor_callees(self.constructor_callees)
            .with_mro_modules(self.module_names)
    }
}

/// The class a call to `receiver` returns, following re-export aliases until a
/// recorded return type is found.
///
/// Bounded rather than run to a fixpoint: a malformed cache could describe a
/// re-export cycle, and this runs per candidate over every module in the build.
/// A chain longer than the bound goes unresolved, which is what happened to
/// every chain before aliases were followed at all.
fn resolve_return_class(
    receiver: ModuleName,
    return_classes: &AHashMap<ModuleName, ModuleName>,
    reexport_targets: &AHashMap<ModuleName, ModuleName>,
) -> Option<ModuleName> {
    const MAX_REEXPORT_HOPS: usize = 8;
    let mut name = receiver;
    for _ in 0..MAX_REEXPORT_HOPS {
        if let Some(class) = return_classes.get(&name) {
            return Some(*class);
        }
        match reexport_targets.get(&name) {
            Some(next) if *next != name => name = *next,
            _ => return None,
        }
    }
    None
}

fn is_resolved_error_verified_safe(
    caller: ModuleName,
    error: &SafetyError,
    whole_program: &SafetyResolver,
    scoped: Option<&SafetyResolver>,
    promoted: &AHashSet<(ModuleName, String)>,
) -> bool {
    // A constructor call clears on the class's map-phase-recorded callees, which
    // are whole-program facts. Scoping that check to a module's newly resolved
    // imports would withhold it from every module that resolved nothing new.
    if let Some(cleared) = whole_program.recorded_constructor_clears(caller, error) {
        return cleared;
    }
    let qualified = unqualified_index_key(error.metadata.as_str()).is_none();
    if error.kind == ErrorKind::UnsafeDecoratorCall || !qualified {
        return whole_program.is_error_verified_safe(error);
    }
    // Own-module callees verify against merged verdicts, unless promoted:
    // promotion evidence is cross-module, so it stays scoped like any other.
    if matches!(
        error.kind,
        ErrorKind::UnsafeFunctionCall | ErrorKind::UnsafeMethodCall
    ) && whole_program
        .split_at_module(error.metadata.as_str().trim_end_matches("()"))
        .is_some_and(|(module, local)| {
            module == caller && !promoted.contains(&(module, local.to_owned()))
        })
    {
        return whole_program.is_error_verified_safe(error);
    }
    scoped.is_some_and(|scoped| scoped.is_error_verified_safe(error))
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

    /// Clear cached errors that are verified safe by the resolved program facts.
    ///
    /// Qualified callees use the owning module's newly resolved imports,
    /// except bound calls to the caller's own module, which use the merged
    /// verdicts unless the callee was promoted. Unqualified callees use the
    /// whole-program safe-name index, and an `UnsafeDecoratorCall` uses the
    /// whole merged program's static verdicts.
    /// A qualified `UnknownDecoratorCall` is not in that last group: it names a
    /// callee, so it resolves like any other qualified error.
    fn finalize_resolution(&mut self, ctx: &ResolutionContext<'_>, outcome: &ResolutionOutcome) {
        // The map is built from the same facts `ctx.resolver` would use, and
        // hands out the whole-program resolver itself; the per-module scoped
        // resolvers below are over a different module set, so they cannot reach
        // its cache.
        let decorator_verdicts =
            DecoratorVerdictMap::new(ctx.module_names, ctx.func_safety_by_module);
        let whole_program = decorator_verdicts
            .resolver(&outcome.globally_safe)
            .with_class_bases(ctx.class_bases)
            .with_constructor_callees(ctx.constructor_callees);
        let promoted: AHashSet<(ModuleName, String)> = outcome.promoted.iter().cloned().collect();

        self.modules.par_iter_mut().for_each(|module| {
            let caller = module.name;
            let CachedSafety::Ok(ref mut safety) = module.safety else {
                return;
            };
            let scoped = ctx
                .resolved_by_module
                .get(&caller)
                .map(|resolved| ctx.resolver(resolved, &outcome.globally_safe));

            retain_unverified_errors(safety, |error| {
                is_resolved_error_verified_safe(
                    caller,
                    error,
                    &whole_program,
                    scoped.as_ref(),
                    &promoted,
                )
            });
        });
        debug!("{} functions promoted", outcome.promoted.len());
    }

    /// Turn recorded attribute accesses into errors where the merged facts now
    /// show the receiver's attribute is a property whose getter is not safe.
    ///
    /// This runs after `finalize_resolution` so the getter verdicts it reads are
    /// final and the errors it adds are not then considered for clearing.
    fn resolve_property_candidates(
        &mut self,
        module_names: &AHashSet<ModuleName>,
        func_safety_by_module: &AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
        outcome: &ResolutionOutcome,
        class_properties: &HashMap<ModuleName, AHashSet<String>>,
    ) {
        if class_properties.is_empty() {
            return;
        }
        let return_classes: AHashMap<ModuleName, ModuleName> = self
            .exports
            .return_types
            .iter()
            .map(|rt| (rt.function, rt.class))
            .collect();
        // A candidate's receiver names whatever the caller imported, which may be
        // a re-export: `facade.make` where the return type was recorded against
        // `factory.make`. Looking up the alias alone misses, the candidate is
        // dropped, and dropping one is a false-safe -- no error is emitted for a
        // getter that is unsafe.
        let reexport_targets: AHashMap<ModuleName, ModuleName> = self
            .exports
            .re_exports
            .iter()
            .map(|re| {
                (
                    re.exported_module.append_str(&re.exported_attr),
                    re.imported_module.append_str(&re.imported_attr),
                )
            })
            .collect();
        let resolver = SafetyResolver::with_safe_index(
            module_names,
            func_safety_by_module,
            &outcome.globally_safe,
        );

        self.modules.par_iter_mut().for_each(|module| {
            let candidates = std::mem::take(&mut module.property_candidates);
            let CachedSafety::Ok(ref mut safety) = module.safety else {
                return;
            };
            for candidate in candidates {
                let Some((receiver, attr)) = candidate.attribute.split_attr() else {
                    continue;
                };
                let class_fqn =
                    match resolve_return_class(receiver, &return_classes, &reexport_targets) {
                        Some(class) => class,
                        None => receiver,
                    };
                if !class_properties
                    .get(&class_fqn)
                    .is_some_and(|properties| properties.contains(attr.as_str()))
                {
                    continue;
                }
                let attribute = class_fqn.append_str(attr.as_str());
                let getter_unsafe = resolver
                    .split_at_module(attribute.as_str())
                    .and_then(|(module, local)| resolver.own_verdict(&module, local))
                    .is_some_and(|verdict| !verdict.is_safe());
                if getter_unsafe {
                    safety.errors.push(SafetyError::new(
                        ErrorKind::UnsafeMethodCall,
                        attribute.as_str().to_owned(),
                        candidate.range,
                    ));
                }
            }
        });
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
                    let Some(name) = unqualified_index_key(error.metadata.as_str()) else {
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

    /// Resolve missing imports against the merged cache and selectively clear
    /// false errors using per-function safety verdicts.
    fn resolve_cross_library_errors(&mut self, merged: MergedClassFacts) {
        let module_names: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let ambiguous_resolved = self.resolve_ambiguous_imports(&module_names);

        let mut class_bases = merged.class_bases;
        fold_fqn_lists(&mut class_bases, std::mem::take(&mut self.class_bases));
        let mut constructor_callees = merged.constructor_callees;
        fold_constructor_callees(
            &mut constructor_callees,
            std::mem::take(&mut self.constructor_callees),
        );
        let class_properties = merge_class_properties(std::mem::take(&mut self.class_properties));

        self.propagate_re_export_safety();

        let mut func_safety_by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            self.modules
                .iter_mut()
                .map(|m| (m.name, std::mem::take(&mut m.function_safety)))
                .collect();

        let resolved_by_module: AHashMap<ModuleName, AHashSet<ModuleName>> = self
            .modules
            .par_iter_mut()
            .filter_map(|module| {
                if let CachedSafety::Ok(ref mut safety) = module.safety {
                    dedupe_implicit_imports(&mut safety.implicit_imports);
                }

                let from_ambiguous = ambiguous_resolved.get(&module.name);

                if module.missing_imports.is_empty() && from_ambiguous.is_none() {
                    return None;
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
                    // A recorded constructor callee outranks the class's
                    // aggregate verdict here too, or the error is gone before
                    // `finalize_resolution` ever sees it.
                    retain_unverified_errors(safety, |error| {
                        match resolver.recorded_constructor_clears(caller, error) {
                            Some(cleared) => cleared,
                            None => resolver.is_error_verified_safe(error),
                        }
                    });
                }

                Some((module.name, resolved_modules))
            })
            .collect();

        let needed_unqualified = self.unqualified_error_names();
        let mut module_errors: HashMap<ModuleName, Vec<(String, TextRange)>> = HashMap::new();
        let outcome = resolve_program(
            &module_names,
            &mut func_safety_by_module,
            self.modules
                .iter()
                .map(|module| (module.name, module.mutation_candidates.as_slice())),
            needed_unqualified,
            |module_name, metadata, range| {
                module_errors
                    .entry(module_name)
                    .or_default()
                    .push((metadata, range));
            },
        );
        for module in &mut self.modules {
            let Some(errors) = module_errors.get(&module.name) else {
                continue;
            };
            if let CachedSafety::Ok(ref mut safety) = module.safety {
                safety.errors.extend(errors.iter().map(|(metadata, range)| {
                    SafetyError::new(ErrorKind::ImportedVarArgument, metadata.clone(), *range)
                }));
            }
        }

        self.finalize_resolution(
            &ResolutionContext {
                module_names: &module_names,
                resolved_by_module: &resolved_by_module,
                func_safety_by_module: &func_safety_by_module,
                class_bases: &class_bases,
                constructor_callees: &constructor_callees,
            },
            &outcome,
        );

        self.resolve_property_candidates(
            &module_names,
            &func_safety_by_module,
            &outcome,
            &class_properties,
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
                return_types: Vec::new(),
            },
            ..Default::default()
        };
        let graph_only_stubs = AHashSet::from_iter([ModuleName::from_str("missing_stub")]);

        ReduceWorkspace::from_merged(cache, graph_only_stubs, MergedClassFacts::default());
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
                range: TextRange::default(),
            });
            LibraryCache {
                modules: vec![cached_module],
                exports: CachedExports {
                    re_exports: Vec::new(),
                    return_types: Vec::new(),
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
    fn every_module_with_new_evidence_is_processed() {
        let module_a = ModuleName::from_str("test.module_a");
        let module_b = ModuleName::from_str("test.module_b");
        let dep_a = ModuleName::from_str("dep_a");
        let dep_b = ModuleName::from_str("dep_b");

        let caller = |name: ModuleName, dep: ModuleName| CachedModule {
            safety: CachedSafety::Ok(CachedModuleSafety {
                errors: vec![SafetyError::new(
                    ErrorKind::UnknownFunctionCall,
                    format!("{}.helper()", dep.as_str()),
                    TextRange::default(),
                )],
                ..Default::default()
            }),
            missing_imports: [dep].into_iter().collect(),
            ..CachedModule::empty(name)
        };
        let dependency = |name: ModuleName| CachedModule {
            function_safety: [(
                "helper".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
            ..CachedModule::empty(name)
        };

        let mut cache = LibraryCache {
            modules: vec![
                caller(module_a, dep_a),
                caller(module_b, dep_b),
                dependency(dep_a),
                dependency(dep_b),
            ],
            exports: CachedExports {
                re_exports: Vec::new(),
                return_types: Vec::new(),
            },
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        assert!(
            cache.modules.iter().all(CachedModule::is_safe),
            "every module's verified error should be cleared, got {:?}",
            cache
                .modules
                .iter()
                .map(|m| (m.name.as_str(), m.is_safe()))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn own_module_callee_clears_without_new_imports() {
        // A call to the caller's own module verifies against the merged
        // verdicts, which carry this reduce's resolutions of the caller's own
        // functions: no newly resolved import is needed to clear it. An
        // external callee with no new evidence stays scoped and is kept.
        let module_m = ModuleName::from_str("test.module_m");
        let external = ModuleName::from_str("test.external");

        let caller = CachedModule {
            safety: CachedSafety::Ok(CachedModuleSafety {
                errors: vec![
                    SafetyError::new(
                        ErrorKind::UnsafeFunctionCall,
                        format!("{}.helper()", module_m.as_str()),
                        TextRange::default(),
                    ),
                    SafetyError::new(
                        ErrorKind::UnsafeFunctionCall,
                        format!("{}.helper()", external.as_str()),
                        TextRange::default(),
                    ),
                ],
                ..Default::default()
            }),
            function_safety: [(
                "helper".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
            ..CachedModule::empty(module_m)
        };
        let dependency = CachedModule {
            function_safety: [(
                "helper".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
            ..CachedModule::empty(external)
        };

        let mut cache = LibraryCache {
            modules: vec![caller, dependency],
            exports: CachedExports {
                re_exports: Vec::new(),
                return_types: Vec::new(),
            },
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let errors = cache
            .modules
            .iter()
            .find(|m| m.name == module_m)
            .and_then(|m| match &m.safety {
                CachedSafety::Ok(safety) => Some(
                    safety
                        .errors
                        .iter()
                        .map(|e| e.metadata.as_str().to_owned())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .expect("caller module should survive resolution");
        assert_eq!(
            errors,
            vec![format!("{}.helper()", external.as_str())],
            "own-module error should clear, external error should stay"
        );
    }

    #[test]
    fn unbound_own_module_callee_stays_scoped() {
        // An unbound callee never verifies against merged verdicts, even in
        // the caller's own module: only newly resolved imports re-bind it.
        let module_m = ModuleName::from_str("test.module_m");

        let caller = CachedModule {
            safety: CachedSafety::Ok(CachedModuleSafety {
                errors: vec![SafetyError::new(
                    ErrorKind::UnknownFunctionCall,
                    format!("{}.helper()", module_m.as_str()),
                    TextRange::default(),
                )],
                ..Default::default()
            }),
            function_safety: [(
                "helper".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
            ..CachedModule::empty(module_m)
        };

        let mut cache = LibraryCache {
            modules: vec![caller],
            exports: CachedExports {
                re_exports: Vec::new(),
                return_types: Vec::new(),
            },
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        assert!(
            !cache.modules.iter().all(CachedModule::is_safe),
            "unbound own-module error should stay without new imports"
        );
    }

    #[test]
    fn promoted_own_module_callee_stays_scoped() {
        // A callee the reduce promoted verified against cross-module
        // evidence, so its callers stay scoped to newly resolved imports.
        let module_m = ModuleName::from_str("test.module_m");
        let dep = ModuleName::from_str("dep");

        let caller = CachedModule {
            safety: CachedSafety::Ok(CachedModuleSafety {
                errors: vec![SafetyError::new(
                    ErrorKind::UnsafeFunctionCall,
                    format!("{}.helper()", module_m.as_str()),
                    TextRange::default(),
                )],
                ..Default::default()
            }),
            function_safety: [(
                "helper".to_owned(),
                FunctionSafetyInfo::unsafe_missing_dep(ModuleName::from_str("dep.fn")),
            )]
            .into_iter()
            .collect(),
            ..CachedModule::empty(module_m)
        };
        let dependency = CachedModule {
            function_safety: [(
                "fn".to_owned(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]
            .into_iter()
            .collect(),
            ..CachedModule::empty(dep)
        };

        let mut cache = LibraryCache {
            modules: vec![caller, dependency],
            exports: CachedExports {
                re_exports: Vec::new(),
                return_types: Vec::new(),
            },
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let caller = cache
            .modules
            .iter()
            .find(|m| m.name == module_m)
            .expect("caller module should survive resolution");
        assert_eq!(
            caller
                .function_safety
                .get("helper")
                .map(|info| info.verdict),
            Some(FunctionSafety::Safe),
            "helper should have been promoted"
        );
        let CachedSafety::Ok(safety) = &caller.safety else {
            panic!("caller module should be analyzable");
        };
        assert_eq!(
            safety.errors.len(),
            1,
            "promoted-callee error should stay without new imports"
        );
    }
}
