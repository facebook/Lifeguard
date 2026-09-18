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

use std::collections::HashMap;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;

use crate::cache::CONSTRUCTOR_METHODS;
use crate::cache::CachedError;
use crate::cache::ConstructorCallees;
use crate::errors::ErrorKind;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::FixedState;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::mro::c3_linearize;
use crate::traits::ModuleNameExt;

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
pub(crate) struct SafetyResolver<'a> {
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
    /// Map-phase-resolved constructor callees, keyed by class FQN. When present
    /// for a class, these replace re-deriving its constructor method set.
    constructor_callees: Option<&'a HashMap<ModuleName, ConstructorCallees>>,
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
            decorator_scan_cache: None,
            class_bases: None,
            constructor_callees: None,
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
            decorator_scan_cache: None,
            class_bases: None,
            constructor_callees: None,
        }
    }

    pub(crate) fn with_decorator_cache(
        mut self,
        cache: &'a DashMap<String, bool, FixedState>,
    ) -> Self {
        self.decorator_scan_cache = Some(cache);
        self
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
    pub(crate) fn split_at_module<'n>(&self, func_name: &'n str) -> Option<(ModuleName, &'n str)> {
        let fqn = ModuleName::from_str(func_name);
        fqn.iter_parents()
            .find(|(parent, _)| self.modules.contains(parent))
            .map(|(parent, dot_pos)| (parent, &func_name[dot_pos + 1..]))
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

    /// Whether `module` has an own entry for `local` verified `Safe`.
    fn own_call_safe(&self, module: &ModuleName, local: &str) -> bool {
        self.own_verdict(module, local).is_some_and(|v| v.is_safe())
    }

    /// Whether a plain function call is found and verified `Safe`.
    pub(crate) fn is_call_verified_safe(&self, func_name: &str) -> bool {
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
    pub(crate) fn is_decorator_call_verified_safe(&self, func_name: &str) -> bool {
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

    /// Dispatch a cached error to the right verified-safe check by kind. The
    /// callee `metadata` may render with trailing `()` suffixes; strip them here.
    ///
    /// `UnknownFunctionCall` / `UnknownMethodCall` couldn't bind the call target,
    /// so they additionally skip the unqualified fallback: an unbound short name
    /// must not clear on a same-named safe function elsewhere.
    pub(crate) fn is_error_verified_safe(&self, error: &CachedError) -> bool {
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

    /// Whether `error` in `caller` may be dropped.
    ///
    /// A call to a class the map phase recorded constructor callees for is
    /// decided by those callees alone. Such a call also does not consult `kinds`;
    /// its answer follows from static verdicts, with no promotion evidence needed.
    ///
    /// Every other error clears only when `kinds` admits it and the general
    /// verdict verifies it.
    pub(crate) fn clears_error(
        &self,
        caller: ModuleName,
        error: &CachedError,
        kinds: impl Fn(ErrorKind) -> bool,
    ) -> bool {
        if let Some(cleared) = self.recorded_constructor_clears(caller, error) {
            return cleared;
        }
        kinds(error.kind) && self.is_error_verified_safe(error)
    }

    /// `Some(cleared)` when `error` is a call to a class with recorded
    /// constructor callees, `None` when no record applies and the general path
    /// decides.
    ///
    /// `Unknown*` kinds are included because they are what the map emits
    /// for a class it could not bind -- the cross-library instantiation the
    /// recorded callees exist to answer. The lookup is an exact match on a
    /// recorded class FQN, so an unbound short name still cannot clear here.
    pub(crate) fn recorded_constructor_clears(
        &self,
        caller: ModuleName,
        error: &CachedError,
    ) -> Option<bool> {
        match error.kind {
            ErrorKind::UnsafeFunctionCall
            | ErrorKind::UnknownFunctionCall
            | ErrorKind::UnsafeDecoratorCall
            | ErrorKind::UnknownDecoratorCall => {
                let func_name = error.metadata.trim_end_matches("()");
                let fqn = ModuleName::from_str(func_name);
                let verdict = self.recorded_constructor_verdict(&fqn)?;
                Some(self.constructor_verdict_clears(verdict, &caller, &fqn))
            }
            _ => None,
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
    CONSTRUCTOR_METHODS
        .into_iter()
        .map(move |method| format!("{local_name}.{method}"))
}

/// Whether `local_name` names a class: it has a cached `__init__`/`__new__`.
fn is_class_like_entry(local_name: &str, fs: &AHashMap<String, FunctionSafetyInfo>) -> bool {
    constructors(local_name).any(|method| fs.contains_key(&method))
}
