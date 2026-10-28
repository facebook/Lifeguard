/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Call-site safety checks and recursive effect traversal.

use anyhow::Result;
use anyhow::anyhow;
use pyrefly_python::module_name::ModuleName;

use crate::effects::Effect;
use crate::effects::EffectKind;
use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::errors::is_parameterized_decorator_effect;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::manual_override;
use crate::module_safety::FunctionSafety;
use crate::project::dispatch::iter_callees;
use crate::project::info::ProjectInfo;
use crate::project::state::GlobalAnalysisState;

#[derive(Debug)]
pub(super) struct Call<'a> {
    pub(super) caller_module: &'a ModuleName,
    pub(super) effect: &'a Effect,
    pub(super) func: ModuleName,
    pub(super) stack: CallStack,
    // Distinct from check_call_safety's publish_safety_error: that flag is true only at the
    // top-level call (false for recursive calls to avoid double-counting), while is_module_scope
    // stays true through the entire chain so check_call_params catches transitive param mutation.
    pub(super) is_module_scope: bool,
}

impl<'a> Call<'a> {
    fn clone_with_name(&self, func: ModuleName) -> Self {
        Self {
            caller_module: self.caller_module,
            effect: self.effect,
            func,
            stack: self.stack.clone(),
            is_module_scope: self.is_module_scope,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(super) struct CallStack {
    entries: Vec<ModuleName>,
    seen: AHashSet<ModuleName>,
}

impl CallStack {
    pub(super) fn new(initial: ModuleName) -> Self {
        let mut seen = AHashSet::with_capacity(16);
        seen.insert(initial);
        Self {
            entries: vec![initial],
            seen,
        }
    }

    fn contains(&self, name: &ModuleName) -> bool {
        self.seen.contains(name)
    }

    fn push(&mut self, name: ModuleName) {
        self.seen.insert(name);
        self.entries.push(name);
    }

    fn pop(&mut self) {
        if let Some(name) = self.entries.pop() {
            // Make sure we have filtered out recursive stacks
            debug_assert!(!self.entries.contains(&name));
            self.seen.remove(&name);
        }
    }
}

impl ProjectInfo {
    fn can_resolve_call(&self, call: &Call, state: &GlobalAnalysisState) -> bool {
        self.contains_callable(&call.func) || state.function_safety.contains_key(&call.func)
    }

    /// Mark `func` unsafe because one of its callees failed its safety check.
    /// A resolvable callee is intrinsically unsafe, so `func` is hard `Unsafe`.
    /// An unresolvable callee may just be a missing cross-library dep, so `func`
    /// gets the recoverable `UnsafeMissingDep` (which cross-library resolution
    /// can later upgrade) — but never downgrade an already hard-`Unsafe` verdict.
    fn mark_caller_unsafe_for_failed_callee(
        &self,
        func: &ModuleName,
        callee: &Call,
        state: &GlobalAnalysisState,
    ) {
        // A callee that is only recoverably unsafe (an unresolved/missing dep,
        // including a deferred cross-library mutation) makes the caller
        // recoverably unsafe too, even when the callee is resolvable in this
        // library. Marking the caller hard-`Unsafe` here would strand it if the
        // callee is later promoted/resolved at reduce time.
        //
        // This precompute verdict is provisional: reads of a callee's snapshot may
        // race a concurrent thread's intermediate `UnsafeMissingDep`. That cannot
        // cause a false-safe because reduce re-checks final verdicts — the recorded
        // callee, if finally unsafe, blocks promotion (`is_call_verified_safe`) and
        // blocks resolution (`callee_resolves_unsafe`).
        let callee_recoverable = state.function_safety.get(&callee.func).is_some_and(|info| {
            info.verdict.has(FunctionSafety::UnsafeMissingDep)
                && !info.verdict.has(FunctionSafety::Unsafe)
        });
        if callee_recoverable {
            if !state.is_unsafe(func) {
                state.mark_unsafe_missing_dep(func, &callee.func);
            }
        } else if self.can_resolve_call(callee, state) {
            state.mark_unsafe(func);
        } else if !state.is_unsafe(func) {
            state.mark_unsafe_missing_dep(func, &callee.func);
        }
    }

    fn check_unknown_call(&self, call: &Call) -> Result<SafetyError> {
        match call.effect.kind {
            EffectKind::ImportedFunctionCall | EffectKind::FunctionCall => {
                // This is a call with a name, but we don't have a resolved function or class name
                // corresponding to it. Mark it unsafe.
                let err = SafetyError::new_from_effect(ErrorKind::UnknownFunctionCall, call.effect);
                Ok(err)
            }
            EffectKind::MethodCall
            | EffectKind::UnboundMethodCall
            | EffectKind::ParamMethodCall => {
                let err = SafetyError::new_from_effect(ErrorKind::UnknownMethodCall, call.effect);
                Ok(err)
            }
            EffectKind::ImportedDecoratorCall | EffectKind::DecoratorCall => {
                let err =
                    SafetyError::new_from_effect(ErrorKind::UnknownDecoratorCall, call.effect);
                Ok(err)
            }
            _ => {
                // We should not reach this function with any other type of call
                Err(anyhow!("Unexpected call type {:?}", call))
            }
        }
    }

    pub(super) fn check_call_safety(
        &self,
        call: &mut Call,
        state: &GlobalAnalysisState,
        publish_safety_error: bool,
    ) -> Result<bool> {
        let written = call.func;
        if call.effect.kind == EffectKind::ClassCreation {
            // No ancestor defines `__init_subclass__` means `object`'s no-op runs
            let callees = self
                .metaclass_creation_callees(&call.func)
                .into_iter()
                .chain(self.resolve_dispatch_above(&call.func));
            let mut safe = true;
            for callee in callees {
                let mut callee_call = call.clone_with_name(callee);
                safe &=
                    self.check_resolved_call(&mut callee_call, state, publish_safety_error, true)?;
            }
            return Ok(safe);
        }
        match call.effect.kind {
            EffectKind::SuperMethodCall => match self.resolve_dispatch_above(&call.func) {
                Some(resolved) => call.func = resolved,
                // No ancestor defines the method, so there is no body to run.
                None => return Ok(true),
            },
            _ => {
                if let Some(resolved) = self.resolve_callable(&call.func) {
                    call.func = resolved;
                }
            }
        }
        let name_callee = self.re_exports.contains_key(&written);
        self.check_resolved_call(call, state, publish_safety_error, name_callee)
    }

    /// Check a call whose callee is already resolved. `name_callee` records the
    /// error under the callee rather than the written name, because the reduce
    /// step re-verifies errors by name.
    fn check_resolved_call(
        &self,
        call: &mut Call,
        state: &GlobalAnalysisState,
        publish_safety_error: bool,
        name_callee: bool,
    ) -> Result<bool> {
        if !self.can_resolve_call(call, state) {
            if publish_safety_error {
                let err = self.check_unknown_call(call)?;
                state.add_error_to_module(call.caller_module, err);
            }
            return Ok(false);
        }

        if !self.check_call(call, state)? {
            if publish_safety_error {
                let mut err = SafetyError::from_unsafe_call(call.effect)?;
                if name_callee {
                    err.metadata = call.func.as_str().into();
                }
                state.add_error_to_module(call.caller_module, err);
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn check_call(&self, call: &mut Call, state: &GlobalAnalysisState) -> Result<bool> {
        // the call site saw a re-export name, so the safelist missed it there
        if manual_override::declared_safe(&call.func) {
            return Ok(true);
        }
        let mut ret = if self.classes.contains(&call.func) {
            // This is a class constructor
            self.check_constructor_call(call, state)?
        } else {
            self.check_call_body(call, state)?
        };
        // A parameterized decorator @deco(args) also calls the function deco returns,
        // so its nested functions' effects run at import time too. This is checked at
        // the call site, not folded into check_call_body's cached verdict: that verdict
        // must stay independent of the call kind that first reached the function.
        if self.is_parameterized_decorator_call(call) {
            ret &= self.check_decorator_nested_functions(call, state)?;
        }
        Ok(ret)
    }

    /// Check the nested functions of a parameterized decorator call without
    /// mutating the cached safety verdict of the decorator function itself.
    fn check_decorator_nested_functions(
        &self,
        call: &mut Call,
        state: &GlobalAnalysisState,
    ) -> Result<bool> {
        let Some(children) = self.nested_functions.get(&call.func) else {
            return Ok(true);
        };
        let mut ret = true;
        // Only the decorator factory's immediate children run at decoration time.
        for child in children {
            // Use a FunctionCall effect for the child so we don't re-enter the
            // parameterized decorator path (which would cause infinite recursion).
            let child_effect = Effect::new(EffectKind::FunctionCall, *child, call.effect.range);
            let mut child_call = Call {
                caller_module: call.caller_module,
                effect: &child_effect,
                func: *child,
                stack: std::mem::take(&mut call.stack),
                is_module_scope: call.is_module_scope,
            };
            let child_ret = self.check_call_body(&mut child_call, state)?;
            call.stack = child_call.stack;
            ret &= child_ret;
        }
        Ok(ret)
    }

    pub(super) fn check_constructor_call(
        &self,
        call: &Call,
        state: &GlobalAnalysisState,
    ) -> Result<bool> {
        let mut ret = true;
        for method in self.constructor_methods(call.func) {
            let mut method_call = call.clone_with_name(method);
            ret &= self.check_call_body(&mut method_call, state)?;
        }
        Ok(ret)
    }

    pub(super) fn check_call_body(
        &self,
        call: &mut Call,
        state: &GlobalAnalysisState,
    ) -> Result<bool> {
        let func = call.func;
        // We need to mark errors in the module containing the called function, not the caller's
        // module.
        let Some(call_module) = self.functions.get(&func) else {
            // This function is not in the function table so we cannot find effects for it.
            return Ok(true);
        };
        let is_cross_module_call = *call.caller_module != *call_module;

        let effs = self.effect_table.get(&func);

        if call.is_module_scope {
            self.check_call_params(call, state);
        }

        if let Some(verdict) = state.function_safety.get(&func).map(|info| info.verdict) {
            let ret = if verdict.has(FunctionSafety::Unsafe | FunctionSafety::UnsafeMissingDep) {
                false
            } else if verdict.has(FunctionSafety::UnsafeIfImported) {
                !is_cross_module_call
            } else {
                // No concerns: safe.
                true
            };
            return Ok(ret);
        }

        let mut ret = true;

        if let Some(effs) = effs {
            for eff in effs {
                if SafetyError::from_effect(eff).is_some() {
                    // We have an effect that translates unconditionally to an error, so mark the
                    // function unsafe
                    state.mark_unsafe(&func);
                    ret = false;
                } else if eff.kind.is_runnable() {
                    // If we pass an imported variable to a function that mutates it
                    // (directly or transitively), mark the current function as unsafe.
                    if self.call_mutates_imported_arg(eff) {
                        state.mark_unsafe(&func);
                        ret = false;
                    } else if !call.is_module_scope {
                        // Precompute pass only: a call passing an imported object to a callee
                        // unresolved in this library may mutate it, but we can't tell here.
                        // This propagates a recoverable error that the reduce phase resolves.
                        if self.defers_cross_library_mutation(eff) {
                            for (callee, _) in iter_callees(eff, &self.classes) {
                                if !self.functions.contains_key(&callee)
                                    && !self.mutated_params.contains_key(&callee)
                                {
                                    state.mark_unsafe_missing_dep(&func, &callee);
                                    ret = false;
                                }
                            }
                        }
                    }
                    if call.stack.contains(&eff.name) {
                        // We have a recursive function call; mark it unsafe
                        state.mark_unsafe(&func);
                        ret = false;
                    } else {
                        call.stack.push(eff.name);
                        let child_caller_module = if !call.is_module_scope {
                            // When precomputing function safety, the child module is the immediate
                            // caller, so that we can inherit the "unsafe if imported" status
                            // independent of the original entry point.
                            call_module
                        } else {
                            // When checking module safety, thread the entry module through all
                            // calls, so that we can resolve the "unsafe if imported" effects from
                            // the previous pass with the correct value of "if imported".
                            call.caller_module
                        };
                        let mut child_call = Call {
                            caller_module: child_caller_module,
                            effect: eff,
                            func: eff.name,
                            stack: std::mem::take(&mut call.stack),
                            is_module_scope: call.is_module_scope,
                        };
                        if !self.check_call_safety(&mut child_call, state, false)? {
                            self.mark_caller_unsafe_for_failed_callee(&func, &child_call, state);
                            ret = false;
                        } else if state
                            .function_safety
                            .get(&child_call.func)
                            .is_some_and(|i| i.verdict.has(FunctionSafety::UnsafeIfImported))
                            && self.functions.get(&child_call.func) == Some(call_module)
                        {
                            // propagate `UnsafeIfImported` to the caller
                            state.mark_unsafe_if_imported(&func);
                            if is_cross_module_call {
                                ret = false;
                            }
                        }
                        call.stack = child_call.stack;
                        call.stack.pop();
                    }
                } else {
                    match eff.kind {
                        EffectKind::ImportedVarMutation => {
                            // Mark this call unsafe but don't add an error here: it
                            // is not happening at global scope. If this callable is
                            // called, the error is added there instead.
                            state.mark_unsafe(&func);
                            ret = false;
                        }
                        EffectKind::GlobalVarAssign | EffectKind::GlobalVarMutation => {
                            // This function is attempting to mutate a global variable, and so is only
                            // safe if being called from its own module.
                            let no_missing_dep_or_unsafe =
                                state.function_safety.get(&func).is_none_or(|info| {
                                    !info.verdict.has(
                                        FunctionSafety::UnsafeMissingDep | FunctionSafety::Unsafe,
                                    )
                                });
                            state.mark_unsafe_if_imported(&func);
                            if no_missing_dep_or_unsafe && is_cross_module_call {
                                ret = false;
                            }
                        }
                        _ => (),
                    }
                }
            }
        }

        // We haven't detected any unsafe behaviour
        if state.function_safety.get(&func).is_none() {
            state.mark_safe(&func);
        }
        Ok(ret)
    }

    fn is_parameterized_decorator_call(&self, call: &Call) -> bool {
        is_parameterized_decorator_effect(call.effect)
    }
}
