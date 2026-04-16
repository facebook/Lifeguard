/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::str::FromStr;

use ahash::AHashMap;
use anyhow::Result;
use pyrefly_python::module_name::ModuleName;
use ruff_text_size::TextRange;
use serde::Deserialize;
use serde::Serialize;
use serde_json;

use crate::cursor::TryHandler;
use crate::format::ErrorString;
use crate::format::bare_string;
use crate::module_parser::ParsedModule;

// NOTE: This crate uses ModuleName throughout to store fully qualified names of all kinds.

// Track side effects of various python statements.
// We need to track side effects both at the module level (triggered by importing the module) and
// at the function level (triggered by calling the function).
#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectKind {
    // Assigning to a global via `global` statement.
    GlobalVarAssign,
    // Assigning to a class variable.
    ClassVarAssign,
    // Mutating a global (e.g., `my_list.append(x)` at module level).
    GlobalVarMutation,
    // Decorator application.
    DecoratorCall,
    // Imported decorator application.
    ImportedDecoratorCall,
    // Import triggered implicitly.
    ImplicitImport,
    // Mutating an imported variable (e.g., `foo.x = 1`).
    ImportedVarMutation,
    // Calling an imported function.
    ImportedFunctionCall,
    // Accessing an attribute on an imported type (may be a property).
    ImportedTypeAttr,
    // Calling a method.
    MethodCall,
    // Calling a method on a function parameter.
    ParamMethodCall,
    // Calling a locally-defined function.
    FunctionCall,
    // Calling explicitly prohibited builtins (e.g., `getattr` with non-literal arg).
    ProhibitedFunctionCall,
    // Call where the decorator target cannot be resolved.
    UnknownDecoratorCall,
    // Call where the function target cannot be resolved.
    UnknownFunctionCall,
    // Call where the method target cannot be resolved.
    UnknownMethodCall,
    // Binary operation on imported values (may trigger `__eq__` etc.).
    UnknownValueBinaryOp,
    // Accessing an attribute on an unresolved object.
    UnknownObject,
    // Explicitly raising an exception.
    Raise,
    // Attribute assignment.
    SetAttr,
    // Subscript assignment.
    SetSubscript,
    // Class with `__del__` method.
    CustomFinalizer,
    // Calling `exec()`.
    ExecCall,
    // Accessing `sys.modules`.
    SysModulesAccess,
    // Re-exporting a submodule's name via `__all__`.
    SubmoduleReExport,
    // Passing an imported variable as a function argument.
    ImportedVarArgument,
    // Re-assigning an imported variable (for re-export tracking).
    ImportedVarReassignment,
    // For stub files. Update is_unsafe_stub_effect() when adding entries here.
    // Explicitly marks a function as side-effect-free.
    NoEffects,
    // Function body is `...` (unknown).
    UnknownEffects,
    // Explicitly marks as unsafe.
    Unsafe,
    // Marks a method as mutating its receiver.
    Mutation,
    // Marks a dunder method.
    Dunder,
    // Call has more than 64 positional arguments, exceeding the tracking bitset.
    TooManyArgs,
}

impl ErrorString for EffectKind {
    fn error_string(&self) -> String {
        bare_string(&self)
    }
}

impl FromStr for EffectKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        // Convert s to a quoted string so that serde can parse it as json
        let json = format!(r#""{:}""#, s);
        Ok(serde_json::from_str(&json)?)
    }
}

impl EffectKind {
    // Is this effect a call where we can run the body to get transitive effects
    pub fn is_runnable(&self) -> bool {
        matches!(
            self,
            Self::FunctionCall
                | Self::ImportedFunctionCall
                | Self::DecoratorCall
                | Self::ImportedDecoratorCall
                | Self::MethodCall
        )
    }

    // Regardless of reachability from a top-level statement, this effect anywhere in a module triggers the module's addition to the "load_imports_eagerly" set
    pub fn requires_eager_loading_imports(&self) -> bool {
        matches!(
            self,
            Self::CustomFinalizer | Self::ExecCall | Self::SysModulesAccess
        )
    }

    pub fn is_unsafe_stub_effect(&self) -> bool {
        matches!(self, Self::UnknownEffects | Self::Unsafe | Self::Mutation)
    }
}

#[derive(Debug)]
pub enum CallKind {
    Function,
    Method,
    Decorator,
}

impl CallKind {
    pub fn unknown_call_effect(&self) -> EffectKind {
        match self {
            Self::Function => EffectKind::UnknownFunctionCall,
            Self::Method => EffectKind::UnknownMethodCall,
            Self::Decorator => EffectKind::UnknownDecoratorCall,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct CallData {
    has_unsafe_args: bool,
    unsafe_arg_indices: u64,
    /// Names of keyword arguments that are imported variables.
    unsafe_keyword_names: Vec<ModuleName>,
    /// Set when a **kwargs expansion contains an imported variable,
    /// meaning we can't determine which specific keywords are unsafe.
    has_unsafe_kwargs_expansion: bool,
}

impl CallData {
    pub fn new(
        has_unsafe_args: bool,
        unsafe_arg_indices: u64,
        unsafe_keyword_names: Vec<ModuleName>,
        has_unsafe_kwargs_expansion: bool,
    ) -> Self {
        Self {
            has_unsafe_args,
            unsafe_arg_indices,
            unsafe_keyword_names,
            has_unsafe_kwargs_expansion,
        }
    }

    pub fn empty() -> Self {
        Self {
            has_unsafe_args: false,
            unsafe_arg_indices: 0,
            unsafe_keyword_names: Vec::new(),
            has_unsafe_kwargs_expansion: false,
        }
    }

    pub fn has_unsafe_args(&self) -> bool {
        self.has_unsafe_args
    }

    pub fn has_unsafe_arg_index(&self, idx: usize) -> bool {
        idx < 64 && (self.unsafe_arg_indices & (1u64 << idx)) != 0
    }

    pub fn has_unsafe_keywords(&self) -> bool {
        !self.unsafe_keyword_names.is_empty() || self.has_unsafe_kwargs_expansion
    }

    pub fn has_unsafe_keyword(&self, name: &str) -> bool {
        self.has_unsafe_kwargs_expansion
            || self
                .unsafe_keyword_names
                .iter()
                .any(|kw| kw.as_str() == name)
    }

    pub fn has_precise_keyword_tracking(&self) -> bool {
        !self.has_unsafe_kwargs_expansion
    }

    pub fn has_any_tracked_args(&self) -> bool {
        self.unsafe_arg_indices != 0
            || !self.unsafe_keyword_names.is_empty()
            || self.has_unsafe_kwargs_expansion
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub enum EffectData {
    None,
    Call(Box<CallData>),
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct Effect {
    pub kind: EffectKind,
    pub name: ModuleName,
    pub range: TextRange,
    pub data: EffectData,
    /// Try handlers from enclosing try blocks at the call site.
    /// Only present for runnable effects inside try bodies.
    pub try_handlers: Option<Box<[TryHandler]>>,
}

impl Effect {
    pub fn new(kind: EffectKind, name: ModuleName, range: TextRange) -> Self {
        Self {
            kind,
            name,
            range,
            data: EffectData::None,
            try_handlers: None,
        }
    }

    pub fn with_data(
        kind: EffectKind,
        name: ModuleName,
        range: TextRange,
        data: EffectData,
    ) -> Self {
        Self {
            kind,
            name,
            range,
            data,
            try_handlers: None,
        }
    }

    pub fn with_try_handlers<'a>(
        mut self,
        try_handlers: impl Iterator<Item = &'a TryHandler>,
    ) -> Self {
        let handlers: Vec<TryHandler> = try_handlers.cloned().collect();
        if !handlers.is_empty() {
            self.try_handlers = Some(handlers.into_boxed_slice());
        }
        self
    }

    /// Whether this effect has enclosing try handlers that could catch exceptions.
    pub fn has_try_context(&self) -> bool {
        self.try_handlers.is_some()
    }

    /// Check whether any enclosing try handler catches the given exception name.
    pub fn try_context_catches(&self, exc_name: &ModuleName) -> bool {
        self.try_handlers
            .as_ref()
            .is_some_and(|handlers| handlers.iter().any(|h| h.catches(exc_name)))
    }
}

/// Track effects based on scope.  Keys are either a module name or a fully qualified
/// function/method name, corresponding to the scope within which the effect occurs.
#[derive(Debug, Clone)]
pub struct EffectTable {
    table: AHashMap<ModuleName, Vec<Effect>>,
}

impl EffectTable {
    pub fn empty() -> Self {
        EffectTable {
            table: AHashMap::new(),
        }
    }

    pub fn new(table: AHashMap<ModuleName, Vec<Effect>>) -> Self {
        EffectTable { table }
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    pub fn insert(&mut self, name: ModuleName, effect: Effect) {
        self.table.entry(name).or_default().push(effect);
    }

    pub fn get(&self, name: &ModuleName) -> Option<&Vec<Effect>> {
        self.table.get(name)
    }

    /// Get an iterator over the names of all the scopes that have effects.
    pub fn keys(&self) -> impl Iterator<Item = &ModuleName> {
        self.table.keys()
    }

    /// Get an iterator over all effects, grouped by scope name.
    pub fn values(&self) -> impl Iterator<Item = &Vec<Effect>> {
        self.table.values()
    }

    /// Get an iterator over all effects, grouped and keyed by their scope name.
    pub fn iter(&self) -> impl Iterator<Item = (&ModuleName, &Vec<Effect>)> {
        self.table.iter()
    }

    /// Get a mutable iterator over all effects, grouped and keyed by their scope name.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&ModuleName, &mut Vec<Effect>)> {
        self.table.iter_mut()
    }

    /// Filter out any entries that don't pass the given predicate.
    pub(crate) fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&ModuleName, &mut Vec<Effect>) -> bool,
    {
        self.table.retain(f)
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        for (name, effs) in &other.table {
            if let Some(val) = self.table.get_mut(name) {
                val.extend(effs.iter().cloned());
            } else {
                self.table.insert(*name, effs.clone());
            };
        }
    }

    pub fn pretty_print(&self, module: &ParsedModule, file_contents: &str, show_expr: bool) {
        // Sort scopes by name.  The current module is the most important and that will always show
        // up first.  Following modules will _generally_ follow in order of scope depth, but for
        // names like "current_scope.foo.bar" and "current_scope.foot" you might not get the exact
        // order that you want.  That's okay, it's close enough.
        let mut tuples: Vec<_> = self.table.iter().collect();
        tuples.sort_by_key(|(scope, _)| scope.as_str());

        for (scope, eff_set) in tuples {
            // Sort effects by the start of their text range.
            let mut effs: Vec<_> = eff_set.iter().collect();
            effs.sort_by_key(|eff| eff.range.start());

            println!("Scope: {}", scope.as_str());
            for eff in effs {
                println!(
                    "    Line {}:",
                    module.byte_to_line_number(eff.range.start().into())
                );
                println!("        Effect: {:?}", eff.kind);
                if show_expr {
                    if let Some(expr) = file_contents.get(eff.range.to_std_range()) {
                        println!("        Expr: {}", expr);
                    } else {
                        println!("        Expr: <Error: Can't resolve range {:?}>", eff.range);
                    }
                }
                if !eff.name.as_str().is_empty() {
                    println!("        Name: {}", eff.name.as_str());
                }
                match &eff.data {
                    EffectData::None => {}
                    EffectData::Call(call) => {
                        println!("        UnsafeArgs: {}", call.has_unsafe_args());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::TryHandler;

    #[test]
    fn test_serialize_effect_kind() {
        let out = &EffectKind::GlobalVarAssign.error_string();
        assert_eq!(out, "global-var-assign");
    }

    #[test]
    fn test_deserialize_effect_kind() {
        let out = EffectKind::from_str("global-var-assign").unwrap();
        assert_eq!(out, EffectKind::GlobalVarAssign);
    }

    #[test]
    fn test_call_kind_unknown_call_effect() {
        assert_eq!(
            CallKind::Function.unknown_call_effect(),
            EffectKind::UnknownFunctionCall
        );
        assert_eq!(
            CallKind::Method.unknown_call_effect(),
            EffectKind::UnknownMethodCall
        );
        assert_eq!(
            CallKind::Decorator.unknown_call_effect(),
            EffectKind::UnknownDecoratorCall
        );
    }

    #[test]
    fn test_call_data_has_any_tracked_args() {
        let empty = CallData::empty();
        assert!(!empty.has_any_tracked_args());

        let with_indices = CallData::new(true, 1, Vec::new(), false);
        assert!(with_indices.has_any_tracked_args());

        let with_keywords = CallData::new(false, 0, vec![ModuleName::from_str("kwarg")], false);
        assert!(with_keywords.has_any_tracked_args());

        let with_expansion = CallData::new(false, 0, Vec::new(), true);
        assert!(with_expansion.has_any_tracked_args());
    }

    #[test]
    fn test_call_data_has_unsafe_keyword() {
        let with_expansion = CallData::new(false, 0, Vec::new(), true);
        assert!(with_expansion.has_unsafe_keyword("anything"));
        assert!(with_expansion.has_unsafe_keywords());
        assert!(!with_expansion.has_precise_keyword_tracking());

        let with_names = CallData::new(false, 0, vec![ModuleName::from_str("foo")], false);
        assert!(with_names.has_unsafe_keyword("foo"));
        assert!(!with_names.has_unsafe_keyword("bar"));
        assert!(with_names.has_unsafe_keywords());
        assert!(with_names.has_precise_keyword_tracking());

        let empty = CallData::empty();
        assert!(!empty.has_unsafe_keyword("foo"));
        assert!(!empty.has_unsafe_keywords());
    }

    #[test]
    fn test_with_try_handlers_empty_iterator() {
        let eff = Effect::new(
            EffectKind::Raise,
            ModuleName::from_str("ValueError"),
            TextRange::default(),
        )
        .with_try_handlers(std::iter::empty());
        assert!(!eff.has_try_context());
        assert!(eff.try_handlers.is_none());
    }

    #[test]
    fn test_with_try_handlers_populates_field() {
        let handlers = [
            TryHandler::Bare,
            TryHandler::Single(ModuleName::from_str("TypeError")),
        ];
        let eff = Effect::new(
            EffectKind::Raise,
            ModuleName::from_str("ValueError"),
            TextRange::default(),
        )
        .with_try_handlers(handlers.iter());
        assert!(eff.has_try_context());
        assert_eq!(eff.try_handlers.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_try_context_catches_delegates_to_handlers() {
        let handlers = [
            TryHandler::Single(ModuleName::from_str("TypeError")),
            TryHandler::Single(ModuleName::from_str("ValueError")),
        ];
        let eff = Effect::new(
            EffectKind::Raise,
            ModuleName::from_str("ValueError"),
            TextRange::default(),
        )
        .with_try_handlers(handlers.iter());

        assert!(eff.try_context_catches(&ModuleName::from_str("ValueError")));
        assert!(eff.try_context_catches(&ModuleName::from_str("TypeError")));
        assert!(!eff.try_context_catches(&ModuleName::from_str("KeyError")));
    }

    #[test]
    fn test_try_context_catches_no_handlers() {
        let eff = Effect::new(
            EffectKind::Raise,
            ModuleName::from_str("ValueError"),
            TextRange::default(),
        );
        assert!(!eff.try_context_catches(&ModuleName::from_str("ValueError")));
    }

    #[test]
    fn test_effect_table_merge_with_new_keys() {
        use ruff_text_size::TextRange;

        let range = TextRange::default();
        let mod_a = ModuleName::from_str("mod_a");
        let mod_b = ModuleName::from_str("mod_b");

        let mut table1 = EffectTable::empty();
        table1.insert(mod_a, Effect::new(EffectKind::FunctionCall, mod_a, range));

        let mut table2 = EffectTable::empty();
        table2.insert(mod_a, Effect::new(EffectKind::MethodCall, mod_a, range));
        table2.insert(mod_b, Effect::new(EffectKind::Raise, mod_b, range));

        table1.merge(&table2);

        assert_eq!(table1.get(&mod_a).unwrap().len(), 2);
        assert_eq!(table1.get(&mod_b).unwrap().len(), 1);
    }
}
