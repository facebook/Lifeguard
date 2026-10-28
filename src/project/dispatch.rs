/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Syntactic call candidates and resolved callable, MRO, and constructor dispatch.

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::cache::CONSTRUCTOR_METHODS;
use crate::cache::ConstructorCallees;
use crate::cache::constructor_mask_bits;
use crate::class::ClassTable;
use crate::class::FieldKind;
use crate::effects::Effect;
use crate::effects::EffectKind;
use crate::mro::c3_linearize;
use crate::project::info::ProjectInfo;
use crate::traits::ModuleNameExt;

/// What `type.__call__` runs on the metaclass when a `class` statement creates a class.
const METACLASS_CREATION_METHODS: [&str; 2] = ["__new__", "__init__"];

/// The functions a constructor call to `cls_name` may dispatch to, in the
/// order `check_constructor_call` checks them: `CONSTRUCTOR_METHODS` on the
/// metaclass (if the class has one), then the same set on the class itself.
/// Mirrors the candidate set `ConstructorCallees` masks, so the direct and
/// cache-backed paths dispatch to the same callees.
/// TODO: Look up `__init__` in the MRO.
fn constructor_method_names(
    cls_name: ModuleName,
    classes: &ClassTable,
) -> impl Iterator<Item = ModuleName> {
    let metaclass = classes.lookup(&cls_name).and_then(|cls| cls.metaclass);
    metaclass
        .into_iter()
        .flat_map(|mcls| CONSTRUCTOR_METHODS.map(|method| mcls.append_str(method)))
        .chain(CONSTRUCTOR_METHODS.map(|method| cls_name.append_str(method)))
}

/// Number of implicit leading parameters (the receiver) for a method call.
/// Returns `None` when the method's kind is unknown (e.g. class could not be resolved).
fn method_receiver_offset(method: &ModuleName, classes: &ClassTable, bound: bool) -> Option<usize> {
    let (class, leaf) = method.split_attr()?;
    Some(match classes.lookup(&class)?.get_field(&leaf)?.kind {
        FieldKind::StaticMethod => 0,
        FieldKind::ClassMethod => 1,
        FieldKind::InstanceMethod => usize::from(bound),
        _ => return None,
    })
}

/// Yield each `(callee, arg_offset)` a call binds to: the function(s) whose
/// parameters the call's explicit arguments map to, paired with the number of
/// implicit leading parameters (the receiver) to skip.
pub(super) fn iter_callees<'a>(
    eff: &'a Effect,
    classes: &'a ClassTable,
) -> impl Iterator<Item = (ModuleName, usize)> + 'a {
    let is_method = matches!(
        eff.kind,
        EffectKind::MethodCall | EffectKind::UnboundMethodCall | EffectKind::SuperMethodCall
    );
    let is_constructor = !is_method && classes.contains(&eff.name);

    // Constructor calls dispatch to each constructor method (offset 1); empty
    // for non-constructor calls.
    let constructors = is_constructor
        .then(|| constructor_method_names(eff.name, classes))
        .into_iter()
        .flatten()
        .map(|method| (method, 1));

    // Method / plain function calls resolve to a single callee (`eff.name`) at
    // one or two offsets; empty for constructor calls.
    let (offset, extra_offset) = match eff.kind {
        _ if is_constructor => (None, None),
        // A `super()` call is bound, and its recorded name is the caller's own
        // method, whose signature an override mirrors.
        EffectKind::MethodCall | EffectKind::SuperMethodCall => {
            match method_receiver_offset(&eff.name, classes, true) {
                Some(offset) => (Some(offset), None),
                // Unknown kind (e.g. a builtin / third-party class): assume an
                // implicit receiver, as bound calls usually have one.
                None => (Some(1), None),
            }
        }
        EffectKind::UnboundMethodCall => match method_receiver_offset(&eff.name, classes, false) {
            Some(offset) => (Some(offset), None),
            // Unknown kind: the receiver may be explicit (0) or implicit (1), so
            // consider both to avoid missing a mutated parameter.
            None => (Some(0), Some(1)),
        },
        _ => (Some(0), None),
    };
    let name = eff.name;
    let single = offset
        .into_iter()
        .chain(extra_offset)
        .map(move |offset| (name, offset));

    constructors.chain(single)
}

impl ProjectInfo {
    pub(super) fn contains_callable(&self, name: &ModuleName) -> bool {
        if self.functions.contains_key(name) || self.classes.contains(name) {
            return true;
        }
        let call_name = self.methods.get(name).copied().unwrap_or(*name);
        if self.functions.contains_key(&call_name) || self.classes.contains(&call_name) {
            true
        } else {
            self.re_exports.contains_key(&call_name)
        }
    }

    pub(super) fn resolve_callable(&self, name: &ModuleName) -> Option<ModuleName> {
        if self.functions.contains_key(name) || self.classes.contains(name) {
            return Some(*name);
        }
        let call_name = self.methods.get(name).copied().unwrap_or(*name);
        if self.functions.contains_key(&call_name) || self.classes.contains(&call_name) {
            return Some(call_name);
        }
        if let Some(target) = self.re_exports.get(&call_name) {
            // A re-exported name has no effects of its own; its definition does.
            return Some(*target);
        }
        let (class, method) = name.split_attr()?;
        if !self.classes.contains(&class) {
            return None;
        }
        self.resolve_inherited(&class, method.as_str())
    }

    /// The nearest ancestor's definition of `method`, skipping `class`'s own.
    fn resolve_inherited(&self, class: &ModuleName, method: &str) -> Option<ModuleName> {
        // Classes without bases inherit nothing, and c3_linearize allocates a
        // memo per call, so skipping them keeps this off the hot path.
        if !self.class_bases.contains_key(class) {
            return None;
        }
        c3_linearize(&self.class_bases, class)
            .into_iter()
            .skip(1)
            .map(|base| base.append_str(method))
            .find(|candidate| self.contains_callable(candidate))
    }

    /// The definition an implicit dispatch above `name`'s class runs: `name` is
    /// a method on the class the statement is written in, and lookup starts
    /// above it. Zero-argument `super()` and class creation both dispatch this
    /// way — the latter because `type.__new__` calls
    /// `super().__init_subclass__`.
    pub(super) fn resolve_dispatch_above(&self, name: &ModuleName) -> Option<ModuleName> {
        let (class, method) = name.split_attr()?;
        self.resolve_inherited(&class, method.as_str())
    }

    /// The metaclass a `class` statement calls: the first `metaclass=` declared
    /// along its MRO, which starts with the class itself.
    fn creation_metaclass(&self, class: &ModuleName) -> Option<ModuleName> {
        // skip linearizing a class without bases, as `resolve_inherited` does
        if !self.class_bases.contains_key(class) {
            return self.classes.lookup(class)?.metaclass;
        }
        c3_linearize(&self.class_bases, class)
            .iter()
            .find_map(|cls| self.classes.lookup(cls)?.metaclass)
    }

    /// The metaclass methods a `class` statement runs. `name` is the
    /// `<class>.__init_subclass__` a `ClassCreation` effect is recorded under.
    pub(super) fn metaclass_creation_callees(&self, name: &ModuleName) -> Vec<ModuleName> {
        let Some((class, _)) = name.split_attr() else {
            return Vec::new();
        };
        self.creation_metaclass(&class)
            .into_iter()
            .flat_map(|metaclass| {
                METACLASS_CREATION_METHODS
                    .iter()
                    .filter_map(move |method| self.resolve_callable(&metaclass.append_str(method)))
            })
            .collect()
    }

    /// Every class paired with the functions its constructor call dispatches to,
    /// keeping only those that resolve to something this analysis can see. An
    /// entry with no resolvable callee is dropped: reduce must fall back to its
    /// own lookup there rather than read the empty list as "nothing runs".
    pub(super) fn resolved_constructor_callees(&self) -> Vec<(ModuleName, ConstructorCallees)> {
        self.classes
            .par_keys()
            .filter_map(|cls_name| {
                let recorded = self.resolve_constructor_callees(*cls_name);
                (!recorded.is_empty()).then_some((*cls_name, recorded))
            })
            .collect()
    }

    /// Which of `cls_name`'s candidate constructor callees resolve to something
    /// this analysis can see. The metaclass is recorded only when one of its
    /// methods resolved, so an entry never carries a name none of its bits use.
    fn resolve_constructor_callees(&self, cls_name: ModuleName) -> ConstructorCallees {
        let metaclass = self.classes.lookup(&cls_name).and_then(|cls| cls.metaclass);
        let metaclass_mask = metaclass.map_or(0, |mcls| {
            constructor_mask_bits(mcls, 0, &CONSTRUCTOR_METHODS, |m| self.contains_callable(m))
        });
        let own_mask = constructor_mask_bits(
            cls_name,
            CONSTRUCTOR_METHODS.len(),
            &CONSTRUCTOR_METHODS,
            |m| self.contains_callable(m),
        );
        ConstructorCallees {
            metaclass: (metaclass_mask != 0).then_some(metaclass).flatten(),
            mask: metaclass_mask | own_mask,
            extra: self.inherited_constructor_callees(cls_name),
        }
    }

    /// The constructor callees the mask cannot name: the ones a class inherits,
    /// whose owner is neither the class nor its metaclass. `resolve_callable`
    /// walks the MRO, so this names the ancestor that actually defines the
    /// method rather than the first base that mentions it.
    fn inherited_constructor_callees(&self, cls_name: ModuleName) -> Vec<ModuleName> {
        CONSTRUCTOR_METHODS
            .iter()
            .filter_map(|method| {
                let own = cls_name.append_str(method);
                let resolved = self.resolve_callable(&own)?;
                (resolved != own).then_some(resolved)
            })
            .collect()
    }

    pub(super) fn constructor_methods(
        &self,
        cls_name: ModuleName,
    ) -> impl Iterator<Item = ModuleName> {
        constructor_method_names(cls_name, &self.classes)
    }
    /// The callee a call effect dispatches to, or `None` when the project view
    /// cannot name one. A `super()` call or a class statement resolves through the
    /// MRO above the name it was recorded under, which is never the callee.
    pub(super) fn call_graph_target(
        &self,
        eff: &Effect,
        is_node: impl Fn(&ModuleName) -> bool,
    ) -> Option<ModuleName> {
        match eff.kind {
            EffectKind::SuperMethodCall | EffectKind::ClassCreation => {
                self.resolve_dispatch_above(&eff.name)
            }
            _ if is_node(&eff.name) => Some(eff.name),
            _ => self.resolve_callable(&eff.name),
        }
    }
}
