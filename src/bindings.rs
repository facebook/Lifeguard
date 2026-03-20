/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use ahash::AHashMap;
use pyrefly_python::module_name::ModuleName;
use pyrefly_util::visit::Visit;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprCall;
use ruff_python_ast::ExprNumberLiteral;
use ruff_python_ast::Number;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtAnnAssign;
use ruff_python_ast::StmtAssign;
use ruff_python_ast::StmtClassDef;
use ruff_python_ast::StmtFunctionDef;
use ruff_python_ast::StmtImport;
use ruff_python_ast::StmtImportFrom;
use ruff_python_ast::name::Name;

use crate::cursor::Cursor;
use crate::exports::Attribute;
use crate::exports::Exports;
use crate::imports::ImportGraph;
use crate::module_info::DefinitionTable;
use crate::module_info::ResolvedName;
use crate::module_info::get_import_module_state_from_def;
use crate::module_parser::ParsedModule;
use crate::traits::DefinitionExt;
use crate::traits::ExprExt;

// The value a name is bound to
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    // An expression we cannot resolve
    Unknown,
    // An imported module
    Module(ModuleName),
    // A class
    Class(ModuleName),
    // A function or method
    Function(ModuleName),
    // An instance of a statically defined type
    Instance(ModuleName),
    // A named variable (e.g. a global constant) whose type we cannot determine.
    // See Alias::Global for why this is useful.
    Variable(ModuleName),
}

impl Value {
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    pub fn as_module_name(&self) -> Option<&ModuleName> {
        match self {
            Self::Module(module)
            | Self::Class(module)
            | Self::Function(module)
            | Self::Instance(module)
            | Self::Variable(module) => Some(module),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alias {
    // A globally reachable definition (e.g. a module or class)
    Global(Value),
    // A locally defined name, along with the scope in which it is defined
    Local(ModuleName, Name),
}

impl Alias {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Global(value) => value.as_module_name().map(|v| v.as_str()).unwrap_or(""),
            Self::Local(_, name) => name.as_str(),
        }
    }
}

type Bindings = AHashMap<Name, Value>;
type Aliases = AHashMap<Name, Alias>;

#[derive(Debug)]
pub struct BindingsTable {
    bindings: AHashMap<ModuleName, Bindings>,
    aliases: AHashMap<ModuleName, Aliases>,
}

impl BindingsTable {
    pub fn new(
        definitions: &DefinitionTable,
        exports: &Exports,
        import_graph: &ImportGraph,
        parsed_module: &ParsedModule,
    ) -> Self {
        let module_name = parsed_module.name;
        let aliases = AliasTable::new(definitions, exports, import_graph, parsed_module);
        BindingsTableBuilder::new(module_name, definitions, exports, aliases).build(parsed_module)
    }

    pub fn lookup(&self, scope: &ModuleName, name: &Name) -> Option<&Value> {
        self.bindings.get(scope)?.get(name)
    }

    pub fn lookup_alias(&self, scope: &ModuleName, name: &Name) -> Option<&Alias> {
        self.aliases.get(scope)?.get(name)
    }

    // Useful for testing
    pub fn lookup_str(&self, scope: &str, name: &str) -> Option<&Value> {
        self.lookup(&ModuleName::from_str(scope), &Name::new(name))
    }

    pub fn lookup_alias_str(&self, scope: &str, name: &str) -> Option<&Alias> {
        self.lookup_alias(&ModuleName::from_str(scope), &Name::new(name))
    }

    pub fn get_type(&self, scope: &ModuleName, name: &Name) -> Option<&ModuleName> {
        let val = self.lookup(scope, name)?;
        match val {
            Value::Unknown => None,
            Value::Instance(x) => Some(x),
            _ => None,
        }
    }

    pub fn pretty_print(&self) {
        for (k, v) in &self.bindings {
            println!("Scope: {}", k.as_str());
            for (name, val) in v {
                println!("    {} {:?}", name.as_str(), val);
            }
        }
    }
}

#[derive(Debug)]
pub struct AliasTable {
    aliases: AHashMap<ModuleName, Aliases>,
}

impl AliasTable {
    pub fn new(
        definitions: &DefinitionTable,
        exports: &Exports,
        import_graph: &ImportGraph,
        parsed_module: &ParsedModule,
    ) -> Self {
        let module_name = parsed_module.name;
        AliasTableBuilder::new(module_name, definitions, exports, import_graph).build(parsed_module)
    }

    pub fn lookup_alias(&self, scope: &ModuleName, name: &Name) -> Option<&Alias> {
        self.aliases.get(scope)?.get(name)
    }

    // Follow transitive aliases. Track depth to guard against infinite loops, since we
    // can get confused by control flow.
    fn _resolve(&self, scope: &ModuleName, name: &Name, depth: u8) -> Option<&Alias> {
        // We are likely in an infinite loop if we have followed 5 levels of aliases
        // and still have an alias.
        if depth > 5 {
            return None;
        }
        let ret = self.lookup_alias(scope, name);
        match ret {
            // If we have an alias to a global, return it directly
            Some(Alias::Global(_)) => ret,
            // If we have an alias to a local, it may itself be an alias
            Some(Alias::Local(s, n)) => match self._resolve(s, n, depth + 1) {
                // If it is, return its transitively resolved value
                Some(x) => Some(x),
                // If not, return the local alias
                None => ret,
            },
            None => None,
        }
    }

    pub fn resolve(&self, scope: &ModuleName, name: &Name) -> Option<&Alias> {
        self._resolve(scope, name, 0)
    }
}

struct AliasTableBuilder<'a, 'b> {
    aliases: AHashMap<ModuleName, Aliases>,
    module_name: ModuleName,
    definitions: &'a DefinitionTable,
    exports: &'b Exports,
    import_graph: &'b ImportGraph,
    cursor: Cursor,
}

impl<'a, 'b> AliasTableBuilder<'a, 'b> {
    pub fn new(
        module_name: ModuleName,
        definitions: &'a DefinitionTable,
        exports: &'b Exports,
        import_graph: &'b ImportGraph,
    ) -> Self {
        Self {
            aliases: AHashMap::new(),
            module_name,
            exports,
            import_graph,
            definitions,
            cursor: Cursor::new(),
        }
    }

    pub fn build(mut self, parsed_module: &ParsedModule) -> AliasTable {
        self.module(&parsed_module.ast.body);
        AliasTable {
            aliases: self.aliases,
        }
    }

    fn add_alias(&mut self, name: Name, rhs: Alias) {
        let scope = self.cursor.scope();
        let aliases = self.aliases.entry(scope).or_default();
        aliases.insert(name, rhs);
    }

    fn extract_aliases(&mut self, body: &[Stmt]) {
        self.stmts(body);
    }

    fn module(&mut self, body: &[Stmt]) {
        self.cursor.enter_module_scope(&self.module_name);
        self.extract_aliases(body);
        self.cursor.exit_scope();
    }

    fn class_def(&mut self, cls: &StmtClassDef) {
        self.cursor.enter_class_scope(cls);
        self.extract_aliases(&cls.body);
        self.cursor.exit_scope();
    }

    fn function_def(&mut self, func: &StmtFunctionDef) {
        self.cursor.enter_function_scope(func);
        self.extract_aliases(&func.body);
        self.cursor.exit_scope();
    }

    fn lookup_external_name(&self, name: &ModuleName) -> Option<Value> {
        if self.import_graph.contains(name) {
            Some(Value::Module(name.clone()))
        } else if self.exports.is_class(name) {
            Some(Value::Class(name.clone()))
        } else {
            None
        }
    }

    // See if an expression resolves to a "definition" (a named variable defined in some scope),
    // and is therefore a direct reference to that variable.
    fn expr_def(&self, expr: &Expr) -> Option<Value> {
        let res = self.definitions.resolve(&self.cursor, expr)?;
        let expr_name = expr.full_name()?;
        let fq_name = res.qualify_name(&expr_name);
        self.lookup_external_name(&fq_name)
    }

    fn assign_single(&mut self, lhs: &Expr, rhs: &Expr) {
        let Some(name) = lhs.as_var_name() else {
            // We can't have an alias without a variable name
            return;
        };

        if let Expr::Call(call) = rhs {
            let import_module_state = get_import_module_state_from_def(
                &self.definitions.definitions,
                &self.cursor.scope(),
            );
            if let Some(mod_name) = import_module_state.match_call(call) {
                // We have an alias to a module imported via importlib.import_module
                self.add_alias(name.clone(), Alias::Global(Value::Module(mod_name)));
                return;
            }
        }

        let def = self.expr_def(rhs);
        match def {
            Some(Value::Class(_)) | Some(Value::Module(_)) => {
                // We have an alias to a class or module, so add it and exit.
                self.add_alias(name.clone(), Alias::Global(def.unwrap()));
                return;
            }
            _ => {}
        }

        // If the rhs is not a class or module, try to resolve it as an alias to a variable
        let r = self.definitions.resolve(&self.cursor, rhs);
        if let Some(res) = r {
            if res.definition.is_import() {
                // We cannot resolve this to a type, but we know it's an imported value
                if let Some(var_name) = rhs.full_name() {
                    let rhs_name = res.qualify_name(&var_name);
                    self.add_alias(name.clone(), Alias::Global(Value::Variable(rhs_name)));
                } else {
                    // We don't even have a name but we still want to track this as a cross-module
                    // reference
                    self.add_alias(name.clone(), Alias::Global(Value::Unknown));
                }
            } else if matches!(rhs, Expr::Name(_)) {
                self.add_alias(name, Alias::Local(res.scope, res.name));
            }
        }
    }

    fn assign(&mut self, x: &StmtAssign) {
        // We can only add aliases for single-lhs assignments
        if x.targets.len() == 1 {
            self.assign_single(&x.targets[0], &x.value);
        }
    }

    fn ann_assign(&mut self, x: &StmtAnnAssign) {
        if let Some(val) = &x.value {
            self.assign_single(&x.target, val.as_ref())
        }
    }

    fn import(&mut self, x: &StmtImport) {
        for name in &x.names {
            if let Some(asname) = &name.asname {
                self.add_alias(
                    asname.id.clone(),
                    Alias::Global(Value::Module(ModuleName::from_name(&name.name.id))),
                );
            }
        }
    }

    fn import_from(&mut self, x: &StmtImportFrom) {
        let from_mod = match &x.module {
            Some(m) => ModuleName::from_name(&m.id),
            None => ModuleName::from_str(""),
        };
        for name in &x.names {
            if let Some(asname) = &name.asname {
                let fq_name = from_mod.append(&name.name.id);
                let val = self
                    .lookup_external_name(&fq_name)
                    .unwrap_or(Value::Variable(fq_name));
                self.add_alias(asname.id.clone(), Alias::Global(val));
            }
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::ClassDef(x) => self.class_def(x),
            Stmt::FunctionDef(x) => self.function_def(x),
            Stmt::Assign(x) => self.assign(x),
            Stmt::AnnAssign(x) => self.ann_assign(x),
            Stmt::Import(x) => self.import(x),
            Stmt::ImportFrom(x) => self.import_from(x),
            _ => stmt.recurse(&mut |s| self.stmt(s)),
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }
}

struct BindingsTableBuilder<'a, 'b> {
    bindings: AHashMap<ModuleName, Bindings>,
    aliases: AliasTable,
    module_name: ModuleName,
    definitions: &'a DefinitionTable,
    exports: &'b Exports,
    cursor: Cursor,
    is_stub: bool,
}

impl<'a, 'b> BindingsTableBuilder<'a, 'b> {
    pub fn new(
        module_name: ModuleName,
        definitions: &'a DefinitionTable,
        exports: &'b Exports,
        aliases: AliasTable,
    ) -> Self {
        Self {
            bindings: AHashMap::new(),
            aliases,
            module_name,
            exports,
            definitions,
            cursor: Cursor::new(),
            is_stub: false,
        }
    }

    pub fn build(mut self, parsed_module: &ParsedModule) -> BindingsTable {
        self.is_stub = parsed_module.is_stub();
        self.module(&parsed_module.ast.body);
        BindingsTable {
            bindings: self.bindings,
            aliases: self.aliases.aliases,
        }
    }

    /// Whether we trust type annotations as a source of truth
    fn trust_annotations(&self) -> bool {
        // We treat type annotations in stubs as trustworthy because they are often the only source
        // of truth for variable types, e.g. in os.pyi we have
        //   environ: Environ
        // where a py file would have had
        //   environ: Environ = Environ()
        self.is_stub
    }

    fn resolve_expr(&self, x: &Expr) -> Option<ResolvedName<'_>> {
        self.definitions.resolve(&self.cursor, x)
    }

    fn add_binding(&mut self, name: Name, value: Value) {
        let scope = self.cursor.scope();
        let bindings = self.bindings.entry(scope).or_default();
        bindings.insert(name, value);
    }

    /// Resolve an expression to the class it names, if any.
    /// Used both for constructor calls (to infer instance types) and for
    /// type annotations in stub files.
    fn resolve_to_class(&self, expr: &Expr) -> Option<ModuleName> {
        // If we don't have a named expression, or if we cannot resolve the name in the
        // current scope, return None
        let expr_name = expr.full_name()?;
        let res = self.resolve_expr(expr)?;
        if let Some(alias) = self.aliases.resolve(&res.scope, &res.name) {
            // We have resolved `res.name`, which is the base name of `expr`, i.e. if expr is
            // `foo.bar.baz` we have only resolved `foo`, so we need to be careful that we take the
            // rest of the name into account
            let parts = expr_name.components();
            match alias {
                Alias::Global(Value::Class(c)) => {
                    if parts.len() == 1 {
                        // We have an undotted name that aliases a class; return the class
                        return Some(*c);
                    }
                }
                Alias::Global(Value::Module(m)) => {
                    // The prefix of `expr_name` is an aliased module; replace it with the actual
                    // module name and then see if the new qualified name is a class
                    let mut new = m.components();
                    new.extend_from_slice(&parts[1..]);
                    let name = ModuleName::from_parts(new);
                    if self.exports.is_class(&name) {
                        return Some(name);
                    }
                }
                _ => {}
            }
        } else {
            // This is not an aliased name, look it up directly
            let name = res.qualified_name();
            if self.exports.is_class(&name) {
                return Some(name);
            }
        }
        // We have not matched the expression to a class
        None
    }

    fn get_call_type(&self, call: &ExprCall) -> Option<ModuleName> {
        self.resolve_to_class(&call.func)
    }

    fn get_expr_type(&self, expr: &Expr) -> Option<ModuleName> {
        let m = |s| Some(ModuleName::from_str(s));
        match expr {
            Expr::BooleanLiteral(_) => m("builtins.bool"),
            Expr::BytesLiteral(_) => m("builtins.bytes"),
            Expr::EllipsisLiteral(_) => m("builtins.ellipsis"),
            Expr::NoneLiteral(_) => m("builtins.NoneType"),
            Expr::NumberLiteral(n) => m(get_numeric_type(n)),
            Expr::StringLiteral(_) => m("builtins.str"),
            Expr::Call(call) => self.get_call_type(call),
            Expr::Dict(_) => m("builtins.dict"),
            Expr::Set(_) => m("builtins.set"),
            Expr::ListComp(_) => m("builtins.list"),
            Expr::SetComp(_) => m("builtins.set"),
            Expr::DictComp(_) => m("builtins.dict"),
            Expr::FString(_) => m("builtins.str"),
            Expr::TString(_) => m("builtins.str"),
            Expr::List(_) => m("builtins.list"),
            Expr::Tuple(_) => m("builtins.tuple"),
            _ => None,
        }
    }

    fn extract_bindings(&mut self, body: &[Stmt]) {
        self.stmts(body);
    }

    fn module(&mut self, body: &[Stmt]) {
        self.cursor.enter_module_scope(&self.module_name);
        self.extract_bindings(body);
        self.cursor.exit_scope();
    }

    fn class_def(&mut self, cls: &StmtClassDef) {
        self.cursor.enter_class_scope(cls);
        self.extract_bindings(&cls.body);
        self.cursor.exit_scope();
    }

    fn function_def(&mut self, func: &StmtFunctionDef) {
        self.cursor.enter_function_scope(func);
        self.extract_bindings(&func.body);
        self.cursor.exit_scope();
    }

    fn expr_value(&self, x: &Expr) -> Value {
        if let Some(typ) = self.get_expr_type(x) {
            Value::Instance(typ)
        } else {
            Value::Unknown
        }
    }

    fn get_existing_binding(&self, scope: &ModuleName, name: &Name) -> Value {
        let val = self.bindings.get(scope).map(|b| b.get(name));
        match val {
            Some(Some(x)) => x.clone(),
            _ => Value::Unknown,
        }
    }

    fn get_existing_alias_binding(&self, scope: &ModuleName, name: &Name) -> Value {
        match self.aliases.resolve(scope, name) {
            Some(Alias::Global(val)) => val.clone(),
            Some(Alias::Local(var_scope, var)) => self.get_existing_binding(var_scope, var),
            None => Value::Unknown,
        }
    }

    fn assign_single(&mut self, lhs: &Expr, rhs: &Expr) {
        let Some(name) = lhs.as_var_name() else {
            // We can't have a binding without a variable name
            return;
        };

        // See if we have already detected a global alias for the lhs
        let val = self.get_existing_alias_binding(&self.cursor.scope(), &name);
        if !val.is_unknown() {
            self.add_binding(name.clone(), val);
            return;
        }

        // If not, resolve the rhs to a definition and then check that definition for an existing
        // binding or an alias
        let r = self.definitions.resolve(&self.cursor, rhs);
        if let Some(res) = r {
            // See if we have a binding for the resolved value. If not, see if the value is an
            // alias to something we do have a value for.
            let mut val = self.get_existing_binding(&res.scope, &res.name);
            if val.is_unknown() {
                val = self.get_existing_alias_binding(&res.scope, &res.name);
            }
            self.add_binding(name.clone(), val);
        } else {
            if let Some(typ) = self.match_imported_constructor(rhs) {
                self.add_binding(name, Value::Instance(typ));
                return;
            }
            let val = self.expr_value(rhs);
            self.add_binding(name, val.clone());
        }
    }

    /// See if an expression is a constructor for an imported class, e.g.
    ///   from foo import A as FooA
    ///   x = FooA()  # returns `Some(foo.A)`
    fn match_imported_constructor(&mut self, rhs: &Expr) -> Option<ModuleName> {
        let Expr::Call(call) = rhs else {
            return None;
        };
        let Expr::Name(expr_name) = call.func.as_ref() else {
            return None;
        };
        let imported_name = self.resolve_to_imported_name(&expr_name.id)?;
        let source_name = self.exports.get_definition_source_name(&imported_name)?;
        if self.exports.is_class(&source_name.as_module_name()) {
            Some(imported_name.as_module_name())
        } else {
            None
        }
    }

    // An `import` statement introduces a name into its scope. If that scope is visible at module
    // level (either module scope or class scope), the name will be reexported qualified with that
    // scope prefix. e.g. in the following code:
    //   # module foo.py
    //   class A:
    //     from bar import B
    //
    // will create the symbol `foo.A.B` and mark it as an alias to `bar.B`. This makes the
    // reexports table a convenient way to find imported symbols within the module.
    pub fn resolve_to_imported_name(&self, name: &Name) -> Option<Attribute> {
        for scope in self.cursor.ascending_scope_names_iter() {
            let qualname = Attribute::new(scope, name);
            if let Some(imported_name) = self.exports.resolve_imported_name(&qualname) {
                return Some(imported_name);
            }
        }
        None
    }

    fn assign(&mut self, x: &StmtAssign) {
        if x.targets.len() == 1 {
            self.assign_single(&x.targets[0], &x.value);
            return;
        }

        // We have an assignment like `a, b = <expr>`. Since we don't know what to assign to the
        // individual lhs variables, mark them all as unknown.
        for target in &x.targets {
            if let Some(name) = target.as_var_name() {
                self.add_binding(name, Value::Unknown);
            }
        }
    }

    fn ann_assign(&mut self, x: &StmtAnnAssign) {
        if let Some(val) = &x.value {
            self.assign_single(&x.target, val.as_ref())
        } else if self.trust_annotations() {
            if let Some(name) = x.target.as_var_name() {
                if let Some(class_name) = self.resolve_to_class(&x.annotation) {
                    self.add_binding(name, Value::Instance(class_name));
                }
            }
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::ClassDef(x) => self.class_def(x),
            Stmt::FunctionDef(x) => self.function_def(x),
            Stmt::Assign(x) => self.assign(x),
            Stmt::AnnAssign(x) => self.ann_assign(x),
            _ => stmt.recurse(&mut |s| self.stmt(s)),
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }
}

fn get_numeric_type(n: &ExprNumberLiteral) -> &str {
    match n.value {
        Number::Int(_) => "builtins.int",
        Number::Float(_) => "builtins.float",
        Number::Complex { .. } => "builtins.complex",
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::module_info::build_definitions_and_classes;
    use crate::module_parser::parse_pyi;
    use crate::pyrefly::sys_info::SysInfo;
    use crate::source_map::ModuleProvider;
    use crate::test_lib::*;
    use crate::traits::SysInfoExt;

    pub fn make_bindings(module: &str, modules: &[(&str, &str)]) -> BindingsTable {
        let mod_name = ModuleName::from_str(module);
        let sources = TestSources::new(modules);
        let sys_info = SysInfo::lg_default();
        let ast_result = sources.parse(&mod_name).expect("module not found");
        let parsed_module = ast_result.as_parsed().expect("module failed to parse");
        let (definitions, _classes) = build_definitions_and_classes(parsed_module, &sys_info);
        let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &sys_info);
        BindingsTable::new(&definitions, &exports, &import_graph, parsed_module)
    }

    pub fn make_stub_bindings(module: &str, modules: &[(&str, &str)]) -> BindingsTable {
        let mod_name = ModuleName::from_str(module);
        let sources = TestSources::new(modules);
        let sys_info = SysInfo::lg_default();
        let code = sources.get_code(&mod_name).expect("module not found");
        let parsed_module = parse_pyi(code, mod_name, false);
        let (definitions, _classes) = build_definitions_and_classes(&parsed_module, &sys_info);
        let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &sys_info);
        BindingsTable::new(&definitions, &exports, &import_graph, &parsed_module)
    }

    pub fn test_instances(bt: &BindingsTable, expected: Vec<(&str, &str, &str)>) {
        for (scope, sym, typ) in expected {
            let typ = if typ.is_empty() {
                &Value::Unknown
            } else {
                &Value::Instance(ModuleName::from_str(typ))
            };
            assert_eq!(bt.lookup_str(scope, sym), Some(typ));
        }
    }

    #[test]
    fn test_as_module_name() {
        assert_eq!(None, Value::Unknown.as_module_name());
        assert_eq!(
            Some(&ModuleName::from_str("int")),
            Value::Instance(ModuleName::from_str("int")).as_module_name()
        );
    }

    #[test]
    fn test_as_str() {
        assert_eq!(
            "A",
            Alias::Local(ModuleName::from_str("test"), Name::new("A")).as_str()
        );
        assert_eq!(
            "foo.bar",
            Alias::Global(Value::Function(ModuleName::from_str("foo.bar"))).as_str()
        );
    }

    #[test]
    fn test_basic() {
        let code = r#"
A = 10
B = A + 1

def f():
    x = A
"#;
        let modules = vec![("test", code)];
        let bt = make_bindings("test", &modules);
        assert_eq!(
            bt.lookup_str("test", "A"),
            Some(&Value::Instance(ModuleName::from_str("builtins.int")))
        );
        assert_eq!(bt.lookup_str("test", "B"), Some(&Value::Unknown));
        assert_eq!(
            bt.lookup_alias_str("test.f", "x"),
            Some(&Alias::Local(ModuleName::from_str("test"), Name::new("A")))
        );
        assert_eq!(
            bt.lookup_str("test.f", "x"),
            Some(&Value::Instance(ModuleName::from_str("builtins.int")))
        );
    }

    #[test]
    fn test_import_with_alias() {
        let code = r#"
from m2 import X as xx
xx()
"#;
        let modules = vec![("test", code)];
        let bt = make_bindings("test", &modules);
        assert_eq!(
            bt.lookup_alias_str("test", "xx"),
            Some(&Alias::Global(Value::Variable(ModuleName::from_str(
                "m2.X"
            ))))
        );
    }

    #[test]
    fn test_constructor_type_inference() {
        let foo = r#"
class C:
    pass
"#;
        let test = r#"
from foo import C

class A:
    pass

a = A()
b = A(1, 2)
c = C()
"#;
        let modules = vec![("foo", foo), ("test", test)];
        let bt = make_bindings("test", &modules);
        test_instances(
            &bt,
            vec![
                ("test", "a", "test.A"),
                ("test", "b", "test.A"),
                ("test", "c", "foo.C"),
            ],
        );
    }

    #[test]
    fn test_imported_constructor() {
        let foo = r#"
class C:
    pass
"#;
        let test = r#"
from foo import C

c = C()
"#;
        let modules = vec![("foo", foo), ("test", test)];
        let bt = make_bindings("test", &modules);
        test_instances(&bt, vec![("test", "c", "foo.C")]);
    }
    #[test]
    fn test_literal_type_inference() {
        let code = r#"
a = 1.2
b = {'a': 1}
c = [x for x in range(10)]
d = f'a = {a}'
e = (1, 2, 3)
"#;
        let modules = vec![("test", code)];
        let bt = make_bindings("test", &modules);
        test_instances(
            &bt,
            vec![
                ("test", "a", "builtins.float"),
                ("test", "b", "builtins.dict"),
                ("test", "c", "builtins.list"),
                ("test", "d", "builtins.str"),
                ("test", "e", "builtins.tuple"),
            ],
        );
    }

    #[test]
    fn test_ann_assign() {
        let code = r#"
a: int = 1
b: float = 1  # annotation is ignored
"#;
        let modules = vec![("test", code)];
        let bt = make_bindings("test", &modules);
        test_instances(
            &bt,
            vec![("test", "a", "builtins.int"), ("test", "b", "builtins.int")],
        );
    }

    #[test]
    fn test_follow_aliases() {
        let code = r#"
A = 1

def f():
    x = A
    y = "hello"
    def g():
        p = x
        q = y
"#;
        let modules = vec![("test", code)];
        let bt = make_bindings("test", &modules);
        test_instances(
            &bt,
            vec![
                ("test.f", "x", "builtins.int"),
                ("test.f.g", "p", "builtins.int"),
                ("test.f.g", "q", "builtins.str"),
            ],
        );
    }

    #[test]
    fn test_global_aliases() {
        let code1 = r#"
class A:
    pass
"#;

        let code2 = r#"
    import mod1

    def f():
        x = mod1.A
"#;
        let modules = vec![("mod1", code1), ("test", code2)];

        let bt = make_bindings("test", &modules);
        let x = bt.lookup_str("test.f", "x").unwrap();
        let Value::Class(name) = x else { panic!() };
        assert_eq!(name, &ModuleName::from_str("mod1.A"));
    }

    #[test]
    fn test_global_aliases_as_types() {
        let code1 = r#"
class A:
    pass
"#;

        let code2 = r#"
    import mod1

    B = mod1.A

    def f():
        x = B()
"#;
        let modules = vec![("mod1", code1), ("test", code2)];

        let bt = make_bindings("test", &modules);
        test_instances(&bt, vec![("test.f", "x", "mod1.A")]);
    }

    #[test]
    fn test_import_aliasing() {
        let code1 = r#"
class A:
    pass

class B:
    pass
"#;
        let code2 = r#"
from mod1 import A as renamed_class
import mod1 as renamed_module
import foo

from mod1 import A as alpha, B

a = renamed_class()
b = renamed_module.A()
c = alpha()
d = B()
"#;
        let modules = vec![("mod1", code1), ("test", code2)];

        let bt = make_bindings("test", &modules);
        test_instances(
            &bt,
            vec![
                ("test", "a", "mod1.A"),
                ("test", "b", "mod1.A"),
                ("test", "c", "mod1.A"),
                ("test", "d", "mod1.B"),
            ],
        );
    }

    #[test]
    fn test_assign_single() {
        let code1 = r#"
class A:
    pass
        "#;
        let code2 = r#"
from mod1 import A
"#;
        let code3 = r#"
from mod2 import A

a = A()
"#;
        let modules = vec![("mod1", code1), ("mod2", code2), ("test", code3)];

        let bt = make_bindings("test", &modules);
        test_instances(&bt, vec![("test", "a", "mod2.A")]);
    }

    #[test]
    fn test_stub_annotated_constant() {
        let stub = r#"
class Environ:
    def get(self, key: str) -> str: ...

environ: Environ
"#;
        let modules = vec![("test", stub)];
        let bt = make_stub_bindings("test", &modules);
        test_instances(&bt, vec![("test", "environ", "test.Environ")]);
    }

    #[test]
    fn test_stub_annotated_constant_imported_type() {
        let types = r#"
class MyType:
    pass
"#;
        let stub = r#"
from types_mod import MyType

x: MyType
"#;
        let modules = vec![("types_mod", types), ("test", stub)];
        let bt = make_stub_bindings("test", &modules);
        test_instances(&bt, vec![("test", "x", "types_mod.MyType")]);
    }

    #[test]
    fn test_non_stub_ignores_annotation_without_value() {
        let code = r#"
class Foo:
    pass

x: Foo
"#;
        let modules = vec![("test", code)];
        let bt = make_bindings("test", &modules);
        assert_eq!(bt.lookup_str("test", "x"), None);
    }
}
