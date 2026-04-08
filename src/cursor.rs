/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::StmtClassDef;
use ruff_python_ast::StmtFunctionDef;
use ruff_python_ast::name::Name;

/// A kind of block that encloses an AST node.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Block {
    TryBody,
}

#[derive(Debug, Clone)]
pub struct BlockStack {
    stack: Vec<Block>,
}

impl BlockStack {
    pub fn new() -> Self {
        BlockStack { stack: vec![] }
    }

    pub fn contains(&self, block: &Block) -> bool {
        self.stack.iter().any(|s| s == block)
    }

    pub fn push(&mut self, block: Block) {
        self.stack.push(block);
    }

    pub fn pop(&mut self) {
        self.stack.pop();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScopeKind {
    Module,
    Class,
    Function,
}

impl ScopeKind {
    // Module and Class scopes are "eager" in that their bodies are evaluated at import time
    // (contrast Function scopes, whose bodies are only evaluated when the function is called).
    pub fn is_eager(&self) -> bool {
        matches!(self, Self::Module | Self::Class)
    }
}

#[derive(Debug, Clone)]
pub struct Scope {
    kind: ScopeKind,
    name: Name,
}

impl Scope {
    pub fn new(kind: ScopeKind, name: Name) -> Self {
        Self { kind, name }
    }
}

/// A cursor that moves through a Python module's AST and tracks block and scope information.
#[derive(Debug, Clone)]
pub struct Cursor {
    block_stack: BlockStack,
    scopes: Vec<Scope>,
    // Fully-qualified ModuleName at each scope depth.
    // qualified_scopes[i] == ModuleName::from_parts(scopes[0..=i].map(|s| &s.name))
    qualified_scopes: Vec<ModuleName>,
}

impl Cursor {
    pub fn new() -> Self {
        Self {
            block_stack: BlockStack::new(),
            scopes: Vec::new(),
            qualified_scopes: Vec::new(),
        }
    }

    pub fn enter_block(&mut self, block: Block) {
        self.block_stack.push(block);
    }

    pub fn leave_block(&mut self) {
        self.block_stack.pop();
    }

    pub fn in_block(&self, block: &Block) -> bool {
        self.block_stack.contains(block)
    }

    pub fn enter_module_scope(&mut self, mod_name: &ModuleName) {
        self.scopes
            .push(Scope::new(ScopeKind::Module, Name::new(mod_name.as_str())));
        self.qualified_scopes.push(*mod_name);
    }

    pub fn enter_function_scope(&mut self, func: &StmtFunctionDef) {
        self.enter_function_scope_name(func.name.id.clone());
    }

    pub fn enter_function_scope_name(&mut self, name: Name) {
        self.push_scope(ScopeKind::Function, name);
    }

    pub fn enter_class_scope(&mut self, cls: &StmtClassDef) {
        self.enter_class_scope_name(cls.name.id.clone());
    }

    pub fn enter_class_scope_name(&mut self, name: Name) {
        self.push_scope(ScopeKind::Class, name);
    }

    fn push_scope(&mut self, kind: ScopeKind, name: Name) {
        let cached_name = match self.qualified_scopes.last() {
            Some(parent) => parent.append(&name),
            None => ModuleName::from_name(&name),
        };
        self.scopes.push(Scope::new(kind, name));
        self.qualified_scopes.push(cached_name);
    }

    pub fn exit_scope(&mut self) {
        self.scopes.pop();
        self.qualified_scopes.pop();
    }

    /// Get an iterator over the base name of each scope, starting with the outermost scope.
    ///
    /// e.g. "mod" -> "Class" -> "func"
    pub fn descending_scope_base_names_iter(&self) -> impl Iterator<Item = &Name> {
        self.scopes.iter().map(|s| &s.name)
    }

    /// Get an iterator over the fully qualified name of each scope, starting with the innermost.
    ///
    /// e.g. "mod.Class.func" -> "mod.Class" -> "mod"
    pub fn ascending_scope_names_iter(&self) -> impl Iterator<Item = ModuleName> {
        self.qualified_scopes.iter().rev().copied()
    }

    /// Get an iterator over scopes following Python's LEGB rule:
    /// Local -> Enclosing functions (skipping class scopes) -> Global (module scope).
    /// Builtins are handled separately.
    pub fn legb_scope_names_iter(&self) -> impl Iterator<Item = ModuleName> {
        let len = self.scopes.len();
        let mut names = Vec::with_capacity(len);

        if len > 0 {
            // L: current (innermost) scope
            names.push(self.qualified_scopes[len - 1]);

            // E: walk outward, skipping class scopes, until we hit module scope
            let mut in_function = self.scopes[len - 1].kind == ScopeKind::Function;
            let mut included_module = false;
            for i in (1..len - 1).rev() {
                match self.scopes[i].kind {
                    ScopeKind::Class => {
                        // If we are inside a function looking outward, skip class scopes
                        // (Python's LEGB rule: class bodies don't create enclosing scopes
                        // for nested functions). But if we're in a class scope directly
                        // (e.g. class body referencing class-level vars), include it.
                        if !in_function {
                            names.push(self.qualified_scopes[i]);
                        }
                    }
                    ScopeKind::Function => {
                        names.push(self.qualified_scopes[i]);
                        in_function = true;
                    }
                    ScopeKind::Module => {
                        names.push(self.qualified_scopes[i]);
                        included_module = true;
                    }
                }
            }

            // G: module scope (index 0) - always included if not already
            if len > 1 && !included_module {
                names.push(self.qualified_scopes[0]);
            }
        }

        names.into_iter()
    }

    /// Get a vector containing the base name of each scope, starting with the outermost scope.
    pub fn scope_names(&self) -> Vec<&Name> {
        self.descending_scope_base_names_iter().collect()
    }

    /// Get the name of the current scope.
    pub fn scope(&self) -> ModuleName {
        *self
            .qualified_scopes
            .last()
            .expect("scope() called on empty cursor")
    }

    // Nested scopes are eager as long as every scope in the chain is eager (e.g. if A and B are
    // classes and f is a method, mod.A.B is eager but mod.A.f.B is not, because the class
    // definition of B within A.f will not be executed at import time.
    pub fn in_eager_scope(&self) -> bool {
        self.scopes.iter().all(|s| s.kind.is_eager())
    }

    /// Get the name of the outermost function scope, if it exists.
    pub fn enclosing_function_scope(&self) -> Option<ModuleName> {
        let i = self
            .scopes
            .iter()
            .position(|s| s.kind == ScopeKind::Function)?;
        Some(self.qualified_scopes[i])
    }

    /// Get the name of the nearest (innermost) enclosing function scope,
    /// excluding the current scope itself.
    ///
    /// Used to determine which function a class body's effects should be
    /// attributed to: class bodies are eager, so their effects execute when
    /// the immediately enclosing function is called.
    pub fn nearest_function_scope(&self) -> Option<ModuleName> {
        for i in (0..self.scopes.len().saturating_sub(1)).rev() {
            if self.scopes[i].kind == ScopeKind::Function {
                return Some(self.qualified_scopes[i]);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eager_scopes() {
        let mut c = Cursor::new();

        c.enter_module_scope(&ModuleName::from_str("mod"));
        assert!(c.in_eager_scope());

        c.enter_class_scope_name(Name::new("A"));
        assert!(c.in_eager_scope());

        c.enter_class_scope_name(Name::new("B"));
        assert!(c.in_eager_scope());

        c.enter_function_scope_name(Name::new("f"));
        assert!(!c.in_eager_scope());

        c.enter_class_scope_name(Name::new("C"));
        assert!(!c.in_eager_scope());

        c.exit_scope();
        assert!(!c.in_eager_scope());

        c.exit_scope();
        assert!(c.in_eager_scope());
        assert_eq!(c.scope(), ModuleName::from_str("mod.A.B"));
    }

    #[test]
    fn test_enclosing_function_scope() {
        let mut c = Cursor::new();

        c.enter_module_scope(&ModuleName::from_str("mod"));
        c.enter_class_scope_name(Name::new("A"));
        c.enter_class_scope_name(Name::new("B"));
        assert_eq!(c.enclosing_function_scope(), None);

        c.enter_function_scope_name(Name::new("f"));
        assert_eq!(
            c.enclosing_function_scope(),
            Some(ModuleName::from_str("mod.A.B.f"))
        );

        c.enter_class_scope_name(Name::new("C"));
        assert_eq!(
            c.enclosing_function_scope(),
            Some(ModuleName::from_str("mod.A.B.f"))
        );
    }

    #[test]
    fn test_ascending_scope_names() {
        let mut c = Cursor::new();

        c.enter_module_scope(&ModuleName::from_str("mod"));
        c.enter_class_scope_name(Name::new("A"));
        c.enter_class_scope_name(Name::new("B"));
        c.enter_function_scope_name(Name::new("f"));
        c.enter_class_scope_name(Name::new("C"));

        let expected = ["mod.A.B.f.C", "mod.A.B.f", "mod.A.B", "mod.A", "mod"]
            .iter()
            .map(|s| ModuleName::from_str(s))
            .collect::<Vec<_>>();
        let actual = c.ascending_scope_names_iter().collect::<Vec<_>>();

        assert_eq!(expected, actual);
    }

    #[test]
    fn test_legb_skips_class_scope_for_nested_functions() {
        let mut c = Cursor::new();
        c.enter_module_scope(&ModuleName::from_str("mod"));
        c.enter_class_scope_name(Name::new("A"));
        c.enter_function_scope_name(Name::new("f"));

        // LEGB should be: f (local) -> mod (global), skipping A (class)
        let expected = ["mod.A.f", "mod"]
            .iter()
            .map(|s| ModuleName::from_str(s))
            .collect::<Vec<_>>();
        let actual = c.legb_scope_names_iter().collect::<Vec<_>>();
        assert_eq!(expected, actual);
    }

    #[test]
    fn test_legb_includes_class_scope_from_class_body() {
        let mut c = Cursor::new();
        c.enter_module_scope(&ModuleName::from_str("mod"));
        c.enter_class_scope_name(Name::new("A"));

        // LEGB should be: A (local/class) -> mod (global)
        let expected = ["mod.A", "mod"]
            .iter()
            .map(|s| ModuleName::from_str(s))
            .collect::<Vec<_>>();
        let actual = c.legb_scope_names_iter().collect::<Vec<_>>();
        assert_eq!(expected, actual);
    }

    #[test]
    fn test_legb_nested_functions() {
        let mut c = Cursor::new();
        c.enter_module_scope(&ModuleName::from_str("mod"));
        c.enter_function_scope_name(Name::new("f"));
        c.enter_function_scope_name(Name::new("g"));

        let expected = ["mod.f.g", "mod.f", "mod"]
            .iter()
            .map(|s| ModuleName::from_str(s))
            .collect::<Vec<_>>();
        let actual = c.legb_scope_names_iter().collect::<Vec<_>>();
        assert_eq!(expected, actual);
    }
}
