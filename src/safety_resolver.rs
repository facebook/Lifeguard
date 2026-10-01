/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Answering "is this callee safe?" against merged facts.
//!
//! The reduce clears an error when the merge can show its callee safe, and
//! promotes a function when everything it was waiting on resolves. Both ask the
//! same question of the same data, and the answer depends on three choices this
//! module owns: which module set a qualified name resolves against, whether an
//! unqualified name may fall back to a same-named function elsewhere, and
//! whether a class method may be inherited rather than defined.
//!
//! Those choices are easy to get subtly wrong -- refusing the MRO along with the
//! unqualified fallback strands every cross-library inherited call, and letting
//! an own verdict fall through to a base clears an override on its parent -- so
//! they live together rather than beside the cache schema.

use std::borrow::Cow;
use std::collections::HashMap;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;

use crate::cache::CONSTRUCTOR_METHODS;
use crate::cache::CachedReExport;
use crate::cache::ConstructorCallees;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::FixedState;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::mro::c3_linearize;
use crate::names::enclosing_module;
use crate::traits::ModuleNameExt;

/// Whether `local_name` is cached `Safe` in `fs`.
/// Inherited methods resolve up the class MRO in the resolver
/// (`SafetyResolver::mro_method_verdict`).
fn lookup_in_safety_map(local_name: &str, fs: &AHashMap<String, FunctionSafetyInfo>) -> bool {
    fs.get(local_name)
        .is_some_and(|info| info.verdict.is_safe())
}

/// Whether an unqualified decorator name is verified safe, memoized.
///
/// The answer is "safe given *these* modules and *these* verdicts", which is why
/// the map holds them and the re-export index and hands out the resolver itself:
/// a resolver over other facts has no way to reach this cache. The unqualified
/// decorator scan reads all three, so pinning only the module set would leave
/// the same sharing bug one level down.
pub(crate) struct DecoratorVerdictMap<'a> {
    modules: &'a AHashSet<ModuleName>,
    by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    re_exports: &'a ReExportIndex<'a>,
    entries: DashMap<String, bool, FixedState>,
}

impl<'a> DecoratorVerdictMap<'a> {
    pub(crate) fn new(
        modules: &'a AHashSet<ModuleName>,
        by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
        re_exports: &'a ReExportIndex<'a>,
    ) -> Self {
        Self {
            modules,
            by_module,
            re_exports,
            entries: DashMap::default(),
        }
    }

    /// A resolver over the facts these verdicts were computed from, memoising
    /// its unqualified decorator lookups here.
    ///
    /// The map hands out the resolver rather than being attached to one, so a
    /// resolver over different facts cannot reach this cache. `globally_safe`
    /// is the caller's, because the decorator scan never consults it.
    pub(crate) fn resolver(&'a self, globally_safe: &'a AHashSet<String>) -> SafetyResolver<'a> {
        let mut resolver =
            SafetyResolver::with_safe_index(self.modules, self.by_module, globally_safe);
        resolver.re_exports = Some(self.re_exports);
        resolver.decorator_verdicts = Some(self);
        resolver
    }
}

/// Merged re-exports keyed by the exporting module and name, each pointing at
/// the module and name it was imported from.
pub(crate) struct ReExportIndex<'a> {
    definitions: AHashMap<(ModuleName, &'a str), (ModuleName, &'a str)>,
}

impl<'a> ReExportIndex<'a> {
    pub(crate) fn new(re_exports: &'a [CachedReExport]) -> Self {
        Self {
            definitions: re_exports
                .iter()
                .map(|re| {
                    (
                        (re.exported_module, re.exported_attr.as_str()),
                        (re.imported_module, re.imported_attr.as_str()),
                    )
                })
                .collect(),
        }
    }

    /// The module and name that `module.name`'s re-export chain ends at, or
    /// `None` when the chain cycles.
    fn definition_of<'n>(&self, module: ModuleName, name: &'n str) -> Option<(ModuleName, &'n str)>
    where
        'a: 'n,
    {
        let mut current = (module, name);
        let mut visited = Vec::new();
        while let Some(&next) = self.definitions.get(&current) {
            if visited.contains(&current) {
                return None;
            }
            visited.push(current);
            current = next;
        }
        Some(current)
    }
}

/// The merged per-function verdicts plus the module set they resolve against —
/// shared context for reduce-time error clearing and promotion.
///
/// A qualified name (`mod.sub.func`) is split at its longest module prefix and
/// looked up there; an unqualified name (`helper`) uses `globally_safe` if
/// present, else scans `modules`.
#[derive(Clone, Copy)]
pub(crate) struct SafetyResolver<'a> {
    modules: &'a AHashSet<ModuleName>,
    by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    /// `Some` enables O(1) unqualified lookups; `None` scans `modules`.
    globally_safe: Option<&'a AHashSet<String>>,
    /// Caches `scan_unqualified_decorator_safe` by name, so its O(modules) scan
    /// runs once per distinct decorator instead of once per call site.
    decorator_verdicts: Option<&'a DecoratorVerdictMap<'a>>,
    /// Class FQN -> base FQNs, enabling MRO resolution of inherited
    /// `Class.method` calls when there is no exact method verdict.
    class_bases: Option<&'a HashMap<ModuleName, Vec<ModuleName>>>,
    /// Map-phase-resolved constructor callees, keyed by class FQN. When present
    /// for a class, these replace re-deriving its constructor method set.
    constructor_callees: Option<&'a HashMap<ModuleName, ConstructorCallees>>,
    /// Every module in the merge, used only to resolve MRO ancestors.
    mro_modules: Option<&'a AHashSet<ModuleName>>,
    re_exports: Option<&'a ReExportIndex<'a>>,
}

impl<'a> SafetyResolver<'a> {
    /// No prebuilt indices — the unqualified fallback scans `modules`.
    pub(crate) fn new(
        modules: &'a AHashSet<ModuleName>,
        by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
    ) -> Self {
        SafetyResolver {
            modules,
            by_module,
            globally_safe: None,
            decorator_verdicts: None,
            class_bases: None,
            constructor_callees: None,
            mro_modules: None,
            re_exports: None,
        }
    }

    /// Backed by the prebuilt globally-safe index for O(1) unqualified lookups.
    pub(crate) fn with_safe_index(
        modules: &'a AHashSet<ModuleName>,
        by_module: &'a AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>>,
        globally_safe: &'a AHashSet<String>,
    ) -> Self {
        SafetyResolver {
            modules,
            by_module,
            globally_safe: Some(globally_safe),
            decorator_verdicts: None,
            class_bases: None,
            constructor_callees: None,
            mro_modules: None,
            re_exports: None,
        }
    }

    /// Attach class base edges.
    pub(crate) fn with_class_bases(
        mut self,
        class_bases: &'a HashMap<ModuleName, Vec<ModuleName>>,
    ) -> Self {
        self.class_bases = Some(class_bases);
        self
    }

    /// Attach the map phase's resolved constructor callees.
    pub(crate) fn with_constructor_callees(
        mut self,
        constructor_callees: &'a HashMap<ModuleName, ConstructorCallees>,
    ) -> Self {
        self.constructor_callees = Some(constructor_callees);
        self
    }

    pub(crate) fn with_re_exports(mut self, re_exports: &'a ReExportIndex<'a>) -> Self {
        self.decorator_verdicts = None;
        self.re_exports = Some(re_exports);
        self
    }

    pub(crate) fn with_mro_modules(mut self, modules: &'a AHashSet<ModuleName>) -> Self {
        self.mro_modules = Some(modules);
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
        let ancestor_modules = self.mro_modules.unwrap_or(self.modules);
        for ancestor in c3_linearize(class_bases, &class_fqn).iter().skip(1) {
            let candidate = ancestor.append_str(method);
            if let Some((bmod, blocal)) = split_at_module_in(ancestor_modules, candidate.as_str()) {
                if let Some(info) = self.by_module.get(&bmod).and_then(|fs| fs.get(blocal)) {
                    return Some(info.verdict);
                }
            }
        }
        None
    }

    /// The longest prefix of `func_name` naming a module in `self.modules`,
    /// paired with the remaining local name; `None` if unqualified.
    pub(crate) fn split_at_module<'n>(&self, func_name: &'n str) -> Option<(ModuleName, &'n str)> {
        split_at_module_in(self.modules, func_name)
    }

    /// Whether an unqualified name is verified safe: the index when present,
    /// else a scan of `modules`.
    pub(crate) fn unqualified_safe(&self, func_name: &str) -> bool {
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
    pub(crate) fn own_verdict(&self, module: &ModuleName, local: &str) -> Option<FunctionSafety> {
        Some(self.by_module.get(module)?.get(local)?.verdict)
    }

    /// Whether `module`.`local` is verified safe: its own entry when it has one,
    /// otherwise the nearest inherited definition found up the MRO.
    fn qualified_safe(&self, module: &ModuleName, local: &str) -> bool {
        match self.own_verdict(module, local) {
            Some(verdict) => verdict.is_safe(),
            None => self
                .mro_method_verdict(module, local)
                .is_some_and(|verdict| verdict.is_safe()),
        }
    }

    /// Whether a plain function call is found and verified `Safe`.
    pub(crate) fn is_call_verified_safe(&self, func_name: &str) -> bool {
        match self.split_at_module(func_name) {
            Some((module, local)) => self.qualified_safe(&module, local),
            None => self.unqualified_safe(func_name),
        }
    }

    /// Like `is_call_verified_safe`, but an unqualified name never clears. An
    /// `Unknown*` call target means `func_name` is a best-effort textual name
    /// rather than a proven callee, so it must not clear on a same-named safe
    /// function in some resolved module (or in the global index). A qualified
    /// name is specific enough to trust, and clears exactly as it would for a
    /// resolved call, inherited methods included.
    fn is_call_verified_safe_no_unqualified(&self, func_name: &str) -> bool {
        self.split_at_module(func_name)
            .is_some_and(|(module, local)| self.qualified_safe(&module, local))
    }

    /// `is_decorator_call_verified_safe` restricted to a qualified name, for the
    /// same reason `is_call_verified_safe_no_unqualified` exists: an unbound
    /// short name has no proven callee to verify.
    fn is_decorator_call_verified_safe_no_unqualified(&self, func_name: &str) -> bool {
        self.split_at_module(func_name)
            .is_some_and(|(module, local)| self.decorator_safe_in(module, local))
    }

    /// Whether a parameterized-decorator call is safe: the factory AND every
    /// immediate nested function must be `Safe`, since the factory runs its
    /// returned wrapper at decoration time. Never consults `globally_safe`
    /// (own-verdict only).
    pub(crate) fn is_decorator_call_verified_safe(&self, func_name: &str) -> bool {
        // A qualified name names a callee, so its own verdict is the answer and
        // the unqualified fallback below must not rescue a `false`.
        if let Some((module, local)) = self.split_at_module(func_name) {
            return self.decorator_safe_in(module, local);
        }
        let Some(cache) = self.decorator_verdicts else {
            return self.scan_unqualified_decorator_safe(func_name);
        };
        if let Some(cached) = cache.entries.get(func_name) {
            return *cached;
        }
        let result = self.scan_unqualified_decorator_safe(func_name);
        cache.entries.insert(func_name.to_owned(), result);
        result
    }

    /// Whether any module has `func_name` as a decorator-verified-safe function.
    /// O(modules); callers should memoize by name (see `decorator_verdicts`).
    fn scan_unqualified_decorator_safe(&self, func_name: &str) -> bool {
        self.modules
            .iter()
            .any(|module| self.decorator_safe_in(*module, func_name))
    }

    /// Whether `local` is a decorator verified safe in `module`, looked up where
    /// its re-export chain ends when that has a verdict for it. A re-export copies
    /// only the factory's own verdict, so its nested functions are only visible
    /// at the definition.
    fn decorator_safe_in(&self, module: ModuleName, local: &str) -> bool {
        let (head, tail) = match local.split_once('.') {
            Some((head, tail)) => (head, Some(tail)),
            None => (local, None),
        };
        let (defining_module, defining_head) = match self.re_exports {
            Some(index) => match index.definition_of(module, head) {
                Some(definition) => definition,
                None => return false,
            },
            None => (module, head),
        };
        let defining_local = match tail {
            None => Cow::Borrowed(defining_head),
            Some(_) if (defining_module, defining_head) == (module, head) => Cow::Borrowed(local),
            Some(tail) => Cow::Owned(format!("{defining_head}.{tail}")),
        };
        match self.by_module.get(&defining_module) {
            Some(fs) if fs.contains_key(defining_local.as_ref()) => {
                lookup_decorator_in_safety_map(&defining_local, fs)
            }
            _ => self
                .by_module
                .get(&module)
                .is_some_and(|fs| lookup_decorator_in_safety_map(local, fs)),
        }
    }

    /// The combined verdict of the constructor callees the map phase recorded for
    /// `class_fqn`, or `None` when it recorded none (i.e. no visible constructor methods).
    fn recorded_constructor_verdict(&self, class_fqn: &ModuleName) -> Option<FunctionSafety> {
        let recorded = self.constructor_callees?.get(class_fqn)?;
        let derived = recorded
            .iter(*class_fqn)
            .map(|(owner, method)| self.callee_verdict(&owner, method));
        let extra = recorded
            .extra
            .iter()
            .map(|callee| self.recorded_callee_verdict(callee));
        derived.chain(extra).reduce(|acc, verdict| acc | verdict)
    }

    /// The verdict of a callee recorded by its full FQN, resolved the same way
    /// `callee_verdict` resolves a derived one.
    fn recorded_callee_verdict(&self, callee: &ModuleName) -> FunctionSafety {
        self.split_at_module(callee.as_str())
            .and_then(|(module, local)| self.own_verdict(&module, local))
            .unwrap_or(FunctionSafety::Unsafe)
    }

    /// A recorded callee's verdict, treating one that no longer resolves as
    /// `Unsafe`: the map phase saw it run, so losing sight of it is not evidence
    /// that it is safe. The callee's own FQN is never built as a `ModuleName`,
    /// since interning it would outlive the lookup.
    fn callee_verdict(&self, owner: &ModuleName, method: &str) -> FunctionSafety {
        self.split_at_module(owner.as_str())
            .and_then(|(module, local)| self.own_verdict(&module, &format!("{local}.{method}")))
            .unwrap_or(FunctionSafety::Unsafe)
    }

    /// Whether a constructor verdict lets its call clear. `UnsafeIfImported`
    /// means safe only within the defining module, so it clears only when the
    /// caller is that module.
    fn constructor_verdict_clears(
        &self,
        verdict: FunctionSafety,
        caller_module: &ModuleName,
        class_fqn: &ModuleName,
    ) -> bool {
        if verdict == FunctionSafety::Safe {
            return true;
        }
        // Only the module that defines the class may clear an `UnsafeIfImported`
        // constructor. Matching any ancestor package would let the importing
        // module clear it too, which is the opposite of what the verdict means.
        verdict == FunctionSafety::UnsafeIfImported
            && self
                .split_at_module(class_fqn.as_str())
                .is_some_and(|(defining, _)| &defining == caller_module)
    }

    /// Dispatch a cached error to the right verified-safe check, on the two
    /// properties that pick it: whether the call form also runs a returned
    /// wrapper, and whether the callee was bound to anything.
    pub(crate) fn is_error_verified_safe(&self, error: &SafetyError) -> bool {
        // The callee `metadata` may render with trailing `()` suffixes.
        let func_name = error.metadata.as_str().trim_end_matches("()");
        // Both fields, to guard against a stale artifact pairing them wrongly:
        // the kind and the flag travel separately through the cache.
        let parameterized_decorator = error.parameterized_decorator && error.is_decorator_call();

        match (error.kind, parameterized_decorator) {
            // Parameterized decorator without a proven binding.
            (ErrorKind::UnknownDecoratorCall, true) => {
                self.is_decorator_call_verified_safe_no_unqualified(func_name)
            }
            // The returned wrapper runs at decoration time too.
            (_, true) => self.is_decorator_call_verified_safe(func_name),
            // Nothing bound the callee, so do not treat the name as qualified.
            // `UnknownObject` belongs here for the same reason, and is not even a
            // call: it is an attribute access on a name the map could not resolve.
            (
                ErrorKind::UnknownFunctionCall
                | ErrorKind::UnknownMethodCall
                | ErrorKind::UnknownDecoratorCall
                | ErrorKind::UnknownObject,
                false,
            ) => self.is_call_verified_safe_no_unqualified(func_name),
            (_, false) => self.is_call_verified_safe(func_name),
        }
    }

    /// `Some(cleared)` when `error` is a call to a class with recorded
    /// constructor callees, `None` when no record applies and the general path
    /// decides.
    ///
    /// The recorded callees decide in both directions, overriding the class's
    /// aggregate verdict.
    ///
    /// `Unknown*` kinds are included because they are what the map emits
    /// for a class it could not bind. Resolved via an exact match on a
    /// recorded class FQN, so an unbound short name still cannot clear here.
    pub(crate) fn recorded_constructor_clears(
        &self,
        caller: ModuleName,
        error: &SafetyError,
    ) -> Option<bool> {
        match error.kind {
            ErrorKind::UnsafeFunctionCall
            | ErrorKind::UnknownFunctionCall
            | ErrorKind::UnsafeDecoratorCall
            | ErrorKind::UnknownDecoratorCall => {
                let func_name = error.metadata.as_str().trim_end_matches("()");
                let fqn = ModuleName::from_str(func_name);
                let verdict = self.recorded_constructor_verdict(&fqn)?;
                Some(self.constructor_verdict_clears(verdict, &caller, &fqn))
            }
            _ => None,
        }
    }
}

fn split_at_module_in<'n>(
    modules: &AHashSet<ModuleName>,
    func_name: &'n str,
) -> Option<(ModuleName, &'n str)> {
    enclosing_module(func_name, |m| modules.contains(m))
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
    if fs[local_name].returns_identity_decorator {
        return true;
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
    CONSTRUCTOR_METHODS
        .into_iter()
        .map(move |method| format!("{local_name}.{method}"))
}

/// Whether `local_name` names a class: it has a cached `__init__`/`__new__`.
fn is_class_like_entry(local_name: &str, fs: &AHashMap<String, FunctionSafetyInfo>) -> bool {
    constructors(local_name).any(|method| fs.contains_key(&method))
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::name::Name;

    use super::*;

    #[test]
    fn decorator_cache_is_tied_to_the_re_export_index() {
        let origin = ModuleName::from_str("origin");
        let facade = ModuleName::from_str("facade");
        let modules = AHashSet::from_iter([origin, facade]);
        let safe = || FunctionSafetyInfo::new(FunctionSafety::Safe);
        let by_module = AHashMap::from_iter([
            (
                origin,
                AHashMap::from_iter([
                    ("register".to_owned(), safe()),
                    (
                        "register.inner".to_owned(),
                        FunctionSafetyInfo::new(FunctionSafety::Unsafe),
                    ),
                ]),
            ),
            (
                facade,
                AHashMap::from_iter([("register".to_owned(), safe())]),
            ),
        ]);
        let re_exports = [CachedReExport {
            exported_module: facade,
            exported_attr: Name::new_static("register"),
            imported_module: origin,
            imported_attr: Name::new_static("register"),
        }];
        let index = ReExportIndex::new(&re_exports);
        let empty_index = ReExportIndex::new(&[]);
        let globally_safe = AHashSet::default();
        let without_exports = DecoratorVerdictMap::new(&modules, &by_module, &empty_index);
        let resolver = without_exports.resolver(&globally_safe);
        assert!(resolver.is_decorator_call_verified_safe("register"));
        assert!(
            !resolver
                .with_re_exports(&index)
                .is_decorator_call_verified_safe("register"),
            "changing the re-export index must not reuse a cached safe verdict",
        );
        let with_exports = DecoratorVerdictMap::new(&modules, &by_module, &index);
        let resolver = with_exports.resolver(&globally_safe);
        assert!(!resolver.is_decorator_call_verified_safe("register"));
        assert!(!resolver.is_decorator_call_verified_safe("register"));
        assert!(!resolver.is_decorator_call_verified_safe("facade.register"));
    }

    /// The flag and the kind travel separately through the cache, so an artifact
    /// written by another binary could pair them wrongly. The dispatch has to
    /// stay strict on its own rather than trusting the pairing.
    #[test]
    fn a_non_decorator_kind_carrying_the_flag_does_not_take_the_decorator_path() {
        let other = ModuleName::from_str("unrelated");
        let mut fns: AHashMap<String, FunctionSafetyInfo> = AHashMap::default();
        fns.insert(
            "f".to_owned(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::default();
        by_module.insert(other, fns);

        let modules: AHashSet<ModuleName> = AHashSet::from_iter([other]);
        let resolver = SafetyResolver::new(&modules, &by_module);

        let mut corrupt = SafetyError::new(
            ErrorKind::UnknownFunctionCall,
            "f".to_owned(),
            ruff_text_size::TextRange::default(),
        );
        corrupt.parameterized_decorator = true;
        assert!(
            !resolver.is_error_verified_safe(&corrupt),
            "an unbound call must stay on the no-unqualified path; taking the \
             decorator scan would clear it on a coincidental name"
        );
    }

    /// An unresolved decorator target is a textual name like any other
    /// `Unknown*`, so it must not clear on a same-named safe function
    /// elsewhere. This is the plain `@deco` form; the parameterized one takes
    /// the decorator path and is covered above it in the stack.
    #[test]
    fn an_unparameterized_unknown_decorator_does_not_clear_on_a_coincidental_name() {
        let other = ModuleName::from_str("unrelated");
        let mut fns: AHashMap<String, FunctionSafetyInfo> = AHashMap::default();
        fns.insert(
            "deco".to_owned(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::default();
        by_module.insert(other, fns);

        let modules: AHashSet<ModuleName> = AHashSet::from_iter([other]);
        let resolver = SafetyResolver::new(&modules, &by_module);

        let unbound = SafetyError::new(
            ErrorKind::UnknownDecoratorCall,
            "deco".to_owned(),
            ruff_text_size::TextRange::default(),
        );
        assert!(
            !resolver.is_error_verified_safe(&unbound),
            "`deco` was never bound to a callee; a same-named safe function in \
             `unrelated` is a coincidence, not evidence"
        );

        // The qualified form names a callee, so it still clears.
        let bound = SafetyError::new(
            ErrorKind::UnknownDecoratorCall,
            "unrelated.deco".to_owned(),
            ruff_text_size::TextRange::default(),
        );
        assert!(resolver.is_error_verified_safe(&bound));
    }

    /// An `Unknown*` target is a textual name, not a proven callee, so it must
    /// not clear on a same-named safe function elsewhere.
    #[test]
    fn an_unqualified_unknown_decorator_call_does_not_clear_on_a_coincidental_name() {
        let other = ModuleName::from_str("unrelated");
        let mut fns: AHashMap<String, FunctionSafetyInfo> = AHashMap::default();
        fns.insert(
            "deco".to_owned(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        let mut by_module: AHashMap<ModuleName, AHashMap<String, FunctionSafetyInfo>> =
            AHashMap::default();
        by_module.insert(other, fns);

        let modules: AHashSet<ModuleName> = AHashSet::from_iter([other]);
        let resolver = SafetyResolver::new(&modules, &by_module);

        let mut unbound = SafetyError::new(
            ErrorKind::UnknownDecoratorCall,
            "deco".to_owned(),
            ruff_text_size::TextRange::default(),
        );
        unbound.parameterized_decorator = true;
        assert!(
            !resolver.is_error_verified_safe(&unbound),
            "`deco` was never bound to a callee; a same-named safe function in \
             `unrelated` is a coincidence, not evidence"
        );

        // The qualified form names a callee, so it still clears.
        let mut bound = SafetyError::new(
            ErrorKind::UnknownDecoratorCall,
            "unrelated.deco".to_owned(),
            ruff_text_size::TextRange::default(),
        );
        bound.parameterized_decorator = true;
        assert!(resolver.is_error_verified_safe(&bound));
    }
}
