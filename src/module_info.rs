/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use ahash::AHashMap;
use ahash::AHashSet;
use ahash::RandomState;
use pyrefly_python::module_name::ModuleName;
use pyrefly_python::symbol_kind::SymbolKind;
use pyrefly_util::visit::Visit;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtAssign;
use ruff_python_ast::StmtClassDef;
use ruff_python_ast::StmtFunctionDef;
use ruff_python_ast::name::Name;

use crate::bindings::BindingsTable;
use crate::class::Class;
use crate::class::ClassTable;
use crate::class::Field;
use crate::class::FieldKind;
use crate::cursor::Cursor;
use crate::exports::Exports;
use crate::imports::ImportGraph;
use crate::imports::ImportlibState;
use crate::module_parser::ParsedModule;
use crate::pyrefly::definitions::Definition;
use crate::pyrefly::definitions::DefinitionStyle;
use crate::pyrefly::definitions::Definitions;
use crate::pyrefly::definitions::MutableCaptureKind;
use crate::pyrefly::sys_info::SysInfo;
use crate::stubs::Stubs;
use crate::traits::DefinitionExt;
use crate::traits::DefinitionsExt;
use crate::traits::ExprExt;
use crate::traits::ModuleNameExt;

// Derived information about a module which does not change over the course of analysis.

/// Definitions collected across all scopes in a module.
#[derive(Debug)]
pub struct DefinitionTable {
    // Map of scope to definitions defined in that scope
    pub definitions: AHashMap<ModuleName, Definitions>,
    // Scopes with statements executed at import time (module, class definitions)
    pub eager_scopes: AHashSet<ModuleName>,
    // Fully qualified function and method names (e.g. foo.A.f)
    pub functions: AHashSet<ModuleName>,
    // Tracks the enclosing top-level function scope for nested defs
    pub enclosing_functions: AHashMap<ModuleName, ModuleName>,
    // Ordered parameter names per function scope, for arg-param matching
    pub param_names: AHashMap<ModuleName, Vec<Name>>,
}

impl DefinitionTable {
    // For testing purposes
    pub fn empty() -> Self {
        Self {
            definitions: AHashMap::new(),
            eager_scopes: AHashSet::new(),
            functions: AHashSet::new(),
            enclosing_functions: AHashMap::new(),
            param_names: AHashMap::new(),
        }
    }

    pub fn get(&self, scope: &ModuleName, name: &Name) -> Option<&Definition> {
        let defs = self.definitions.get(scope)?;
        defs.definitions.get(name)
    }

    pub fn get_param_index(&self, func_scope: &ModuleName, param_name: &str) -> Option<usize> {
        let names = self.param_names.get(func_scope)?;
        names.iter().position(|n| n.as_str() == param_name)
    }

    pub fn resolve(&self, cursor: &Cursor, value: &Expr) -> Option<ResolvedName<'_>> {
        let name = value.base_name()?;
        self.resolve_name(cursor, name)
    }

    pub fn is_imported(&self, cursor: &Cursor, value: &Expr) -> bool {
        self.lookup(cursor, value)
            .is_some_and(|def| def.is_import())
    }

    pub fn is_global(&self, cursor: &Cursor, value: &Expr) -> bool {
        self.resolve(cursor, value)
            .is_some_and(|res| res.is_global())
    }

    // Look up a name following Python's LEGB (Local-Enclosing-Global-Builtin) rule.
    // Class scopes are skipped when looking up from an enclosed function scope.
    // Builtins are handled separately in ModuleInfo::resolve_builtins.
    fn resolve_name(&self, cursor: &Cursor, name: Name) -> Option<ResolvedName<'_>> {
        for scope in cursor.legb_scope_names_iter() {
            if let Some(def) = self.get(&scope, &name) {
                return Some(ResolvedName {
                    name,
                    definition: def,
                    scope,
                    scope_definitions: &self.definitions[&scope],
                });
            }
        }
        None
    }

    // Look up a name in a chain of parent scopes.
    fn lookup(&self, cursor: &Cursor, value: &Expr) -> Option<&Definition> {
        Some(self.resolve(cursor, value)?.definition)
    }
}

#[derive(Debug)]
pub struct ResolvedName<'a> {
    pub name: Name,
    pub definition: &'a Definition,
    pub scope: ModuleName,
    pub scope_definitions: &'a Definitions,
}

impl<'a> ResolvedName<'a> {
    pub fn is_global(&self) -> bool {
        self.definition.style == DefinitionStyle::MutableCapture(MutableCaptureKind::Global)
    }

    pub fn is_import(&self) -> bool {
        self.definition.is_import()
    }

    fn scope_prefix(&self) -> ModuleName {
        match self.definition.get_imported_module_name() {
            Some(mod_name) => mod_name,
            None => self.scope,
        }
    }

    pub fn qualified_name(&self) -> ModuleName {
        self.qualify_name(&ModuleName::from_name(&self.name))
    }

    // Qualify a possibly dotted name with the scope of this name
    // (We have looked up the base name of an expression like `x.f()` and resolved the scope that
    // `x` was found in; separately, we use `full_name` to get the full expression `x.f` as the
    // called function. This method joins the parts together.)
    // TODO: Do all this more coherently; we should not need to call this manually.
    pub fn qualify_name(&self, name: &ModuleName) -> ModuleName {
        if matches!(self.definition.style, DefinitionStyle::ImportModule(_)) {
            // Not a from import, so we don't have a prefix
            *name
        } else {
            self.scope_prefix().concat(name)
        }
    }
}

/// Representation of an analyzed Python module, internally used by SourceAnalyzer and StubAnalyzer.
/// Precursor to an AnalyzedModule object.
#[derive(Debug)]
pub struct ModuleInfo<'a> {
    pub module_name: ModuleName,
    pub definitions: DefinitionTable,
    pub bindings: BindingsTable,
    pub classes: ClassTable,
    pub exports: &'a Exports,
    pub stubs: &'a Stubs,
    pub sys_info: &'a SysInfo,
}

impl<'a> ModuleInfo<'a> {
    pub fn new(
        parsed_module: &ParsedModule,
        exports: &'a Exports,
        import_graph: &'a ImportGraph,
        stubs: &'a Stubs,
        sys_info: &'a SysInfo,
    ) -> Self {
        let module_name = parsed_module.name;
        let (definitions, classes) = build_definitions_and_classes(parsed_module, sys_info);
        let bindings = BindingsTable::new(&definitions, exports, import_graph, parsed_module);

        Self {
            module_name,
            definitions,
            bindings,
            exports,
            classes,
            stubs,
            sys_info,
        }
    }

    // Resolve a name through the scope chain, falling back to builtins.
    pub fn resolve(&self, cursor: &Cursor, value: &Expr) -> Option<ResolvedName<'_>> {
        self.definitions
            .resolve(cursor, value)
            .or_else(|| self.resolve_builtins(value))
    }

    // Look up a name in the builtins module definitions.
    fn resolve_builtins(&self, value: &Expr) -> Option<ResolvedName<'_>> {
        let name = value.base_name()?;
        let builtins_module = self.stubs.get(&ModuleName::builtins())?;
        let builtins_scope = ModuleName::builtins();
        let defs = builtins_module
            .definitions
            .definitions
            .get(&builtins_scope)?;
        let def = defs.definitions.get(&name)?;
        Some(ResolvedName {
            name,
            definition: def,
            scope: builtins_scope,
            scope_definitions: defs,
        })
    }

    pub fn is_imported(&self, cursor: &Cursor, value: &Expr) -> bool {
        self.definitions.is_imported(cursor, value)
    }

    pub fn is_global(&self, cursor: &Cursor, value: &Expr) -> bool {
        self.definitions.is_global(cursor, value)
    }

    // Is this symbol reachable from module level (e.g. class variables would qualify because you
    // can reach them via `module.Class.Var` but function locals would not.
    pub fn is_reachable(&self, res: &ResolvedName) -> bool {
        res.is_import() || res.is_global() || self.definitions.eager_scopes.contains(&res.scope)
    }
}

pub fn build_definitions_and_classes(
    parsed_module: &ParsedModule,
    sys_info: &SysInfo,
) -> (DefinitionTable, ClassTable) {
    // Build both data structures in a single pass to reduce the number of
    // necessary AST traversals
    let mut builder = CombinedDefinitionClassBuilder::new(parsed_module, sys_info);
    builder.process_module(&parsed_module.ast.body);
    builder.finalize()
}

pub fn get_import_module_state_from_def(
    definitions: &AHashMap<ModuleName, Definitions, RandomState>,
    scope: &ModuleName,
) -> ImportlibState {
    match definitions.get(scope) {
        Some(def) => ImportlibState::new(def.has_importlib, def.has_import_module),
        None => ImportlibState::new(false, false),
    }
}

struct CombinedDefinitionClassBuilder<'a> {
    module_name: ModuleName,
    is_init: bool,
    sys_info: &'a SysInfo,
    cursor: Cursor,

    // DefinitionTable fields
    definitions_map: AHashMap<ModuleName, Definitions>,
    eager_scopes: AHashSet<ModuleName>,
    functions_set: AHashSet<ModuleName>,
    enclosing_functions: AHashMap<ModuleName, ModuleName>,
    param_names: AHashMap<ModuleName, Vec<Name>>,

    // ClassTable fields
    classes_map: AHashMap<ModuleName, crate::class::Class>,
}

impl<'a> CombinedDefinitionClassBuilder<'a> {
    fn new(parsed_module: &ParsedModule, sys_info: &'a SysInfo) -> Self {
        Self {
            module_name: parsed_module.name,
            is_init: parsed_module.is_init,
            sys_info,
            cursor: Cursor::new(),
            definitions_map: AHashMap::new(),
            eager_scopes: AHashSet::new(),
            functions_set: AHashSet::new(),
            enclosing_functions: AHashMap::new(),
            param_names: AHashMap::new(),
            classes_map: AHashMap::new(),
        }
    }

    fn process_module(&mut self, body: &[Stmt]) {
        self.cursor.enter_module_scope(&self.module_name);
        self.process_scope(body);
        self.cursor.exit_scope();
    }

    fn process_scope(&mut self, body: &[Stmt]) {
        let scope = self.cursor.scope();

        // Extract definitions for this scope (DefinitionTable)
        let defs = Definitions::make(body, self.module_name, self.is_init, self.sys_info);
        self.definitions_map.insert(scope, defs);

        if self.cursor.in_eager_scope() {
            self.eager_scopes.insert(scope);
        }

        // Process statements
        self.process_stmts(body);
    }

    fn process_stmts(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.process_stmt(stmt);
        }
    }

    fn process_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::ClassDef(cls) => self.process_class_def(cls),
            Stmt::FunctionDef(func) => self.process_function_def(func),
            Stmt::Assign(assign) => self.process_assign(assign),
            _ => {
                // Recurse for nested definitions
                stmt.recurse(&mut |s| self.process_stmt(s));
            }
        }
    }

    fn process_assign(&mut self, assign: &StmtAssign) {
        let scope = self.cursor.scope();

        let import_module_state = get_import_module_state_from_def(&self.definitions_map, &scope);
        for target in &assign.targets {
            let Expr::Call(call) = &*assign.value else {
                continue;
            };
            let Expr::Name(target_name) = target else {
                continue;
            };
            let Some(def) = self.definitions_map.get_mut(&scope) else {
                continue;
            };
            if let Some(module_name) = import_module_state.match_call(call) {
                let import_def = Definition {
                    range: target_name.range,
                    style: DefinitionStyle::ImportAs(module_name, target_name.id.clone()),
                    needs_anywhere: false,
                    docstring_range: None,
                };
                def.definitions.insert(target_name.id.clone(), import_def);
            }
        }
    }

    fn process_class_def(&mut self, cls: &StmtClassDef) {
        self.cursor.enter_class_scope(cls);
        let scope = self.cursor.scope();

        // Class bodies are eager: they execute when the class statement is reached.
        // If the class is inside a function, its body executes when that function
        // runs, so bubble the class's effects to the nearest enclosing function.
        if let Some(parent) = self.cursor.nearest_function_scope() {
            self.enclosing_functions.insert(scope, parent);
        }

        // Build Class info for ClassTable
        let class_info = self.build_class_info(cls);
        self.classes_map.insert(scope, class_info);

        // Process class body
        self.process_scope(&cls.body);

        self.cursor.exit_scope();
    }

    fn process_function_def(&mut self, func: &StmtFunctionDef) {
        self.cursor.enter_function_scope(func);
        let scope = self.cursor.scope();

        // Record for DefinitionTable
        self.functions_set.insert(scope);
        if let Some(parent) = self.cursor.enclosing_function_scope() {
            self.enclosing_functions.insert(scope, parent);
        }

        // Process function body
        self.process_scope(&func.body);

        // Extract function parameters
        self.extract_function_params(func);

        self.cursor.exit_scope();
    }

    fn extract_function_params(&mut self, func: &StmtFunctionDef) {
        let scope = self.cursor.scope();
        if let Some(defs) = self.definitions_map.get_mut(&scope) {
            let box params = &func.parameters;
            let mut ordered_names = Vec::new();
            for p in params.iter_non_variadic_params() {
                let def = Definition {
                    range: p.range,
                    style: DefinitionStyle::Unannotated(SymbolKind::Parameter),
                    needs_anywhere: false,
                    docstring_range: None,
                };
                ordered_names.push(p.parameter.name.id.clone());
                defs.definitions.insert(p.parameter.name.id.clone(), def);
            }
            self.param_names.insert(scope, ordered_names);
        }
    }

    fn build_class_info(&self, cls: &StmtClassDef) -> crate::class::Class {
        let mut class = Class::empty(self.module_name);
        class.name = ModuleName::from_name(&cls.name.id);

        // Extract bases and metaclass
        if let Some(args) = &cls.arguments {
            for base in &args.args {
                if let Some(b) = self.get_class_name(base) {
                    class.bases.push(b);
                } else {
                    class.has_unknown_base = true;
                }
            }

            for kwarg in &args.keywords {
                if let Some(id) = &kwarg.arg {
                    if id.as_str() == "metaclass" {
                        let mcls = self.get_class_name(&kwarg.value);
                        class.metaclass = mcls;
                        if mcls.is_none() {
                            class.has_unknown_metaclass = true;
                        }
                    }
                }
            }
        }

        // Extract fields from class body
        for stmt in &cls.body {
            match stmt {
                Stmt::FunctionDef(func) => {
                    let mut kind = FieldKind::InstanceMethod;
                    for dec in &func.decorator_list {
                        if let Some(k) = self.method_kind_from_decorator(&dec.expression) {
                            kind = k;
                        }
                    }
                    class.fields.push(Field {
                        name: func.name.id.clone(),
                        kind,
                    });
                }
                Stmt::Assign(x) => {
                    for target in &x.targets {
                        if let Expr::Name(n) = target {
                            class.fields.push(Field {
                                kind: FieldKind::ClassVar,
                                name: n.id.clone(),
                            });
                        }
                    }
                }
                Stmt::AnnAssign(x) => {
                    if let Expr::Name(n) = &*x.target {
                        class.fields.push(Field {
                            kind: FieldKind::ClassVar,
                            name: n.id.clone(),
                        });
                    }
                }
                _ => {}
            }
        }

        class
    }

    fn get_class_name(&self, expr: &Expr) -> Option<ModuleName> {
        let res = self.resolve_expr(expr)?;
        Some(res.qualify_name(&expr.full_name()?))
    }

    fn resolve_expr(&self, x: &Expr) -> Option<ResolvedName<'_>> {
        let name = x.base_name()?;
        self.resolve_name(name)
    }

    fn resolve_name(&self, name: Name) -> Option<ResolvedName<'_>> {
        for scope in self.cursor.legb_scope_names_iter() {
            if let Some(defs) = self.definitions_map.get(&scope) {
                if let Some(def) = defs.definitions.get(&name) {
                    return Some(ResolvedName {
                        name,
                        definition: def,
                        scope,
                        scope_definitions: defs,
                    });
                }
            }
        }
        None
    }

    fn method_kind_from_decorator(&self, expr: &Expr) -> Option<crate::class::FieldKind> {
        match expr {
            Expr::Name(n) => match n.id.as_str() {
                "property" => Some(FieldKind::Property),
                "classmethod" => Some(FieldKind::ClassMethod),
                "staticmethod" => Some(FieldKind::StaticMethod),
                _ => None,
            },
            Expr::Attribute(a) => {
                (a.attr.as_str() == "setter").then_some(FieldKind::PropertySetter)
            }
            _ => None,
        }
    }

    fn finalize(self) -> (DefinitionTable, ClassTable) {
        let definitions = DefinitionTable {
            definitions: self.definitions_map,
            eager_scopes: self.eager_scopes,
            functions: self.functions_set,
            enclosing_functions: self.enclosing_functions,
            param_names: self.param_names,
        };
        let classes = ClassTable::new(self.classes_map);
        (definitions, classes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_parser::parse_source;
    use crate::test_lib::assert_str_keys;
    use crate::traits::AstExt;
    use crate::traits::SysInfoExt;

    #[test]
    fn test_module_reachability() {
        let code = r#"
from foo import A
import bar

B = 10

class C:
    X = 1
    def f(self, x):
        y = 2
}
"#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let import_graph = ImportGraph::new();
        let stubs = Stubs::new();
        let sys_info = SysInfo::lg_default();
        let exports = Exports::new(&parsed_module, &import_graph, &sys_info);
        let info = ModuleInfo::new(&parsed_module, &exports, &import_graph, &stubs, &sys_info);
        let resolve = |scopes: &[&str], name: &str| -> Option<ResolvedName> {
            // Turn a string like "mod.Class.func" into a Cursor, by assuming the first item is
            // always a module and only classes start with an uppercase character.
            let mut cursor = Cursor::new();
            for (i, scope_name) in scopes.iter().enumerate() {
                if i == 0 {
                    cursor.enter_module_scope(&ModuleName::from_str(scope_name));
                } else if scope_name.starts_with(char::is_uppercase) {
                    cursor.enter_class_scope_name(Name::new(scope_name));
                } else {
                    cursor.enter_function_scope_name(Name::new(scope_name));
                }
            }
            info.definitions.resolve_name(&cursor, Name::new(name))
        };
        let is_reachable = |scopes: &[&str], name: &str| -> bool {
            info.is_reachable(&resolve(scopes, name).unwrap())
        };
        assert!(is_reachable(&["test"], "A"));
        assert!(is_reachable(&["test"], "bar"));
        assert!(is_reachable(&["test"], "B"));
        assert!(is_reachable(&["test"], "C"));
        assert!(is_reachable(&["test", "C"], "X"));
        assert!(is_reachable(&["test", "C"], "f"));
        assert!(!is_reachable(&["test", "C", "f"], "y"));
    }

    #[test]
    fn test_is_init_records_implicit_submodules() {
        // When is_init is true, `import pkg.sub` inside `pkg/__init__.py`
        // should record `sub` as an implicitly imported submodule.
        let code = r#"
import pkg.sub
from . import child
"#;
        let mod_name = ModuleName::from_str("pkg");
        let parsed_module = parse_source(code, mod_name, true);
        let sys_info = SysInfo::lg_default();
        let (definitions, _classes) = build_definitions_and_classes(&parsed_module, &sys_info);
        let defs = definitions.definitions.get(&mod_name).unwrap();
        let submodules: Vec<&str> = defs
            .implicitly_imported_submodules
            .iter()
            .map(|x| x.as_str())
            .collect();
        assert!(
            submodules.contains(&"sub"),
            "expected 'sub' in implicitly imported submodules, got {:?}",
            submodules
        );
    }

    #[test]
    fn test_non_init_does_not_record_implicit_submodules() {
        // When is_init is false, the same imports should NOT produce
        // implicitly imported submodules.
        let code = r#"
import pkg.sub
"#;
        let mod_name = ModuleName::from_str("pkg");
        let parsed_module = parse_source(code, mod_name, false);
        let sys_info = SysInfo::lg_default();
        let (definitions, _classes) = build_definitions_and_classes(&parsed_module, &sys_info);
        let defs = definitions.definitions.get(&mod_name).unwrap();
        assert!(
            defs.implicitly_imported_submodules.is_empty(),
            "expected no implicitly imported submodules for non-init module, got {:?}",
            defs.implicitly_imported_submodules
                .iter()
                .map(|x| x.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_function_params() {
        let code = r#"
def f(x, y):
    pass
"#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let import_graph = ImportGraph::new();
        let stubs = Stubs::new();
        let sys_info = SysInfo::lg_default();
        let exports = Exports::new(&parsed_module, &import_graph, &sys_info);
        let info = ModuleInfo::new(&parsed_module, &exports, &import_graph, &stubs, &sys_info);
        let scope = ModuleName::from_str("test.f");
        let defs = info.definitions.definitions.get(&scope).unwrap();
        assert_str_keys(defs.definitions.keys(), vec!["x", "y"]);
    }

    fn make_module_info_and_resolve(
        code: &str,
        scopes: &[&str],
        name: &str,
    ) -> Option<(ModuleName, bool)> {
        use pyrefly_python::ast::Ast;

        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let import_graph = ImportGraph::new();
        let stubs = Stubs::new();
        let sys_info = SysInfo::lg_default();
        let exports = Exports::new(&parsed_module, &import_graph, &sys_info);
        let info = ModuleInfo::new(&parsed_module, &exports, &import_graph, &stubs, &sys_info);

        let mut cursor = Cursor::new();
        for (i, scope_name) in scopes.iter().enumerate() {
            if i == 0 {
                cursor.enter_module_scope(&ModuleName::from_str(scope_name));
            } else if scope_name.starts_with(char::is_uppercase) {
                cursor.enter_class_scope_name(Name::new(scope_name));
            } else {
                cursor.enter_function_scope_name(Name::new(scope_name));
            }
        }

        // Parse a trivial expression to get a properly constructed Expr::Name
        let (ast, _) = Ast::parse_py(name);
        let stmt = ast.body.first()?;
        let Stmt::Expr(expr_stmt) = stmt else {
            return None;
        };
        let res = info.resolve(&cursor, &expr_stmt.value)?;
        Some((res.scope, res.definition.is_import()))
    }

    #[test]
    fn test_builtin_name_resolves() {
        // `list` is not imported or defined, but should resolve to builtins
        let code = "x = 1\n";
        let result = make_module_info_and_resolve(code, &["test"], "list");
        assert!(result.is_some(), "builtin 'list' should resolve");
        let (scope, is_import) = result.unwrap();
        assert_eq!(scope, ModuleName::builtins());
        assert!(!is_import);
    }

    #[test]
    fn test_builtin_int_resolves() {
        let code = "x = 1\n";
        let result = make_module_info_and_resolve(code, &["test"], "int");
        assert!(result.is_some(), "builtin 'int' should resolve");
        assert_eq!(result.unwrap().0, ModuleName::builtins());
    }

    #[test]
    fn test_local_shadows_builtin() {
        // A local definition of `list` should take precedence over the builtin
        let code = "list = [1, 2, 3]\n";
        let result = make_module_info_and_resolve(code, &["test"], "list");
        assert!(result.is_some(), "'list' should resolve");
        let (scope, _) = result.unwrap();
        assert_eq!(
            scope,
            ModuleName::from_str("test"),
            "local 'list' should shadow builtin"
        );
    }

    #[test]
    fn test_imported_name_not_affected_by_builtins() {
        // An imported name should still resolve as an import, not as a builtin
        let code = "from foo import bar\n";
        let result = make_module_info_and_resolve(code, &["test"], "bar");
        assert!(result.is_some());
        let (scope, is_import) = result.unwrap();
        assert_eq!(scope, ModuleName::from_str("test"));
        assert!(is_import, "'bar' should be an import");
    }

    #[test]
    fn test_nonexistent_name_does_not_resolve() {
        // A name that's not a builtin and not defined should not resolve
        let code = "x = 1\n";
        let result = make_module_info_and_resolve(code, &["test"], "not_a_real_name");
        assert!(result.is_none());
    }
}
