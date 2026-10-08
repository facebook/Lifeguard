/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprAttribute;
use ruff_python_ast::name::Name;
use ruff_text_size::Ranged;

use crate::analyzer::AnalyzedModule;
use crate::effects::Effect;
use crate::effects::EffectKind;
use crate::pyrefly::definitions::Definition;
use crate::traits::ExprExt;
use crate::traits::ModuleNameExt;

#[derive(Debug)]
pub struct Builtins<'a> {
    builtins: &'a AnalyzedModule,
}

// Builtins are bare functions in python; we namespace them under a fake `builtins` module. Add
// some convenience methods for working with this scheme.
impl<'a> Builtins<'a> {
    pub fn new(builtins: &'a AnalyzedModule) -> Self {
        Self { builtins }
    }

    pub fn get(&self, name: &Name) -> Option<&'a Definition> {
        self.builtins.definitions.get(&ModuleName::builtins(), name)
    }

    pub fn contains(&self, name: &Name) -> bool {
        self.get(name).is_some()
    }

    pub fn is_class(&self, name: &Name) -> bool {
        let key = ModuleName::builtins().append(name);
        self.builtins.classes.contains(&key)
    }

    fn effects_for(&self, qname: &ModuleName) -> Option<&'a [Effect]> {
        self.builtins
            .module_effects
            .effects
            .get(qname)
            .map(Vec::as_slice)
    }

    // Check that `name` is in the effects table, and calls `pred` over the set of effects for
    // `name` if so. If `name` is a class, checks for any of `name`, `name.__init__` and
    // `name.__new__`.
    // TODO: Perhaps we should check for name() strictly if name is *not* a class, and the new and
    // init methods if it is.
    fn check_call_effects<F>(&self, name: &Name, pred: F) -> bool
    where
        F: Fn(&[Effect]) -> bool,
    {
        let qname = ModuleName::builtins().append(name);
        let check = |n: &ModuleName| self.effects_for(n).is_some_and(&pred);
        if check(&qname) {
            true
        } else if self.is_class(name) {
            check(&qname.append_str("__new__")) || check(&qname.append_str("__init__"))
        } else {
            false
        }
    }

    fn is_prohibited_call(&self, name: &Name) -> bool {
        self.check_call_effects(name, unsafe_stub_effects)
    }

    /// The `builtins` entry a call target names.
    fn qualified_name(&self, func: &Expr) -> Option<ModuleName> {
        // Bare name, e.g. `len`
        if let Some(name) = func.as_var_name() {
            return Some(ModuleName::builtins().append(&name));
        }
        // Method call, e.g. `list.append`
        let Expr::Attribute(ExprAttribute { value, attr, .. }) = func else {
            return None;
        };
        let base = value.as_var_name()?;
        self.is_class(&base)
            .then(|| ModuleName::builtins().append(&base).append(&attr.id))
    }

    pub fn call_effect(&self, func: &Expr) -> Option<Effect> {
        let qname = self.qualified_name(func)?;
        let prohibited = match func.as_var_name() {
            // A bare class name is a constructor call, so its `__new__` and
            // `__init__` count too.
            Some(name) => self.is_prohibited_call(&name),
            None => self.effects_for(&qname).is_some_and(unsafe_stub_effects),
        };
        // Safe builtin or unknown call (treated as safe): emit no effect.
        prohibited.then(|| Effect::new(EffectKind::ProhibitedFunctionCall, qname, func.range()))
    }

    /// Returns true if the given function name is a known builtin (safe or unsafe).
    /// Callers must consult [`Self::call_effect`] first: a method the stub
    /// declares is "known" whether or not it mutates.
    pub fn is_known_builtin(&self, func: &Expr) -> bool {
        match func.as_var_name() {
            Some(name) => self.contains(&name) || self.is_prohibited_call(&name),
            None => self.declares_method(func),
        }
    }

    /// Whether the stub declares this `<builtin class>.<method>`, either on the
    /// class or on `object`, which every builtin class inherits from.
    fn declares_method(&self, func: &Expr) -> bool {
        let Some(qname) = self.qualified_name(func) else {
            return false;
        };
        let Some((class, method)) = qname.split_attr() else {
            return false;
        };
        self.builtins.definitions.get(&class, &method).is_some()
            || self
                .builtins
                .definitions
                .get(&ModuleName::builtins().append_str("object"), &method)
                .is_some()
    }
}

fn unsafe_stub_effects(effs: &[Effect]) -> bool {
    effs.iter().any(|e| e.kind.is_unsafe_stub_effect())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::LazyLock;

    use super::*;
    use crate::hasher::AHashSet;
    use crate::stubs::Stubs;

    // We no longer use the static list of unsafe builtins; keep it as a cross-check for testing.
    static UNSAFE_BUILTINS: LazyLock<HashSet<&str>> =
        LazyLock::new(|| HashSet::from(["breakpoint", "eval", "input", "open", "__import__"]));

    // These are potentially unsafe because they call dunder methods on their args
    static DUNDER_BUILTINS: LazyLock<HashSet<&str>> = LazyLock::new(|| {
        HashSet::from([
            "abs",
            "bin",
            "bool",
            "bytearray",
            "bytes",
            "complex",
            "delattr",
            "dict",
            "float",
            "getattr",
            "hex",
            "int",
            "isinstance",
            "issubclass",
            "iter",
            "len",
            "list",
            "map",
            "max",
            "min",
            "next",
            "oct",
            "pow",
            // "print",  // calls __str__, but og analyzer does not consider it unsafe
            "range",
            "repr",
            "reversed",
            "round",
            "set",
            "setattr",
            "str",
            "sum",
            "tuple",
            "zip",
        ])
    });

    #[test]
    fn test_unsafe_builtins() {
        // Check that everything we mark in UNSAFE_BUILTINS has an effect added in builtins.pyi
        let stubs = Stubs::new();
        let builtins = stubs.builtins();
        for b in &*UNSAFE_BUILTINS {
            let name = Name::new(b);
            assert!(builtins.check_call_effects(&name, |_| true))
        }
    }

    #[test]
    fn test_dunder_builtins() {
        let stubs = Stubs::new();
        let builtins = stubs.builtins();
        for b in &*DUNDER_BUILTINS {
            let name = Name::new(b);
            assert!(builtins.check_call_effects(&name, |effs| {
                effs.iter().any(|e| matches!(e.kind, EffectKind::Dunder))
            }));
            assert!(!builtins.is_prohibited_call(&name));
        }
    }

    #[test]
    fn test_mutation_annotation() {
        let stubs = Stubs::new();
        let builtins = stubs.builtins();
        let effects = &builtins.builtins.module_effects.effects;
        let effs = effects
            .get(&ModuleName::from_str("builtins.list.append"))
            .unwrap();
        let x = effs.iter().find(|e| e.kind == EffectKind::Mutation);
        assert!(x.is_some());
    }

    #[test]
    fn test_overloads() {
        // Check that we only need the full set of effects in one of a function's overloads
        // Here, we have str.__new__ which has the first overload annotated with dunder("__str__")
        // and dunder("__repr__"), and the second one just with dunder("__str__")
        let stubs = Stubs::new();
        let builtins = stubs.builtins();
        let effects = &builtins.builtins.module_effects.effects;
        let effs = effects
            .get(&ModuleName::from_str("builtins.str.__new__"))
            .unwrap();
        assert_eq!(effs.len(), 2);
        assert!(effs.iter().all(|e| e.kind == EffectKind::Dunder));
        let methods: AHashSet<&str> = effs.iter().map(|e| e.name.as_str()).collect();
        assert!(methods.contains(&"__str__"));
        assert!(methods.contains(&"__repr__"));
    }
}
