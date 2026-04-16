/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use ahash::AHashSet;
use pyrefly_python::module_name::ModuleName;
use pyrefly_util::visit::Visit;
use ruff_python_ast::Arguments;
use ruff_python_ast::Decorator;
use ruff_python_ast::ExceptHandler;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprAttribute;
use ruff_python_ast::ExprCall;
use ruff_python_ast::ExprCompare;
use ruff_python_ast::ExprContext;
use ruff_python_ast::ExprName;
use ruff_python_ast::ExprSubscript;
use ruff_python_ast::Identifier;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtAnnAssign;
use ruff_python_ast::StmtAssign;
use ruff_python_ast::StmtAugAssign;
use ruff_python_ast::StmtClassDef;
use ruff_python_ast::StmtDelete;
use ruff_python_ast::StmtFor;
use ruff_python_ast::StmtFunctionDef;
use ruff_python_ast::StmtIf;
use ruff_python_ast::StmtImport;
use ruff_python_ast::StmtImportFrom;
use ruff_python_ast::StmtMatch;
use ruff_python_ast::StmtRaise;
use ruff_python_ast::StmtTry;
use ruff_python_ast::StmtWhile;
use ruff_python_ast::StmtWith;
use ruff_python_ast::name::Name;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use tracing::trace;

use crate::analyzer::AnalyzedModule;
use crate::analyzer::Analyzer;
use crate::bindings::Alias;
use crate::class::FieldKind;
use crate::cursor::Block;
use crate::cursor::Cursor;
use crate::cursor::TryHandler;
use crate::effects::CallData;
use crate::effects::CallKind;
use crate::effects::Effect;
use crate::effects::EffectData;
use crate::effects::EffectKind;
use crate::exports::Attribute;
use crate::exports::Exports;
use crate::format;
use crate::imports::ImportGraph;
use crate::imports::get_import_chain_string;
use crate::manual_override;
use crate::module_effects::ModuleEffects;
use crate::module_info::ModuleInfo;
use crate::module_info::ResolvedName;
use crate::module_info::get_import_module_state_from_def;
use crate::module_parser::ParsedModule;
use crate::pyrefly::definitions::DefinitionStyle;
use crate::pyrefly::sys_info::SysInfo;
use crate::stubs::Stubs;
use crate::traits::DefinitionExt;
use crate::traits::ExprExt;
use crate::traits::ModuleNameExt;

// Main entry point for the analyzer library.
//
// Runs a side-effect analysis over a module and returns a list of effects.
pub fn analyze(
    parsed_module: &ParsedModule,
    exports: &Exports,
    import_graph: &ImportGraph,
    stubs: &Stubs,
    sys_info: &SysInfo,
) -> AnalyzedModule {
    trace!("Checking module {}", parsed_module.name.as_str());
    let analyzer = SourceAnalyzer::new(parsed_module, exports, import_graph, stubs, sys_info);
    analyzer.analyze()
}

fn mark_called_imports(output: &mut ModuleEffects) {
    let called_functions = &output.called_functions;
    for function in called_functions {
        if let Some(imports) = output.pending_imports.get(function) {
            for import in imports {
                output
                    .called_imports
                    .entry(*function)
                    .or_default()
                    .insert(*import);
            }
        }
    }
}

fn get_qualified_import(res: &ResolvedName, attribute_module: &ModuleName) -> Option<ModuleName> {
    let full_module = match &res.definition.style {
        DefinitionStyle::ImportModule(_) => *attribute_module,
        DefinitionStyle::Import(parent) | DefinitionStyle::ImportAsEq(parent) => {
            // For `from parent import name`, prepend parent to get full path
            parent.concat(attribute_module)
        }
        DefinitionStyle::ImportAs(parent, orig_name) => {
            // For `from parent import orig_name as alias`, replace alias with orig_name
            // attribute_module starts with alias (res.name), replace with parent.orig_name
            let rest = attribute_module
                .as_str()
                .strip_prefix(res.name.as_str())
                .and_then(|s| s.strip_prefix('.'));
            match rest {
                Some(r) => parent.append(orig_name).append_str(r),
                None => parent.append(orig_name),
            }
        }
        _ => return None,
    };
    Some(full_module)
}

/// Implementation of Analyzer for source files (.py).
pub struct SourceAnalyzer<'a> {
    parsed_module: &'a ParsedModule,
    info: ModuleInfo<'a>,
    import_graph: &'a ImportGraph,
    cursor: Cursor,
}

impl<'a> SourceAnalyzer<'a> {
    fn check_method(&self, func_def: &StmtFunctionDef, output: &mut ModuleEffects) {
        if func_def.name.eq("__del__") {
            let name = ModuleName::from_str(&func_def.name);
            let eff = Effect::new(EffectKind::CustomFinalizer, name, func_def.range);
            self.add_effect(eff, output);
        }
    }

    fn check_getattr(&self, args: &Arguments, output: &mut ModuleEffects) {
        let box args = &args.args;
        if args.len() < 2 {
            // Invalid getattr call
            return;
        }

        let obj = &args[0];
        let attr = &args[1];
        match attr {
            // Treat `getattr(foo, "bar")` as equivalent to `foo.bar`
            Expr::StringLiteral(x) => {
                let attr = Identifier::new(x.value.to_str(), x.range);
                self.check_attr_impl(obj, &attr, &ExprContext::Load, output)
            }
            // Treat `getattr(foo, var)` as a side effect
            _ => {
                let name = ModuleName::from_str("getattr");
                let eff = Effect::new(EffectKind::ProhibitedFunctionCall, name, attr.range());
                self.add_effect(eff, output);
            }
        }
    }

    fn check_setattr(&self, args: &Arguments, output: &mut ModuleEffects) {
        let box args = &args.args;
        if args.len() < 3 {
            // Invalid setattr call
            return;
        }

        let obj = &args[0];
        let attr = &args[1];
        match attr {
            // Treat `setattr(foo, "bar", baz)` as equivalent to `foo.bar = baz`
            Expr::StringLiteral(x) => {
                let attr = Identifier::new(x.value.to_str(), x.range);
                self.check_attr_impl(obj, &attr, &ExprContext::Store, output)
            }
            // Treat `setattr(foo, var, ...)` as a side effect
            _ => {
                let name = ModuleName::from_str("setattr");
                let eff = Effect::new(EffectKind::ProhibitedFunctionCall, name, attr.range());
                self.add_effect(eff, output);
            }
        }
        // Check obj as a potentially dangerous assignment target (even though obj.attr is the
        // actual assignment target, we are still potentially modifying obj)
        self.check_assign_target(obj, output);
    }

    fn run_body(
        &mut self,
        func_def: &StmtFunctionDef,
        output: &mut ModuleEffects,
    ) -> ModuleEffects {
        // Run the function body.
        let mut out = ModuleEffects::new();
        self.cursor.enter_function_scope(func_def);
        self.check_function_body_for_imports(func_def, output);
        self.stmts(&func_def.body, &mut out);
        self.cursor.exit_scope();
        out
    }

    fn check_function_body_for_imports(
        &self,
        func_def: &StmtFunctionDef,
        output: &mut ModuleEffects,
    ) {
        for statement in &func_def.body {
            self.check_function_statement(statement, output)
        }
    }

    fn check_function_statement(&self, x: &Stmt, output: &mut ModuleEffects) {
        match x {
            Stmt::Import(x) => {
                self.add_pending_import(x, output);
            }
            Stmt::ImportFrom(x) => {
                self.import_from(x, output);
            }
            Stmt::Expr(x) => {
                if let Expr::Attribute(e) = &*x.value {
                    let obj = &*e.value;
                    let attr = &e.attr;
                    if let Some(called_import_to_add) = self.check_function_attribute(obj, attr) {
                        output.add_called_import(called_import_to_add, &self.cursor.scope());
                    }
                }
            }
            Stmt::Assign(x) => {
                if let Expr::Attribute(e) = &*x.value {
                    let obj = &*e.value;
                    let attr = &e.attr;
                    if let Some(called_import_to_add) = self.check_function_attribute(obj, attr) {
                        output.add_called_import(called_import_to_add, &self.cursor.scope())
                    }
                }
            }
            _ => {}
        }
        x.recurse(&mut |c| self.check_function_statement(c, output));
    }

    fn check_function_attribute(&self, obj: &Expr, attr: &Identifier) -> Option<ModuleName> {
        let res = self.info.resolve(&self.cursor, obj)?;
        if res.is_import() {
            for attr_key in [Some(attr), None] {
                let attribute_module = get_import_chain_string(obj, attr_key, &res.name);
                if let Some(full_module) = get_qualified_import(&res, &attribute_module) {
                    if self.import_graph.contains(&full_module) {
                        return Some(full_module);
                    }
                }
            }
        };
        None
    }

    fn unknown_function_name(&self, func: &Expr, output: &mut ModuleEffects) {
        trace!("Unknown call: {}(...)", format::format_expr(func));
        let name = match func {
            Expr::Attribute(e) => match *e.value {
                // we don't infer return types, so foo().bar() ends up here
                Expr::Call(_) => "<chained method>",
                // probably a method on a literal or an expression
                _ => "<unknown method>",
            },
            // something like functions[i]()
            Expr::Subscript(_) => "<subscript>",
            _ => "<unknown>",
        };
        let fname = ModuleName::from_str(name);
        let eff = Effect::new(EffectKind::UnknownFunctionCall, fname, func.range());
        self.add_effect(eff, output);
    }

    fn check_call_arg(&self, arg: &Expr, output: &mut ModuleEffects) -> bool {
        let mut ret = false;
        if let Some(res) = self.info.resolve(&self.cursor, arg) {
            if res.is_import() || self.is_import_alias(arg) {
                let name = ModuleName::from_name(&res.name);
                let eff = Effect::new(EffectKind::ImportedVarArgument, name, arg.range());
                self.add_effect(eff, output);
                ret = true;
            }
        }
        ret
    }

    fn check_call_args(&self, args: &Arguments, output: &mut ModuleEffects) -> CallData {
        if args.args.len() > 64 {
            let eff = Effect::new(EffectKind::TooManyArgs, ModuleName::empty(), args.range());
            self.add_effect(eff, output);
        }

        let mut has_unsafe = false;
        let mut unsafe_indices: u64 = 0;
        let mut unsafe_keyword_names = Vec::new();
        let mut has_unsafe_kwargs_expansion = false;

        for (i, arg) in args.args.as_ref().iter().enumerate() {
            if self.check_call_arg(arg, output) {
                has_unsafe = true;
                if i < 64 {
                    unsafe_indices |= 1u64 << i;
                }
            }
        }
        for arg in args.keywords.as_ref() {
            if self.check_call_arg(&arg.value, output) {
                has_unsafe = true;
                match &arg.arg {
                    Some(ident) => {
                        unsafe_keyword_names.push(ModuleName::from_str(ident.as_str()));
                    }
                    None => {
                        // **kwargs expansion — can't determine specific keywords
                        has_unsafe_kwargs_expansion = true;
                    }
                }
            }
        }
        CallData::new(
            has_unsafe,
            unsafe_indices,
            unsafe_keyword_names,
            has_unsafe_kwargs_expansion,
        )
    }

    fn check_unresolved_call(
        &self,
        func: &Expr,
        args: Option<&Arguments>,
        output: &mut ModuleEffects,
        call_kind: Option<CallKind>,
    ) {
        let Some(fname) = func.full_name() else {
            // This is probably something like `(expr)(args)`
            self.unknown_function_name(func, output);
            return;
        };

        if let Expr::Attribute(ExprAttribute { value, .. }) = func {
            if self.check_sys_modules_access(value, func.range(), output) {
                return;
            }
        }

        let name = fname.as_str();
        let builtins = self.info.stubs.builtins();

        // If we have a name but can't resolve it, the function is likely a builtin, so don't mark
        // it as an error by default, but special-case a few builtins.
        if name == "getattr" {
            if let Some(a) = args {
                self.check_getattr(a, output);
            }
        } else if name == "setattr" {
            if let Some(a) = args {
                self.check_setattr(a, output);
            }
        } else if name == "exec" {
            let eff = Effect::new(EffectKind::ExecCall, fname, func.range());
            self.add_effect(eff, output);
        } else if let Some(eff) = builtins.call_effect(func) {
            // Prohibited builtin call (e.g. eval, open)
            self.add_effect(eff, output);
        } else if builtins.is_known_builtin(func) {
            // Known safe builtin - no effect needed
        } else {
            // This is not a builtin and we haven't resolved it, so mark it as unknown
            let eff_kind = match call_kind {
                Some(k) => k.unknown_call_effect(),
                None => EffectKind::UnknownFunctionCall,
            };
            let eff = Effect::new(eff_kind, fname, func.range());
            self.add_effect(eff, output);
        }
    }

    /// Resolves import aliases in function call names.
    /// For example, if code has `import m2 as m3` and calls `m3.f()`,
    /// this function converts the call_name from "m3.f" to "m2.f"
    /// by retrieving the alias mapping.
    fn fname_replace_import_alias(
        &self,
        call_name: &ModuleName,
        res: &ResolvedName,
    ) -> Option<ModuleName> {
        // Look up the alias for the base name in the scope the variable was
        // introduced
        let alias = self
            .info
            .bindings
            .lookup_alias(&res.scope, &res.name)
            .map(|v| v.as_str())
            .unwrap_or("");
        if alias.is_empty() {
            return None;
        }
        // Only replace the base name at the start of the call_name
        let call_name_str = call_name.as_str();
        let base_name_str = res.name.as_str();

        call_name_str
            .starts_with(base_name_str)
            .then(|| ModuleName::from_str(&call_name_str.replacen(base_name_str, alias, 1)))
    }

    fn get_method_definition_source_name(
        &self,
        attr_name: &Name,
        bindings_value: &ModuleName,
    ) -> Option<ModuleName> {
        let attr = Attribute::from_module_name(bindings_value);
        let source_name = self.info.exports.get_definition_source_name(&attr)?;
        Some(source_name.as_module_name().append(attr_name))
    }

    fn check_unpacked_call(
        &self,
        func: &Expr,
        args: Option<&Arguments>,
        range: TextRange,
        output: &mut ModuleEffects,
    ) {
        // Check the call arguments for imported variables
        let mut call_data = CallData::empty();
        if let Some(args) = args {
            call_data = self.check_call_args(args, output);
        }

        if let Expr::Attribute(ExprAttribute { value, .. }) = func {
            if self.check_sys_modules_access(value, range, output) {
                return;
            }
        }

        let Some((res, fname)) = self.resolve_function_name(func, args, output) else {
            return;
        };

        output.called_functions.insert(fname);
        let data = if call_data.has_unsafe_args() {
            EffectData::Call(Box::new(call_data))
        } else {
            EffectData::None
        };

        // Functions we have special-cased as safe.
        if manual_override::declared_safe(&fname) {
            return;
        }

        // Check if this is a method call before checking if it's a function
        if self.handle_method_call(func, &res, fname, range, data.clone(), output) {
            return;
        }

        let kind = if res.is_import() {
            EffectKind::ImportedFunctionCall
        } else {
            EffectKind::FunctionCall
        };
        let eff = Effect::with_data(kind, fname, range, data);
        self.add_effect(eff, output);
    }

    // Helper function for check_unpacked_call()
    fn resolve_function_name(
        &self,
        func: &Expr,
        args: Option<&Arguments>,
        output: &mut ModuleEffects,
    ) -> Option<(ResolvedName<'_>, ModuleName)> {
        let Some(res) = self.info.resolve(&self.cursor, func) else {
            self.check_unresolved_call(func, args, output, None);
            return None;
        };

        // If the name resolved to a builtin, delegate to check_unresolved_call
        // which has special handling for exec, getattr, setattr, etc.
        if res.scope == ModuleName::builtins() {
            self.check_unresolved_call(func, args, output, None);
            return None;
        }

        let Some(call_name) = func.full_name() else {
            // We have a base name but no full name (e.g. `x.f[i]()`)
            self.unknown_function_name(func, output);
            return None;
        };

        let fname = self
            .fname_replace_import_alias(&call_name, &res)
            .unwrap_or_else(|| res.qualify_name(&call_name));

        Some((res, fname))
    }

    // Helper function for check_unpacked_call()
    fn handle_method_call(
        &self,
        func: &Expr,
        res: &ResolvedName,
        fname: ModuleName,
        range: TextRange,
        data: EffectData,
        output: &mut ModuleEffects,
    ) -> bool {
        let Expr::Attribute(ExprAttribute { attr, .. }) = func else {
            return false;
        };

        if !matches!(
            res.definition.style,
            DefinitionStyle::Annotated(..) | DefinitionStyle::Unannotated(_)
        ) {
            return false;
        }

        // This is a method call
        let typ = self.info.bindings.get_type(&res.scope, &res.name);
        let fname = match typ {
            // If the method receiver is a type instance, we can determine the fully
            // qualified name of the method
            Some(t) => t.append(&attr.id),
            // Otherwise, we can't resolve the attr so stick with fname
            _ => fname,
        };

        // For param receivers with unknown type, check if the method is
        // known-safe across all builtin types (e.g. copy, get, index).
        // Only applies to builtins — user-defined classes with same-named
        // methods are not affected since their types aren't in the builtins stub.
        let is_safe_builtin_method = res.definition.is_param()
            && typ.is_none()
            && self.info.stubs.is_method_safe_in_builtins(&attr.id);

        if !is_safe_builtin_method {
            let eff = Effect::with_data(EffectKind::MethodCall, fname, range, data);
            self.add_effect(eff, output);
        }

        self.check_indirectly_called_method(fname, res, attr, output);

        if res.definition.is_param() && !is_safe_builtin_method {
            let is_mutating = match typ {
                Some(t) => self.may_mutate_receiver(t, &attr.id),
                None => true,
            };
            if is_mutating {
                let param_name = ModuleName::from_name(&res.name);
                let eff = Effect::new(EffectKind::ParamMethodCall, param_name, range);
                self.add_param_effect(res, eff, output);
            }
        };

        // Calling a mutating method (e.g. list.append) on a module-level variable from a
        // nested scope is a global variable mutation.
        let is_global_receiver = res.is_global()
            || (res.scope == self.info.module_name && self.cursor.scope() != self.info.module_name);
        if is_global_receiver
            && let Some(t) = typ
            && self.may_mutate_receiver(t, &attr.id)
        {
            let name = ModuleName::from_name(&res.name);
            let eff = Effect::new(EffectKind::GlobalVarMutation, name, range);
            self.add_effect(eff, output);
        }

        true
    }

    /// Check whether a method call may mutate its receiver. For stub-defined types,
    /// we trust the annotation: only `mutation()` methods return true. For types not
    /// in stubs (project-defined classes), we conservatively assume mutation.
    fn may_mutate_receiver(&self, typ: &ModuleName, method_name: &Name) -> bool {
        let method_fqn = typ.append(method_name);
        let stub_module = typ.parent().unwrap_or(*typ);
        let Some(stub) = self.info.stubs.get(&stub_module) else {
            return true;
        };
        stub.module_effects
            .effects
            .get(&method_fqn)
            .is_some_and(|effects| effects.iter().any(|e| e.kind == EffectKind::Mutation))
    }

    fn check_indirectly_called_method(
        &self,
        fname: ModuleName,
        res: &ResolvedName,
        attr: &Identifier,
        output: &mut ModuleEffects,
    ) {
        if !output.indirectly_called_methods.contains_key(&fname)
            && let Some(bindings_name) = self.info.bindings.lookup(&res.scope, &res.name)
            && let Some(source_name) = bindings_name
                .as_module_name()
                .and_then(|m| self.get_method_definition_source_name(&attr.id, m))
        {
            output.indirectly_called_methods.insert(fname, source_name);
        }
    }

    fn check_call(&self, call: &ExprCall, output: &mut ModuleEffects) {
        let box func = &call.func;
        if !self.check_call_for_import(call, output) {
            self.check_unpacked_call(func, Some(&call.arguments), call.range(), output);
        }
    }

    fn check_call_for_import(&self, call: &ExprCall, output: &mut ModuleEffects) -> bool {
        let import_module_state = get_import_module_state_from_def(
            &self.info.definitions.definitions,
            &self.cursor.scope(),
        );
        if let Some(module_name) = import_module_state.match_call(call) {
            output.add_pending_import(module_name, &self.cursor.scope());
            return true;
        }
        false
    }

    fn check_attr_impl(
        &self,
        obj: &Expr,
        attr: &Identifier,
        ctx: &ExprContext,
        output: &mut ModuleEffects,
    ) {
        let Some(res) = self.info.resolve(&self.cursor, obj) else {
            if let Some(fname) = obj.full_name() {
                let eff = Effect::new(EffectKind::UnknownObject, fname, obj.range());
                self.add_effect(eff, output);
            }
            return;
        };

        if let Some(typ) = self.info.bindings.get_type(&res.scope, &res.name) {
            if let Some(cls) = self.info.classes.lookup(typ) {
                // We have an attribute call on an instance of a class in this module, so we can
                // retrieve the class fields and check if `attr` is a property.
                if let Some(field) = cls.get_field(&attr.id) {
                    if matches!(field.kind, FieldKind::Property) {
                        let fname = res.qualify_name(&cls.name.append(&attr.id));
                        let eff = Effect::new(EffectKind::MethodCall, fname, obj.range());
                        self.add_effect(eff, output);
                    }
                }
            } else if self.info.exports.is_class(typ) {
                // typ is a class but isn't in the current module's ClassTable, so mark this as an
                // imported attr access, and we will check in project.rs if it is a property.
                if !typ.as_str().starts_with("builtins.") {
                    let fname = typ.append(&attr.id);
                    let eff = Effect::new(EffectKind::ImportedTypeAttr, fname, obj.range());
                    self.add_effect(eff, output);
                }
            }
        }

        if res.is_import() {
            let name = ModuleName::from_name(&res.name);
            let attribute_module = get_import_chain_string(obj, Some(attr), &res.name);

            // Compute the full module path and add to called imports if it differs from base
            if let Some(full_module) = get_qualified_import(&res, &attribute_module) {
                let imported_module = match &res.definition.style {
                    DefinitionStyle::ImportModule(m) => *m,
                    DefinitionStyle::Import(parent) | DefinitionStyle::ImportAsEq(parent) => {
                        parent.append(&res.name)
                    }
                    DefinitionStyle::ImportAs(parent, orig_name) => parent.append(orig_name),
                    _ => panic!("Unexpected definition style: {:?}", res.definition.style),
                };

                if imported_module != full_module {
                    output.add_called_import(full_module, &self.cursor.scope());
                }
            }

            // Check for store context (mutation) regardless of whether we could resolve the full module
            if *ctx == ExprContext::Store {
                let eff = Effect::new(EffectKind::ImportedVarMutation, name, obj.range());
                self.add_effect(eff, output);
            }
        };
    }

    fn check_attr(&self, e: &ExprAttribute, output: &mut ModuleEffects) {
        let box obj = &e.value;
        self.check_attr_impl(obj, &e.attr, &e.ctx, output);
    }

    fn is_import_alias(&self, obj: &Expr) -> bool {
        // See if this is an alias to an imported variable
        if let Some(name) = obj.as_var_name() {
            let alias = self.info.bindings.lookup_alias(&self.cursor.scope(), &name);
            if matches!(alias, Some(Alias::Global(_))) {
                return true;
            }
        }
        false
    }

    fn is_sys_modules_access(&self, expr: &Expr) -> bool {
        if let Expr::Attribute(ExprAttribute { value, attr, .. }) = expr {
            if attr.as_str() == "modules" {
                if let Some(res) = self.info.resolve(&self.cursor, value) {
                    return res.is_import() && res.name.as_str() == "sys";
                }
            }
        }
        false
    }

    fn check_sys_modules_access(
        &self,
        expr: &Expr,
        range: TextRange,
        output: &mut ModuleEffects,
    ) -> bool {
        if self.is_sys_modules_access(expr) {
            let name = ModuleName::from_str("sys.modules");
            let eff = Effect::new(EffectKind::SysModulesAccess, name, range);
            self.add_effect(eff, output);
            return true;
        }
        false
    }

    fn check_subscript(&self, e: &ExprSubscript, output: &mut ModuleEffects) {
        if self.check_sys_modules_access(&e.value, e.range(), output) {
            return;
        }

        // TODO: Add an Effect for accessing the subscript even if we are not assigning to it;
        // we need to cross-check this with whether a side-effecting `__getitem__` is defined
        // on the type of the variable. There is not much point doing this until we have better
        // type information though.
        if e.ctx == ExprContext::Load {
            return;
        };

        let box obj = &e.value;

        let Some(res) = self.info.resolve(&self.cursor, obj) else {
            return;
        };

        if res.is_import() || self.is_import_alias(obj) {
            let name = ModuleName::from_name(&res.name);
            let eff = Effect::new(EffectKind::ImportedVarMutation, name, obj.range());
            self.add_effect(eff, output);
        }
        if res.definition.is_param() {
            let name = ModuleName::from_name(&res.name);
            let eff = Effect::new(EffectKind::ParamMethodCall, name, obj.range());
            self.add_param_effect(&res, eff, output);
        };
    }

    fn check_compare(&self, e: &ExprCompare, output: &mut ModuleEffects) {
        let box left = &e.left;
        let box comparators = &e.comparators;

        let mut check_and_add_effect = |expr: &Expr| {
            if let Some(res) = self.info.resolve(&self.cursor, expr) {
                if res.is_import() {
                    let name = ModuleName::from_name(&res.name);
                    let eff = Effect::new(EffectKind::UnknownValueBinaryOp, name, left.range());
                    self.add_effect(eff, output);
                }
            }
        };

        check_and_add_effect(left);

        for right in comparators {
            check_and_add_effect(right);
        }
    }

    fn check_name(&self, e: &ExprName, output: &mut ModuleEffects) {
        let name = ModuleName::from_name(&e.id);
        if output.all_pending_import_names.contains(&name) {
            output.add_called_import(name, &self.cursor.scope());
        }
    }

    fn expr(&self, x: &Expr, output: &mut ModuleEffects) {
        match x {
            Expr::Call(call) => self.check_call(call, output),
            Expr::Attribute(e) => self.check_attr(e, output),
            Expr::Subscript(e) => self.check_subscript(e, output),
            Expr::Compare(e) => self.check_compare(e, output),
            Expr::Name(e) => self.check_name(e, output),
            _ => (),
        }
        x.recurse(&mut |c| self.expr(c, output));
    }

    fn check_assign_target(&self, target: &Expr, output: &mut ModuleEffects) {
        if let Expr::Subscript(e) = target {
            if self.check_sys_modules_access(&e.value, e.range(), output) {
                return;
            }
        }
        let Some(res) = self.info.resolve(&self.cursor, target) else {
            return;
        };
        let name = ModuleName::from_name(&res.name);
        if res.is_import() {
            let eff = Effect::new(EffectKind::ImportedVarMutation, name, target.range());
            self.add_effect(eff, output);
        } else if res.is_global() {
            let eff = Effect::new(EffectKind::GlobalVarAssign, name, target.range());
            self.add_effect(eff, output);
        } else if res.scope == self.info.module_name && res.scope != self.cursor.scope() {
            // Catch things like subscript and attr assignment of a module variable by checking if the
            // base name of the lhs has been resolved in module scope while the cursor is in a
            // nested scope.
            let eff = Effect::new(EffectKind::GlobalVarMutation, name, target.range());
            self.add_effect(eff, output);
        } else {
            match target {
                Expr::Subscript(e) => self.check_subscript(e, output),
                Expr::Attribute(_) if res.definition.is_param() => {
                    let eff = Effect::new(EffectKind::ParamMethodCall, name, target.range());
                    self.add_param_effect(&res, eff, output);
                }
                _ => {}
            }
        }
    }

    fn assign(&self, x: &StmtAssign, output: &mut ModuleEffects) {
        for target in &x.targets {
            // if the value is an import_module call don't treat it as a regular assign
            if !self.check_assign_to_import_module(target, &x.value, output) {
                self.check_assign_target(target, output);
            }
        }
        // only check toplevel constants for re-exports
        if self.cursor.scope() == self.info.module_name {
            if let Expr::Name(name) = &x.targets[0] {
                let exported_name = Attribute::new(self.info.module_name, &name.id);
                self.check_re_exports(&exported_name, x.range, output);
            };
        }

        self.expr(&x.value, output);
    }

    fn ann_assign(&self, x: &StmtAnnAssign, output: &mut ModuleEffects) {
        // We don't check the annotation since it is unlikely it can cause unsafe behaviour, and
        // checking for corner cases like `x: T[S]` triggering a custom `__getitem__` runs a higher
        // risk of false positives with low chance of actual benefits.
        self.check_assign_target(&x.target, output);
        if let Some(val) = &x.value {
            self.expr(val, output);
        }
    }

    fn aug_assign(&self, x: &StmtAugAssign, output: &mut ModuleEffects) {
        // TODO: In the case
        //     from foo import x
        //     x += 1
        // the Definitions table will always mark x as Local, but if x overrides __iadd__ the
        // assignment will modify foo.x and should therefore be marked unsafe.
        self.check_assign_target(&x.target, output);
        self.expr(&x.value, output);
    }

    fn delete(&self, x: &StmtDelete, output: &mut ModuleEffects) {
        for target in &x.targets {
            match target {
                Expr::Subscript(_) | Expr::Attribute(_) => {
                    self.check_assign_target(target, output);
                }
                _ => {}
            }
        }
    }

    fn check_decorators(&self, decs: &Vec<Decorator>, output: &mut ModuleEffects) {
        // Treat a decorator as a call. If the decorator does not have explicit call syntax,
        // treat it as a call with no arguments.
        for dec in decs {
            let mut call = &dec.expression;
            let mut args = None;
            if let Expr::Call(func) = call {
                args = Some(&func.arguments);
                call = &func.func;
            }
            let Some(res) = self.info.resolve(&self.cursor, call) else {
                self.check_unresolved_call(call, args, output, Some(CallKind::Decorator));
                continue;
            };
            let Some(call_name) = call.full_name() else {
                // Handle cases like @decorators[0] where the decorator is accessed via subscript.
                // We can't statically determine the decorator, so mark it as unknown.
                let name = match call {
                    Expr::Subscript(_) => "<subscript>",
                    _ => "<unknown>",
                };
                let fname = ModuleName::from_str(name);
                let eff = Effect::new(EffectKind::UnknownDecoratorCall, fname, dec.range);
                self.add_effect(eff, output);
                continue;
            };
            let fname = self
                .fname_replace_import_alias(&call_name, &res)
                .unwrap_or_else(|| res.qualify_name(&call_name));

            if manual_override::declared_safe(&fname) {
                continue;
            }

            // Add decorator to called_functions so its nested imports are tracked
            output.called_functions.insert(fname);

            let kind = if res.is_import() {
                EffectKind::ImportedDecoratorCall
            } else {
                EffectKind::DecoratorCall
            };
            // For parameterized decorators like @register("name"), the returned
            // function is called by Python's decorator machinery, so we attach
            // CallData to signal that nested function effects should be checked.
            let eff = if args.is_some() {
                Effect::with_data(
                    kind,
                    fname,
                    dec.range,
                    EffectData::Call(Box::new(CallData::empty())),
                )
            } else {
                Effect::new(kind, fname, dec.range)
            };
            self.add_effect(eff, output);
        }
    }

    fn class_def(&mut self, x: &StmtClassDef, output: &mut ModuleEffects) {
        self.check_decorators(&x.decorator_list, output);
        self.cursor.enter_class_scope(x);
        let exported_name = Attribute::new(self.info.module_name, &x.name.id);
        self.check_re_exports(&exported_name, x.range, output);
        for body_stmt in &x.body {
            if let Stmt::FunctionDef(func_def) = body_stmt {
                self.check_method(func_def, output);
            }
            self.stmt(body_stmt, output);
        }
        self.cursor.exit_scope();
    }

    fn function_def(&mut self, x: &StmtFunctionDef, output: &mut ModuleEffects) {
        self.check_decorators(&x.decorator_list, output);
        // only check toplevel functions
        if self.cursor.scope() == self.info.module_name {
            let exported_name = Attribute::new(self.info.module_name, &x.name.id);
            self.check_re_exports(&exported_name, x.range, output);
        }
        // Analyze the function body and record the side effects
        trace!(
            "Checking body of {} [{:?}]",
            x.name.id.as_str(),
            x.range.start()
        );
        let out = self.run_body(x, output);
        output.effects.merge(&out.effects);
    }

    /// Extract the exception type name from a raise expression.
    /// Handles both `raise ValueError` (ExprName) and `raise ValueError("msg")` (ExprCall).
    fn exception_name(exc: &Expr) -> Option<ModuleName> {
        match exc {
            Expr::Call(call) => call.func.full_name(),
            other => other.full_name(),
        }
    }

    fn raise(&self, x: &StmtRaise, output: &mut ModuleEffects) {
        let unknown = ModuleName::from_str("<unknown exception>");
        let name = match &x.exc {
            Some(exc) => Self::exception_name(exc.as_ref()).unwrap_or(unknown),
            // Bare `raise` (re-raise): we can't determine the type, so treat it as caught if
            // we're inside any try body at all.
            None => {
                if self.cursor.in_try_body() {
                    return;
                }
                unknown
            }
        };
        if self.cursor.catches_exception(&name) {
            return;
        }
        let eff = Effect::new(EffectKind::Raise, name, x.range());
        self.add_effect(eff, output);
    }

    fn extract_try_handlers(handlers: &[ExceptHandler]) -> Vec<TryHandler> {
        handlers
            .iter()
            .map(|ExceptHandler::ExceptHandler(e)| match &e.type_ {
                None => TryHandler::Bare,
                Some(typ) => {
                    let names = match typ.as_ref() {
                        Expr::Tuple(t) => t.elts.iter().filter_map(|elt| elt.full_name()).collect(),
                        expr => expr.full_name().into_iter().collect(),
                    };
                    TryHandler::typed(names)
                }
            })
            .collect()
    }

    fn try_(&mut self, x: &StmtTry, output: &mut ModuleEffects) {
        let handlers = Self::extract_try_handlers(&x.handlers);
        self.cursor.enter_block(Block::TryBody(handlers));
        self.stmts_with_called_imports(&x.body, output);
        self.cursor.leave_block();
        for ExceptHandler::ExceptHandler(e) in &x.handlers {
            self.stmts(&e.body, output);
        }
        self.stmts(&x.orelse, output);
        self.stmts(&x.finalbody, output);
    }

    fn if_(&mut self, x: &StmtIf, output: &mut ModuleEffects) {
        for (test, body) in self.info.sys_info.pruned_if_branches(x) {
            if let Some(test) = test {
                self.expr(test, output);
            }
            self.stmts(body, output);
        }
    }

    fn while_(&self, x: &StmtWhile, output: &mut ModuleEffects) {
        // Body handled by stmt()
        self.expr(&x.test, output);
    }

    fn for_(&self, x: &StmtFor, output: &mut ModuleEffects) {
        // Body handled by stmt()
        self.expr(&x.target, output);
        self.expr(&x.iter, output);
    }

    fn with(&mut self, x: &StmtWith, output: &mut ModuleEffects) {
        for item in &x.items {
            self.expr(&item.context_expr, output);
        }
        self.stmts_with_called_imports(&x.body, output);
    }

    fn match_(&self, x: &StmtMatch, output: &mut ModuleEffects) {
        // Body handled by stmt()
        self.expr(&x.subject, output);
    }

    /// Detect submodule re-exports: when `from X import Y` at module/class scope
    /// in an __init__.py where X is a submodule of the current package and Y
    /// is in __all__. Under lazy imports, `from M import Y` can resolve Y to
    /// the submodule X instead of the re-exported attribute.
    fn check_submodule_re_exports(&self, x: &StmtImportFrom, output: &mut ModuleEffects) {
        if !self.cursor.in_eager_scope() || !self.parsed_module.is_init {
            return;
        }
        let Some(m) = self.info.module_name.new_maybe_relative(
            self.parsed_module.is_init,
            x.level,
            x.module.as_ref().map(|x| &x.id),
        ) else {
            return;
        };
        if !self.import_graph.contains(&m) || m.parent() != Some(self.info.module_name) {
            return;
        }
        let Some(all_names) = self.info.exports.get_all(&self.info.module_name) else {
            return;
        };
        for name in &x.names {
            if &name.name != "*" && all_names.contains(&name.name.id) {
                let eff = Effect::new(EffectKind::SubmoduleReExport, m, x.range);
                self.add_effect(eff, output);
            }
        }
    }

    fn import_from(&self, x: &StmtImportFrom, output: &mut ModuleEffects) {
        self.add_import_from(x, output, false);

        self.check_submodule_re_exports(x, output);

        for name in &x.names {
            if &name.name == "*" {
                self.import_star(x, output);
                continue;
            }

            let as_name = match &name.asname {
                None => &name.name.id,
                Some(asname) => &asname.id,
            };
            let exported_name = Attribute::new(self.info.module_name, as_name);

            // if there are multiple imports that re-export into the same name,
            // we don't want to add the effect to the first import statement
            if self
                .info
                .exports
                .get_re_export(&exported_name)
                .is_some_and(|(v, r)| {
                    v.module == ModuleName::empty() && v.attr.is_empty() && r != &x.range
                })
            {
                let eff = Effect::new(
                    EffectKind::ImportedVarReassignment,
                    exported_name.as_module_name(),
                    x.range,
                );
                self.add_effect(eff, output);
            }
        }
    }

    fn import_star(&self, x: &StmtImportFrom, output: &mut ModuleEffects) {
        let Some(m) = self.info.module_name.new_maybe_relative(
            self.parsed_module.is_init,
            x.level,
            x.module.as_ref().map(|x| &x.id),
        ) else {
            return;
        };
        let Some(all_names) = self.info.exports.get_all(&m) else {
            return;
        };
        for name in all_names {
            let exported_name = Attribute::new(self.info.module_name, name);
            if self
                .info
                .exports
                .get_re_export(&exported_name)
                .is_some_and(|(v, r)| {
                    v.module == ModuleName::empty() && v.attr.is_empty() && r != &x.range
                })
            {
                let eff = Effect::new(
                    EffectKind::ImportedVarReassignment,
                    exported_name.as_module_name(),
                    x.range,
                );
                self.add_effect(eff, output);
            }
        }
    }

    fn add_pending_import(&self, x: &StmtImport, output: &mut ModuleEffects) {
        for name in &x.names {
            let to = ModuleName::from_name(&name.name.id);
            match &name.asname {
                None => output.add_pending_import(to, &self.cursor.scope()),
                Some(asname) => {
                    let as_name_module = ModuleName::from_name(&asname.id.clone());
                    output.add_pending_import(as_name_module, &self.cursor.scope());
                    // add a pending import that maps the alias to the actual import
                    // we'll need this to check if imports are loaded
                    output.add_pending_import(to, &as_name_module);
                }
            }
        }
    }

    fn add_called_import(&self, x: &StmtImport, output: &mut ModuleEffects) {
        for name in &x.names {
            let name_id = match &name.asname {
                None => &name.name.id,
                Some(asname) => &asname.id,
            };
            output.add_called_import(ModuleName::from_name(name_id), &self.cursor.scope());
        }
    }

    fn add_import_from(&self, x: &StmtImportFrom, output: &mut ModuleEffects, is_called: bool) {
        if let Some(m) = self.info.module_name.new_maybe_relative(
            self.parsed_module.is_init,
            x.level,
            x.module.as_ref().map(|x| &x.id),
        ) {
            let scope = self.cursor.scope();

            // `from x import y` adds `x` as a dependency
            if m.as_str() != "" {
                if is_called {
                    output.add_called_import(m, &scope);
                } else {
                    output.add_pending_import(m, &scope);
                }
            }
            for name in &x.names {
                if &name.name == "*" {
                    self.add_star_import_from(&m, &scope, output, is_called);
                } else {
                    let name = &name.name.id;
                    let maybe_sub = if m.as_str() == "" {
                        ModuleName::from_str(name)
                    } else {
                        m.append(name)
                    };
                    if self.import_graph.contains(&maybe_sub)
                        || self
                            .import_graph
                            .has_missing_import(&self.info.module_name, &m)
                    {
                        if is_called {
                            output.add_called_import(maybe_sub, &scope);
                        } else {
                            output.add_pending_import(maybe_sub, &scope);
                        }
                    }
                }
            }
        }
    }

    fn add_star_import_from(
        &self,
        m: &ModuleName,
        scope: &ModuleName,
        output: &mut ModuleEffects,
        is_called: bool,
    ) {
        let Some(all_names) = self.info.exports.get_all(m) else {
            return;
        };
        for name in all_names {
            let maybe_sub = if m.as_str() == "" {
                ModuleName::from_str(name)
            } else {
                m.append(name)
            };
            if self.import_graph.contains(&maybe_sub)
                || self
                    .import_graph
                    .has_missing_import(&self.info.module_name, m)
            {
                if is_called {
                    output.add_called_import(maybe_sub, scope);
                } else {
                    output.add_pending_import(maybe_sub, scope);
                }
            }
        }
    }

    fn check_re_exports(
        &self,
        exported_name: &Attribute,
        range: TextRange,
        output: &mut ModuleEffects,
    ) {
        if self.info.exports.is_re_export(exported_name) {
            let eff = Effect::new(
                EffectKind::ImportedVarReassignment,
                exported_name.as_module_name(),
                range,
            );
            self.add_effect(eff, output);
        }
    }

    fn check_assign_to_import_module(
        &self,
        target: &Expr,
        value: &Expr,
        output: &mut ModuleEffects,
    ) -> bool {
        // import_module call assignments are treated as import as
        let Expr::Call(call) = value else {
            return false;
        };
        let Some(var_name) = target.as_var_name() else {
            return false;
        };

        let import_module_state = get_import_module_state_from_def(
            &self.info.definitions.definitions,
            &self.cursor.scope(),
        );
        if let Some(as_name) = import_module_state.match_call(call) {
            output.add_pending_import(ModuleName::from_name(&var_name), &self.cursor.scope());
            output.add_pending_import(as_name, &self.cursor.scope());
            // adding the as_name as a called import since it's an assign statement and therefore called
            output.add_called_import(as_name, &self.cursor.scope());
            return true;
        }
        false
    }

    fn stmt(&mut self, x: &Stmt, output: &mut ModuleEffects) {
        match x {
            Stmt::Assign(a) => self.assign(a, output),
            Stmt::AnnAssign(a) => self.ann_assign(a, output),
            Stmt::AugAssign(a) => self.aug_assign(a, output),
            Stmt::Delete(d) => self.delete(d, output),
            Stmt::Expr(e) => self.expr(&e.value, output),
            Stmt::ClassDef(c) => self.class_def(c, output),
            Stmt::FunctionDef(f) => self.function_def(f, output),
            Stmt::Raise(e) => self.raise(e, output),
            Stmt::Try(e) => self.try_(e, output),
            Stmt::If(e) => self.if_(e, output),
            Stmt::For(e) => self.for_(e, output),
            Stmt::While(e) => self.while_(e, output),
            Stmt::With(e) => self.with(e, output),
            Stmt::Match(e) => self.match_(e, output),
            Stmt::Import(e) => self.add_pending_import(e, output),
            Stmt::ImportFrom(e) => self.import_from(e, output),
            _ => {}
        }
        // Recurse into the bodies of block constructs we don't handle specially
        if matches!(x, Stmt::For(_) | Stmt::While(_) | Stmt::Match(_)) {
            x.recurse(&mut |s| self.stmt(s, output))
        }
    }

    fn stmts(&mut self, xs: &[Stmt], output: &mut ModuleEffects) {
        for x in xs {
            self.stmt(x, output);
        }
    }

    /// Process a list of statements, marking any imports as called.
    /// Used for blocks whose bodies execute eagerly (try, with).
    fn stmts_with_called_imports(&mut self, xs: &[Stmt], output: &mut ModuleEffects) {
        for body_stmt in xs {
            match body_stmt {
                Stmt::Import(e) => {
                    self.add_called_import(e, output);
                    self.add_pending_import(e, output);
                }
                Stmt::ImportFrom(e) => {
                    self.import_from(e, output);
                    self.add_import_from(e, output, true);
                }
                _ => {}
            }
            self.stmt(body_stmt, output);
        }
    }

    fn add_effect(&self, eff: Effect, output: &mut ModuleEffects) {
        let eff = if eff.kind.is_runnable() && self.cursor.in_try_body() {
            eff.with_try_handlers(self.cursor.try_handlers())
        } else {
            eff
        };
        output.add_effect(self.cursor.scope(), eff);
    }

    /// Record a ParamMethodCall effect in the scope where the parameter was defined.
    /// When a nested function mutates a captured parameter from an enclosing
    /// function, the effect must be attributed to the enclosing function's scope
    /// so that check_call_params can match the param name to the function's
    /// parameter list.
    fn add_param_effect(&self, res: &ResolvedName, eff: Effect, output: &mut ModuleEffects) {
        output.add_effect(res.scope, eff);
    }
}

impl<'a> Analyzer<'a> for SourceAnalyzer<'a> {
    fn new(
        parsed_module: &'a ParsedModule,
        exports: &'a Exports,
        import_graph: &'a ImportGraph,
        stubs: &'a Stubs,
        sys_info: &'a SysInfo,
    ) -> Self {
        let info = ModuleInfo::new(parsed_module, exports, import_graph, stubs, sys_info);
        Self {
            parsed_module,
            info,
            import_graph,
            cursor: Cursor::new(),
        }
    }

    fn analyze(mut self) -> AnalyzedModule {
        let mut output = ModuleEffects::new();
        self.cursor.enter_module_scope(&self.info.module_name);
        self.stmts(&self.parsed_module.ast.body, &mut output);
        mark_called_imports(&mut output);
        AnalyzedModule {
            module_effects: output,
            definitions: self.info.definitions,
            classes: self.info.classes,
            implicit_imports: AHashSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::effects::EffectTable;
    use crate::module_parser::parse_source;
    use crate::test_lib::assert_str_keys;
    use crate::test_lib::check_imports;
    use crate::test_lib::run_module_analysis;

    pub fn run_analysis(code: &str) -> ModuleEffects {
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        run_module_analysis(code, &parsed_module)
    }

    fn assert_keys(effs: &EffectTable, keys: Vec<&str>) {
        assert_str_keys(effs.keys(), keys);
    }

    #[test]
    fn test_effect_details() {
        let code = r#"
import os
os.path.join("a", "b")
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        let eff = effs.get(&ModuleName::from_str("test"));
        assert!(eff.is_some());
        let e: Vec<&Effect> = eff.unwrap().iter().collect();
        let e = e[0];
        assert!(matches!(e.kind, EffectKind::ImportedFunctionCall));
        assert_eq!(e.name.as_str(), "os.path.join");
        assert_eq!(e.range.start(), 11.into());
        assert_eq!(e.range.end(), 33.into());
    }

    #[test]
    fn test_effect() {
        let code = r#"
import a
a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        let eff = effs.get(&ModuleName::from_str("test"));
        assert!(eff.is_some());
        let e: Vec<&Effect> = eff.unwrap().iter().collect();
        assert!(matches!(e[0].kind, EffectKind::ImportedVarMutation));
    }

    #[test]
    fn test_effect_in_function_body() {
        let code = r#"
import a
def f():
    a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        let eff = effs.get(&ModuleName::from_str("test.f"));
        assert!(eff.is_some());
        let e: Vec<&Effect> = eff.unwrap().iter().collect();
        assert!(matches!(e[0].kind, EffectKind::ImportedVarMutation));
        // We should not have any module-level effects
        let module_eff = effs.get(&ModuleName::from_str("test"));
        assert!(module_eff.is_none());
    }

    #[test]
    fn test_effect_in_class_body() {
        let code = r#"
import a
class A:
    a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test.A"]);
    }

    #[test]
    fn test_effect_in_method_body() {
        let code = r#"
import a
class A:
    def f():
        a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test.A.f"]);
    }

    #[test]
    fn test_effects_in_multiple_scopes() {
        let code = r#"
import a
a.x = 10
class A:
    a.x = 10
    def f():
        a.x = 10
    def g():
        a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test", "test.A", "test.A.f", "test.A.g"]);
    }

    #[test]
    fn test_libfb_lazy_import() {
        let code = r#"
from libfb.py.lazy_import import lazy_import

foo = lazy_import("foo")
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec![]);
    }

    #[test]
    fn test_libfb_lazy_import_qualified() {
        let code = r#"
import libfb.py.lazy_import

foo = libfb.py.lazy_import.lazy_import("foo")
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec![]);
    }

    #[test]
    fn test_libfb_lazy_import_aliased() {
        let code = r#"
from libfb.py.lazy_import import lazy_import as cool_import

foo = cool_import("foo")
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec![]);
    }

    #[test]
    fn test_pytest_mark() {
        let code = r#"
import pytest

@pytest.mark.parametrize
def test_foo():
    pass
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec![]);
    }

    #[test]
    fn test_called_and_pending_imports() {
        let code = r#"
import foo
import foo.bar
import foo.bar.baz

def f():
    import baz
    foo.bar

def g():
    import bar
    foo.bar.baz

g()
        "#;
        let out = run_analysis(code);
        check_imports(
            out,
            vec![
                ("test", vec!["foo", "foo.bar", "foo.bar.baz"]),
                ("test.f", vec!["baz"]),
                ("test.g", vec!["bar"]),
            ],
            // foo.bar.baz doesn't exist so it's not listed as "called"
            vec![("test.g", vec!["bar"])],
        );
    }

    #[test]
    fn test_from_import_attribute_access_adds_called_import() {
        // Test that accessing attributes on a `from foo import bar` binding
        // correctly adds the full module path to called_imports
        let code = r#"
import foo.bar.baz  # Add to import graph so it's recognized
from foo import bar
bar.baz  # Should add foo.bar.baz as a called import
        "#;
        let out = run_analysis(code);
        check_imports(
            out,
            vec![("test", vec!["foo", "foo.bar", "foo.bar.baz"])],
            vec![("test", vec!["foo.bar.baz"])],
        );
    }

    #[test]
    fn test_from_import_with_alias_attribute_access() {
        // Test that accessing attributes on an aliased `from foo import bar as b`
        // correctly adds the full module path to called_imports
        let code = r#"
import foo.bar.baz  # Add to import graph so it's recognized
from foo import bar as b
b.baz  # Should add foo.bar.baz as a called import
        "#;
        let out = run_analysis(code);
        check_imports(
            out,
            vec![("test", vec!["foo", "foo.bar", "foo.bar.baz"])],
            vec![("test", vec!["foo.bar.baz"])],
        );
    }

    #[test]
    fn test_from_import_nested_attribute_access() {
        // Test nested attribute access on from imports
        let code = r#"
import foo.bar
import foo.bar.baz
import foo.bar.baz.quux
from foo import bar
bar.baz.quux  # Should add foo.bar.baz and foo.bar.baz.quux as called imports
        "#;
        let out = run_analysis(code);
        check_imports(
            out,
            vec![(
                "test",
                vec!["foo", "foo.bar", "foo.bar.baz", "foo.bar.baz.quux"],
            )],
            // Nested attribute access adds both intermediate and final modules
            vec![("test", vec!["foo.bar.baz", "foo.bar.baz.quux"])],
        );
    }

    #[test]
    fn test_import_vs_from_import_attribute_access() {
        // Compare behavior of `import foo` vs `from baz import quux`
        // Both should track attribute access to submodules correctly
        let code = r#"
import foo
import foo.bar
import foo.bar.baz
from baz import quux
import baz.quux
import baz.quux.sub

foo.bar.baz  # import: adds foo.bar.baz
quux.sub  # from import: should add baz.quux.sub
        "#;
        let out = run_analysis(code);
        // Focus on verifying called_imports behavior for both import styles
        let called = out.called_imports.get(&ModuleName::from_str("test"));
        assert!(called.is_some(), "Expected called imports for test scope");
        let called_set = called.unwrap();
        assert!(
            called_set.contains(&ModuleName::from_str("foo.bar.baz")),
            "Expected foo.bar.baz in called imports for `import foo; foo.bar.baz`"
        );
        assert!(
            called_set.contains(&ModuleName::from_str("baz.quux.sub")),
            "Expected baz.quux.sub in called imports for `from baz import quux; quux.sub`"
        );
    }
}
