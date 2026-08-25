/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

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
use ruff_python_ast::ExprLambda;
use ruff_python_ast::ExprSubscript;
use ruff_python_ast::Identifier;
use ruff_python_ast::Parameters;
use ruff_python_ast::PySourceType;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtAnnAssign;
use ruff_python_ast::StmtAssert;
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
use ruff_python_parser::ParseOptions;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use tracing::trace;

use crate::analyzer::AnalyzedModule;
use crate::analyzer::Analyzer;
use crate::bindings::Alias;
use crate::bindings::QualifiedGlobalAlias;
use crate::bindings::Value;
use crate::class::FieldKind;
use crate::config::AnalysisConfig;
use crate::cursor::Block;
use crate::cursor::Cursor;
use crate::cursor::TryHandler;
use crate::effects::ArgSlot;
use crate::effects::CallData;
use crate::effects::CallKind;
use crate::effects::Effect;
use crate::effects::EffectData;
use crate::effects::EffectKind;
use crate::effects::MAX_ARGS;
use crate::exports::Attribute;
use crate::exports::Exports;
use crate::format;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::imports::get_import_chain_string;
use crate::manual_override;
use crate::module_effects::ModuleEffects;
use crate::module_info::ModuleInfo;
use crate::module_info::ResolvedName;
use crate::module_info::get_import_module_state_from_def;
use crate::module_parser::ParsedModule;
use crate::pyrefly::definitions::DefinitionStyle;
use crate::runner::to_ruff_version;
use crate::stubs::Stubs;
use crate::traits::DefinitionExt;
use crate::traits::ExprExt;
use crate::traits::ModuleNameExt;

/// Classification of a single call argument for tracking imported state flow.
enum ArgClass {
    /// The argument resolves to an imported variable.
    Imported,
    /// The argument forwards one of the enclosing function's parameters (by name).
    ForwardedParam(ModuleName),
    /// Neither imported nor a forwarded parameter.
    Other,
}

#[derive(Default)]
struct CallDataBuilder {
    has_unsafe: bool,
    unsafe_indices: u64,
    unsafe_keyword_names: Vec<ModuleName>,
    has_unsafe_kwargs_expansion: bool,
    unsafe_args_expansion_min: Option<usize>,
    forwarded_params: Vec<(ArgSlot, ModuleName)>,
}

impl CallDataBuilder {
    fn record_positional(&mut self, index: usize, is_starred: bool, class: ArgClass) {
        match class {
            ArgClass::Imported => {
                self.has_unsafe = true;
                if is_starred {
                    // An imported value unpacked through `*args` can reach every positional slot
                    // from `index` onward. Arguments arrive in order, so the first star is the
                    // minimum affected index.
                    self.unsafe_args_expansion_min.get_or_insert(index);
                } else if index < MAX_ARGS {
                    // The bitmask represents only the first `MAX_ARGS` positional slots.
                    self.unsafe_indices |= 1u64 << index;
                }
            }
            ArgClass::ForwardedParam(param) => {
                let slot = if is_starred {
                    // `g(prefix, *param)` spreads the parameter across every callee slot from
                    // `index` onward, so conservatively retain it as a star expansion.
                    ArgSlot::StarExpansion(index)
                } else {
                    // Retain the destination slot so callee mutations can be attributed back to
                    // the forwarded parameter.
                    ArgSlot::Positional(index)
                };
                self.forwarded_params.push((slot, param));
            }
            ArgClass::Other => {}
        }
    }

    fn record_keyword(&mut self, name: Option<&Identifier>, class: ArgClass) {
        match class {
            ArgClass::Imported => {
                self.has_unsafe = true;
                match name {
                    Some(name) => self
                        .unsafe_keyword_names
                        .push(ModuleName::from_str(name.as_str())),
                    // A `**kwargs` expansion cannot be reduced to specific keyword names.
                    None => self.has_unsafe_kwargs_expansion = true,
                }
            }
            ArgClass::ForwardedParam(param) => {
                // Only a named keyword maps to a specific callee parameter; forwarding through
                // `**kwargs` cannot be tracked precisely.
                if let Some(name) = name {
                    self.forwarded_params
                        .push((ArgSlot::Keyword(ModuleName::from_str(name.as_str())), param));
                }
            }
            ArgClass::Other => {}
        }
    }

    fn finish(self) -> CallData {
        CallData::new(
            self.has_unsafe,
            self.unsafe_indices,
            self.unsafe_keyword_names,
            self.has_unsafe_kwargs_expansion,
        )
        .with_args_expansion(self.unsafe_args_expansion_min)
        .with_forwarded_params(self.forwarded_params)
    }
}

// Main entry point for the analyzer library.
//
// Runs a side-effect analysis over a module and returns a list of effects.
pub fn analyze(
    parsed_module: &ParsedModule,
    exports: &Exports,
    import_graph: &ImportGraph,
    stubs: &Stubs,
    config: &AnalysisConfig,
) -> AnalyzedModule {
    if parsed_module.is_thrift_generated {
        trace!(
            "Special-casing Thrift-generated module {}",
            parsed_module.name.as_str()
        );
        let analyzer = SourceAnalyzer::new(parsed_module, exports, import_graph, stubs, config);
        let mut result = analyzer.analyze();
        result.module_effects.effects = crate::effects::EffectTable::empty();
        return result;
    }
    trace!("Checking module {}", parsed_module.name.as_str());
    let analyzer = SourceAnalyzer::new(parsed_module, exports, import_graph, stubs, config);
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

fn is_builtin_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::FString(_)
            | Expr::TString(_)
            | Expr::List(_)
            | Expr::Dict(_)
            | Expr::Set(_)
            | Expr::Tuple(_)
            | Expr::ListComp(_)
            | Expr::SetComp(_)
            | Expr::DictComp(_)
    )
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

    fn check_eval(
        &self,
        args: Option<&Arguments>,
        fname: ModuleName,
        range: TextRange,
        output: &mut ModuleEffects,
    ) {
        let Some(first_arg) = args.and_then(|a| a.args.first()) else {
            return;
        };
        let Expr::StringLiteral(s) = first_arg else {
            self.add_effect(Effect::new(EffectKind::ExecCall, fname, range), output);
            return;
        };
        let code = s.value.to_str();
        if code.is_empty() {
            return;
        }
        let ruff_version = to_ruff_version(&self.info.config.sys_info.version());
        let options = ParseOptions::from(PySourceType::Python).with_target_version(ruff_version);
        let parsed = ruff_python_parser::parse_unchecked(code, options);
        let module = match parsed.into_syntax() {
            ruff_python_ast::Mod::Module(m) => m,
            _ => {
                self.add_effect(Effect::new(EffectKind::ExecCall, fname, range), output);
                return;
            }
        };
        let Some(Stmt::Expr(expr_stmt)) = module.body.first() else {
            self.add_effect(Effect::new(EffectKind::ExecCall, fname, range), output);
            return;
        };
        let mut eval_effects = ModuleEffects::new();
        self.expr(&expr_stmt.value, &mut eval_effects);
        for (scope, effs) in eval_effects.effects.iter() {
            for eff in effs {
                let mut adjusted = eff.clone();
                adjusted.range = range;
                output.add_effect(*scope, adjusted);
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
                if let Some(full_module) = res.qualify_import_access(&attribute_module) {
                    if self.import_graph.contains(&full_module) {
                        return Some(full_module);
                    }
                }
            }
        };
        None
    }

    fn unknown_function_name(&self, func: &Expr, output: &mut ModuleEffects) {
        // A method call whose receiver is a freshly-constructed builtin literal operates on a value
        // with no external aliasing, so it cannot have an import-time side effect.
        if let Expr::Attribute(e) = func
            && is_builtin_literal(&e.value)
        {
            return;
        }
        trace!("Unknown call: {}(...)", format::format_expr(func));
        let name = match func {
            Expr::Attribute(e) => match *e.value {
                // `foo().bar()` whose receiver has no known class
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

    /// Classify a single call argument as imported/forwarded/neither
    fn classify_call_arg(&self, arg: &Expr, output: &mut ModuleEffects) -> ArgClass {
        let Some(res) = self.info.resolve(&self.cursor, arg) else {
            return ArgClass::Other;
        };
        if res.is_import() || self.is_import_alias(arg) {
            let name = ModuleName::from_name(&res.name);
            let eff = Effect::new(EffectKind::ImportedVarArgument, name, arg.range());
            self.add_effect(eff, output);
            return ArgClass::Imported;
        }
        // A reference to one of the current function's parameters, directly or
        // through a local alias (`y = x; g(y)` forwards `x`).
        if let Some((scope, param)) = self.receiver_param(&res)
            && self.cursor.current_function_scope() == Some(scope)
        {
            return ArgClass::ForwardedParam(ModuleName::from_name(&param));
        }
        ArgClass::Other
    }

    fn check_call_args(&self, args: &Arguments, output: &mut ModuleEffects) -> CallData {
        let mut builder = CallDataBuilder::default();

        for (i, arg) in args.args.as_ref().iter().enumerate() {
            // Unwrap starred args *x to classify the inner expression. A starred arg
            // unpacks into positional slots starting at this position `i`, so its
            // effect is tracked from index `i` onward rather than at a single slot.
            let (is_starred, inner_arg) = match arg {
                Expr::Starred(starred) => (true, &*starred.value),
                _ => (false, arg),
            };
            builder.record_positional(i, is_starred, self.classify_call_arg(inner_arg, output));
        }
        for arg in args.keywords.as_ref() {
            builder.record_keyword(arg.arg.as_ref(), self.classify_call_arg(&arg.value, output));
        }
        builder.finish()
    }

    /// True when a stub defines the called function, so the stub's declared
    /// effect should govern the call rather than a conservative fallback.
    fn call_effect_from_stub(&self, fname: &ModuleName) -> bool {
        let Some((module, name)) = fname.split_attr() else {
            return false;
        };
        self.info
            .stubs
            .get(&module)
            .is_some_and(|stub| stub.definitions.get(&module, &name).is_some())
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
        } else if name == "eval" {
            self.check_eval(args, fname, func.range(), output);
        } else if let Some(eff) = builtins.call_effect(func) {
            // Prohibited builtin call (e.g. open, __import__)
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
        // Resolve the base name's alias chain to its terminal binding. Only a
        // module-qualified Global yields a valid substitution: a Local terminal
        // (broken chain, cycle, or depth bound in resolve) carries a local
        // variable name, which would produce a bogus module name here.
        match self
            .info
            .bindings
            .resolve_qualified_global_alias(res, call_name)
        {
            QualifiedGlobalAlias::Resolved(_, qualified, _) => Some(qualified),
            QualifiedGlobalAlias::NoAlias | QualifiedGlobalAlias::UnusableAlias => None,
        }
    }

    fn get_method_definition_source_name(
        &self,
        attr_name: &Name,
        bindings_value: &ModuleName,
    ) -> Option<ModuleName> {
        let attr = Attribute::from_module_name(bindings_value);
        let source_name = self.info.exports.resolve_transitive(&attr)?;
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

        if let Some(fname) = self.chained_call_method(func) {
            output.called_functions.insert(fname);
            let data = call_data.into_effect_data();
            let eff = Effect::with_data(EffectKind::MethodCall, fname, range, data);
            self.add_effect(eff, output);
            return;
        }

        let resolved = self.resolve_function_name(func, args, output);

        // More than MAX_ARGS positional args overflow the `unsafe_indices` bitset,
        // so we fall back to a conservative `TooManyArgs` error — unless a stub
        // provides the callee's effect, in which case that effect governs instead.
        if let Some(a) = args
            && a.args.len() > MAX_ARGS
            && !resolved
                .as_ref()
                .is_some_and(|(_, fname)| self.call_effect_from_stub(fname))
        {
            let eff = Effect::new(EffectKind::TooManyArgs, ModuleName::empty(), a.range());
            self.add_effect(eff, output);
        }

        let Some((res, fname)) = resolved else {
            return;
        };

        output.called_functions.insert(fname);
        let data = call_data.into_effect_data();

        // Functions we have special-cased as safe.
        if manual_override::declared_safe(&fname) {
            return;
        }

        // Check if this is a method call before checking if it's a function
        if self.handle_method_call(func, &res, fname, range, &data, output) {
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

    /// A method called on the result of another call (`Widget().render()`,
    /// `make_widget().render()`), resolved to `<class>.<method>`.
    fn chained_call_method(&self, func: &Expr) -> Option<ModuleName> {
        let Expr::Attribute(ExprAttribute { value, attr, .. }) = func else {
            return None;
        };
        let Expr::Call(receiver) = value.as_ref() else {
            return None;
        };
        Some(self.call_result_class(&receiver.func)?.append_str(&attr.id))
    }

    /// The class a call to `func` returns: `func` itself when it names a class,
    /// otherwise its stub-annotated return type.
    fn call_result_class(&self, func: &Expr) -> Option<ModuleName> {
        // A method on another call's result is not a name the resolver can see,
        // so resolve it against the class that call returns.
        let name = match self.chained_call_method(func) {
            Some(chained) => chained,
            None => self.call_target_name(func)?,
        };
        if self.info.exports.is_class(&name) {
            return Some(name);
        }
        self.info.exports.resolve_return_class(&name)
    }

    /// The fully qualified name a call target resolves to.
    fn call_target_name(&self, func: &Expr) -> Option<ModuleName> {
        let res = self.info.resolve(&self.cursor, func)?;
        // A method on a typed receiver (`factory.create`) belongs to the
        // receiver's class, not to the scope the receiver is bound in.
        if let Expr::Attribute(ExprAttribute { attr, .. }) = func
            && let Some(typ) = self.info.bindings.get_type(&res.scope, &res.name)
        {
            return Some(typ.append_str(&attr.id));
        }
        self.resolved_fname(&res)
    }

    /// The call name `res` resolves to, with any import alias substituted.
    fn resolved_fname(&self, res: &ResolvedName) -> Option<ModuleName> {
        let call_name = res.expr_full_name?;
        Some(
            self.fname_replace_import_alias(&call_name, res)
                .unwrap_or_else(|| res.qualified_name()),
        )
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

        let Some(fname) = self.resolved_fname(&res) else {
            // We have a base name but no full name (e.g. `x.f[i]()`)
            self.unknown_function_name(func, output);
            return None;
        };

        Some((res, fname))
    }

    // Helper function for check_unpacked_call()
    fn handle_method_call(
        &self,
        func: &Expr,
        res: &ResolvedName,
        fname: ModuleName,
        range: TextRange,
        data: &EffectData,
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

        // A receiver that refers to a parameter, directly or through a local
        // alias (`y = x`), is treated as a parameter mutation.
        let receiver_param = self.receiver_param(res);

        // For param receivers with unknown type, check if the method is
        // known-safe across all builtin types (e.g. copy, get, index).
        // Only applies to builtins — user-defined classes with same-named
        // methods are not affected since their types aren't in the builtins stub.
        let is_safe_builtin_method = receiver_param.is_some()
            && typ.is_none()
            && self.info.stubs.is_method_safe_in_builtins(&attr.id);

        // A call through the class itself (`C.method(obj, ...)`) passes the
        // receiver explicitly, so argument-to-parameter alignment differs from a
        // bound call (`obj.method(...)`).
        let receiver_is_class = self.receiver_is_class(res);
        let kind = if receiver_is_class {
            EffectKind::UnboundMethodCall
        } else {
            EffectKind::MethodCall
        };

        if !is_safe_builtin_method {
            let eff = Effect::with_data(kind, fname, range, data.clone());
            self.add_effect(eff, output);
        }

        self.check_indirectly_called_method(fname, res, attr, output);

        // A safe builtin method (e.g. copy/get/index) never mutates its receiver,
        // so it does not count as a parameter mutation.
        if let Some(param) = &receiver_param
            && !is_safe_builtin_method
        {
            self.check_param_method_mutation(param, typ, &attr.id, range, output);
        }
        self.check_global_receiver_mutation(res, typ, &attr.id, range, output);

        true
    }

    /// A mutating method (e.g. `list.append`) on a receiver that resolves to a
    /// parameter records a parameter mutation.
    fn check_param_method_mutation(
        &self,
        param: &(ModuleName, Name),
        typ: Option<&ModuleName>,
        attr: &Name,
        range: TextRange,
        output: &mut ModuleEffects,
    ) {
        let is_mutating = typ.is_none_or(|t| self.may_mutate_receiver(t, attr));
        if is_mutating {
            self.add_param_method_call(param, range, output);
        }
    }

    /// Calling a mutating method on a module-level variable from a nested scope
    /// is a global-variable mutation.
    fn check_global_receiver_mutation(
        &self,
        res: &ResolvedName,
        typ: Option<&ModuleName>,
        attr: &Name,
        range: TextRange,
        output: &mut ModuleEffects,
    ) {
        let is_global_receiver = res.is_global()
            || (res.scope == self.info.module_name && self.cursor.scope() != self.info.module_name);
        if is_global_receiver
            && let Some(t) = typ
            && self.may_mutate_receiver(t, attr)
        {
            let name = ModuleName::from_name(&res.name);
            let eff = Effect::new(EffectKind::GlobalVarMutation, name, range);
            self.add_effect(eff, output);
        }
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
            .scope_has_effect(&method_fqn, EffectKind::Mutation)
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
            if let Some(full_module) = res.qualify_import_access(&attribute_module) {
                // Only the import styles admitted by `is_import()` reach here; an
                // unrecognized style falls back to `None`, conservatively treating
                // the access as a distinct called import.
                let imported_module = res.imported_target();

                if imported_module != Some(full_module) {
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
        if let Some(param) = self.receiver_param(&res) {
            self.add_param_method_call(&param, obj.range(), output);
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

    fn check_name(&self, x: &Expr, output: &mut ModuleEffects) {
        let Expr::Name(e) = x else { return };
        let name = ModuleName::from_name(&e.id);
        if output.all_pending_import_names.contains(&name) {
            output.add_called_import(name, &self.cursor.scope());
            return;
        }

        let Some(res) = self.info.resolve(&self.cursor, x) else {
            return;
        };
        if !res.is_import() {
            return;
        }

        let scope = self.cursor.scope();
        if let Some(module) = res.import_parent() {
            output.add_called_import(module, &scope);
        }
    }

    fn expr(&self, x: &Expr, output: &mut ModuleEffects) {
        match x {
            Expr::Lambda(lambda) => {
                self.check_lambda(lambda, output);
                return;
            }
            Expr::Call(call) => self.check_call(call, output),
            Expr::Attribute(e) => self.check_attr(e, output),
            Expr::Subscript(e) => self.check_subscript(e, output),
            Expr::Compare(e) => self.check_compare(e, output),
            Expr::Name(_) => self.check_name(x, output),
            _ => (),
        }
        x.recurse(&mut |c| self.expr(c, output));
    }

    fn check_lambda(&self, lambda: &ExprLambda, output: &mut ModuleEffects) {
        // TODO(T268531819): Since we cannot detect when a lambda is being called, we do not analyse
        // the body. Parameter defaults are still checked since they execute at definition time.
        if let Some(ref params) = lambda.parameters {
            self.check_param_defaults(params, output);
        }
    }

    /// Parameter defaults are evaluated where the function is defined, not where it
    /// is called, so they run in the enclosing scope like any other expression.
    fn check_param_defaults(&self, params: &Parameters, output: &mut ModuleEffects) {
        for default in params
            .iter_non_variadic_params()
            .filter_map(|p| p.default())
        {
            self.expr(default, output);
        }
    }

    fn is_property_decorator(expr: &Expr) -> bool {
        if let Expr::Attribute(attr) = expr {
            let name = attr.attr.as_str();
            return name == "setter" || name == "getter" || name == "deleter";
        }
        false
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
                Expr::Attribute(_) => {
                    if let Some(param) = self.receiver_param(&res) {
                        self.add_param_method_call(&param, target.range(), output);
                    }
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
                Expr::Subscript(e) => {
                    self.check_assign_target(target, output);
                    // `del f()[k]` evaluates the receiver and the key.
                    self.expr(&e.value, output);
                    self.expr(&e.slice, output);
                }
                Expr::Attribute(e) => {
                    self.check_assign_target(target, output);
                    // `del f().x` evaluates the object the attribute is on.
                    self.expr(&e.value, output);
                }
                _ => {}
            }
        }
    }

    fn check_decorators(&self, decs: &[Decorator], output: &mut ModuleEffects) {
        // Treat a decorator as a call. If the decorator does not have explicit call syntax,
        // treat it as a call with no arguments.
        for dec in decs {
            let mut call = &dec.expression;
            let mut args = None;
            if let Expr::Call(func) = call {
                args = Some(&func.arguments);
                call = &func.func;
            }
            if Self::is_property_decorator(call) {
                continue;
            }
            let Some(res) = self.info.resolve(&self.cursor, call) else {
                self.check_unresolved_call(call, args, output, Some(CallKind::Decorator));
                continue;
            };
            let Some(call_name) = res.expr_full_name else {
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
                .unwrap_or_else(|| res.qualified_name());

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
        // Base classes and keywords (`metaclass=`) are evaluated in the enclosing
        // scope, before the class body runs. A bare name like `class C(Base)` is
        // skipped: looking one up has no side effect
        if let Some(arguments) = &x.arguments {
            let evaluated = arguments
                .args
                .iter()
                .chain(arguments.keywords.iter().map(|keyword| &keyword.value))
                .filter(|expr| !matches!(expr, Expr::Name(_)));
            for expr in evaluated {
                self.expr(expr, output);
            }
        }
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
        self.check_param_defaults(&x.parameters, output);
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
        output.effects.merge(out.effects);
    }

    /// Extract the exception type name from a raise expression.
    /// Handles both `raise ValueError` (ExprName) and `raise ValueError("msg")` (ExprCall).
    fn exception_name(exc: &Expr) -> Option<ModuleName> {
        match exc {
            Expr::Call(call) => call.func.full_name(),
            other => other.full_name(),
        }
    }

    /// `assert cond, msg` evaluates both at import time (unless run under `-O`,
    /// which we can't assume).
    fn assert_(&self, x: &StmtAssert, output: &mut ModuleEffects) {
        self.expr(&x.test, output);
        if let Some(msg) = &x.msg {
            self.expr(msg, output);
        }
    }

    fn raise(&self, x: &StmtRaise, output: &mut ModuleEffects) {
        let name = match &x.exc {
            Some(exc) => {
                // Both are evaluated before the exception propagates, even when a
                // handler catches it.
                self.expr(exc, output);
                if let Some(cause) = &x.cause {
                    self.expr(cause, output);
                }
                Self::exception_name(exc)
                    .unwrap_or_else(|| ModuleName::from_str("<unknown exception>"))
            }
            // Bare `raise` (re-raise): we can't determine the type, so treat it as caught if
            // we're inside any try body at all.
            None if self.cursor.in_try_body() => return,
            None => ModuleName::from_str("<unknown exception>"),
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
        for (test, body) in self
            .info
            .config
            .lg_pruned_if_branches(x, self.info.module_name)
        {
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

    fn import_from(&self, x: &StmtImportFrom, output: &mut ModuleEffects) {
        self.add_import_from(x, output, false);

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
            self.check_multi_reexport(&exported_name, x.range, output);
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
            self.check_multi_reexport(&exported_name, x.range, output);
        }
    }

    /// Emit an `ImportedVarReassignment` effect when `exported_name` is re-exported
    /// by multiple imports (its binding was cleared at a different range), so the
    /// effect is not attributed to the first import statement.
    fn check_multi_reexport(
        &self,
        exported_name: &Attribute,
        range: TextRange,
        output: &mut ModuleEffects,
    ) {
        if self
            .info
            .exports
            .get_re_export(exported_name)
            .is_some_and(|(v, r)| {
                v.module == ModuleName::empty() && v.attr.is_empty() && r != &range
            })
        {
            let eff = Effect::new(
                EffectKind::ImportedVarReassignment,
                exported_name.as_module_name(),
                range,
            );
            self.add_effect(eff, output);
        }
    }

    fn add_pending_import(&self, x: &StmtImport, output: &mut ModuleEffects) {
        for name in &x.names {
            let to = ModuleName::from_name(&name.name.id);
            if !x.is_lazy {
                output.eager_imports.insert(to);
            }
            match &name.asname {
                None => output.add_pending_import(to, &self.cursor.scope()),
                Some(asname) => {
                    let as_name_module = ModuleName::from_name(&asname.id);
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

            if !x.is_lazy {
                output.eager_imports.insert(m);
            }

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
                    self.add_sub_import(&m, &name.name.id, &scope, output, is_called);
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
            self.add_sub_import(m, name, scope, output, is_called);
        }
    }

    /// Add `m.name` (or bare `name` when `m` is empty) as a called or pending
    /// import, when it resolves in the graph or `m` has a missing import.
    fn add_sub_import(
        &self,
        m: &ModuleName,
        name: &Name,
        scope: &ModuleName,
        output: &mut ModuleEffects,
        is_called: bool,
    ) {
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
            Stmt::Return(e) => {
                if let Some(value) = &e.value {
                    self.expr(value, output);
                }
            }
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
            Stmt::Assert(e) => self.assert_(e, output),
            // A type alias' value is only evaluated on first access (PEP 695).
            Stmt::TypeAlias(_) => {}
            // Nothing to evaluate.
            Stmt::Pass(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Global(_)
            | Stmt::Nonlocal(_) => {}
            // `!cmd` / `%magic` runs arbitrary code, but it only parses in a
            // notebook: the analyzer is handed `.py` sources, where this cannot
            // appear.
            Stmt::IpyEscapeCommand(_) => {}
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

    /// If `res` ultimately resolves to a parameter, return that parameter's defining scope and
    /// name.
    fn receiver_param(&self, res: &ResolvedName) -> Option<(ModuleName, Name)> {
        if res.definition.is_param() {
            return Some((res.scope, res.name.clone()));
        }
        match self.info.bindings.resolve(&res.scope, &res.name)? {
            Alias::Local(scope, name)
                if self
                    .info
                    .definitions
                    .get(scope, name)
                    .is_some_and(|d| d.is_param()) =>
            {
                Some((*scope, name.clone()))
            }
            _ => None,
        }
    }

    /// Record a `ParamMethodCall` effect for a mutation of `param` (scope, name)
    /// in the scope where the parameter is defined.
    /// We need the defining scope because when a nested function mutates a captured parameter from
    /// an enclosing function, the effect must be attributed to the enclosing function's scope so
    /// that check_call_params can match the param name to the function's parameter list.
    fn add_param_method_call(
        &self,
        param: &(ModuleName, Name),
        range: TextRange,
        output: &mut ModuleEffects,
    ) {
        let (param_scope, param_name) = param;
        let eff = Effect::new(
            EffectKind::ParamMethodCall,
            ModuleName::from_name(param_name),
            range,
        );
        output.add_effect(*param_scope, eff);
    }

    /// Whether `res` resolves to a class definition, directly or through a local alias chain.
    /// Used to distinguish bound method calls (`obj.method()`) from unbound calls via the class
    /// (`C.method(obj, ...)`), including cases like `D = C; D.method(...)`.
    fn receiver_is_class(&self, res: &ResolvedName) -> bool {
        if res.definition.is_class() {
            return true;
        }
        // Check direct bindings lookup first for efficiency.
        if matches!(
            self.info.bindings.lookup(&res.scope, &res.name),
            Some(Value::Class(_))
        ) {
            return true;
        }
        // Follow alias chain to terminal binding and check if it resolves to a class.
        match self.info.bindings.resolve(&res.scope, &res.name) {
            Some(Alias::Global(Value::Class(_))) => true,
            // Mirror the direct check above: a terminal local name may be recorded
            // as a class in either the definition table or the bindings table.
            Some(Alias::Local(scope, name)) => {
                self.info
                    .definitions
                    .get(scope, name)
                    .is_some_and(|d| d.is_class())
                    || matches!(
                        self.info.bindings.lookup(scope, name),
                        Some(Value::Class(_))
                    )
            }
            _ => false,
        }
    }
}

impl<'a> Analyzer<'a> for SourceAnalyzer<'a> {
    fn new(
        parsed_module: &'a ParsedModule,
        exports: &'a Exports,
        import_graph: &'a ImportGraph,
        stubs: &'a Stubs,
        config: &'a AnalysisConfig,
    ) -> Self {
        let info = ModuleInfo::new(parsed_module, exports, import_graph, stubs, config);
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
