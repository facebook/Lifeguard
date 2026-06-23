/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::Expr;
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

    // Check that `name` is in the effects table, and calls `pred` over the set of effects for
    // `name` if so. If `name` is a class, checks for any of `name`, `name.__init__` and
    // `name.__new__`.
    // TODO: Perhaps we should check for name() strictly if name is *not* a class, and the new and
    // init methods if it is.
    fn check_call_effects<F>(&self, name: &Name, pred: F) -> bool
    where
        F: Fn(&Vec<Effect>) -> bool,
    {
        let effects = &self.builtins.module_effects.effects;
        let qname = ModuleName::builtins().append(name);
        let check = |n: &ModuleName| effects.get(n).is_some_and(&pred);
        if check(&qname) {
            true
        } else if self.is_class(name) {
            let k_new = qname.append_str("__new__");
            let k_init = qname.append_str("__init__");
            check(&k_new) || check(&k_init)
        } else {
            false
        }
    }

    fn is_prohibited_call(&self, name: &Name) -> bool {
        self.check_call_effects(name, |effs| {
            effs.iter().any(|e| e.kind.is_unsafe_stub_effect())
        })
    }

    pub fn call_effect(&self, func: &Expr) -> Option<Effect> {
        // A builtin function should be an undotted name
        let fname = func.as_var_name()?;
        let qname = ModuleName::builtins().append(&fname);
        if self.is_prohibited_call(&fname) {
            let eff = Effect::new(EffectKind::ProhibitedFunctionCall, qname, func.range());
            Some(eff)
        } else if self.contains(&fname) {
            // Known safe builtin; skip emitting any effect to avoid
            // unnecessary work checking it in project.rs.
            None
        } else {
            None
        }
    }

    /// Returns true if the given function name is a known builtin (safe or unsafe).
    pub fn is_known_builtin(&self, func: &Expr) -> bool {
        func.as_var_name()
            .is_some_and(|fname| self.contains(&fname) || self.is_prohibited_call(&fname))
    }
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
