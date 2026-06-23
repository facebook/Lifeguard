/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::str::FromStr;

use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprCall;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtClassDef;
use ruff_python_ast::StmtFunctionDef;
use ruff_python_ast::StmtIf;
use tracing::trace;

use crate::analyzer::AnalyzedModule;
use crate::analyzer::Analyzer;
use crate::config::AnalysisConfig;
use crate::cursor::Cursor;
use crate::effects::Effect;
use crate::effects::EffectKind;
use crate::exports::Exports;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::module_effects::ModuleEffects;
use crate::module_info::ModuleInfo;
use crate::module_parser::ParsedModule;
use crate::module_parser::parse_pyi;
use crate::stubs::Stubs;
use crate::traits::ExprExt;
use crate::traits::ModuleNameExt;

/// Main entry point for the stub analyzer.
pub fn analyze(
    parsed_module: &ParsedModule,
    exports: &Exports,
    import_graph: &ImportGraph,
    stubs: &Stubs,
    config: &AnalysisConfig,
) -> AnalyzedModule {
    trace!("Checking module {}", parsed_module.name.as_str());
    let reader = StubAnalyzer::new(parsed_module, exports, import_graph, stubs, config);
    reader.analyze()
}

/// Processes a stub file that's represented as a string.  Used for the bundled stubs, doesn't take
/// in an import graph.
pub fn analyze_str(mod_name: ModuleName, body: &str, stubs: &Stubs) -> AnalyzedModule {
    let parsed_module = parse_pyi(body, mod_name, false);
    let import_graph = ImportGraph::new();
    let exports = Exports::empty();
    let config = AnalysisConfig::default();
    analyze(&parsed_module, &exports, &import_graph, stubs, &config)
}

/// Implementation of Analyzer for stub files (.pyi).
pub struct StubAnalyzer<'a> {
    info: ModuleInfo<'a>,
    parsed_module: &'a ParsedModule,
    cursor: Cursor,
}

impl<'a> StubAnalyzer<'a> {
    fn run_body(&mut self, func_def: &StmtFunctionDef) -> ModuleEffects {
        // Run the function body.
        let mut out = ModuleEffects::new();
        self.cursor.enter_function_scope(func_def);
        self.stmts(&func_def.body, &mut out);
        if out.effects.is_empty() {
            let kind = EffectKind::UnknownEffects;
            let name = ModuleName::empty();
            out.add_effect(self.cursor.scope(), Effect::new(kind, name, func_def.range));
        }
        self.cursor.exit_scope();
        out
    }

    fn get_call_effect(&self, call: &ExprCall) -> Option<Effect> {
        // Try to parse a function call `f("arg")` into effect `f` with name `arg`
        let box func = &call.func;
        let name = func.as_var_name()?;
        let eff_name = name.as_str().replace("_", "-");
        let kind = EffectKind::from_str(&eff_name).ok()?;
        let box args = &call.arguments.args;
        let arg = match &args {
            [Expr::StringLiteral(e), ..] => Some(ModuleName::from_str(e.value.to_str())),
            [] => Some(ModuleName::empty()),
            _ => None,
        }?;
        Some(Effect::new(kind, arg, call.range))
    }

    fn parse_call(&self, call: &ExprCall, output: &mut ModuleEffects) {
        if let Some(eff) = self.get_call_effect(call) {
            output.add_effect(self.cursor.scope(), eff)
        } else {
            let err = format!("Could not parse effect {:?}", call);
            output.add_file_error(err, call.range);
        }
    }

    fn expr(&self, x: &Expr, output: &mut ModuleEffects) {
        match x {
            Expr::Call(call) => self.parse_call(call, output),
            _ => (),
        }
    }

    fn class_def(&mut self, x: &StmtClassDef, output: &mut ModuleEffects) {
        self.cursor.enter_class_scope(x);
        for body_stmt in &x.body {
            self.stmt(body_stmt, output);
        }
        self.cursor.exit_scope();
    }

    fn function_def(&mut self, x: &StmtFunctionDef, output: &mut ModuleEffects) {
        let out = self.run_body(x);
        output.effects.merge(out.effects);
    }

    fn if_(&mut self, x: &StmtIf, output: &mut ModuleEffects) {
        for (_, body) in self
            .info
            .config
            .lg_pruned_if_branches(x, self.info.module_name)
        {
            self.stmts(body, output);
        }
    }

    fn stmt(&mut self, x: &Stmt, output: &mut ModuleEffects) {
        match x {
            Stmt::Expr(e) => self.expr(&e.value, output),
            Stmt::ClassDef(c) => self.class_def(c, output),
            Stmt::FunctionDef(f) => self.function_def(f, output),
            Stmt::If(x) => self.if_(x, output),
            _ => {}
        }
    }

    fn stmts(&mut self, xs: &[Stmt], output: &mut ModuleEffects) {
        for x in xs {
            self.stmt(x, output);
        }
    }

    // Remove unknown_effects() if we have any other effect for the function.
    // Lets us annotate only the first overload with effects and keep the rest as `...`
    fn remove_unknown_effects(&self, output: &mut ModuleEffects) {
        for (_, effs) in output.effects.iter_mut() {
            if effs.len() > 1 {
                effs.retain(|e| e.kind != EffectKind::UnknownEffects);
            }
        }
    }

    // Strip out any entry containing `no-effects` from the effect table.
    fn remove_noeffects(&self, output: &mut ModuleEffects) {
        output.effects.retain(|k, effs| !has_noeffects(k, effs));
    }

    fn module(&mut self, xs: &[Stmt], output: &mut ModuleEffects) {
        self.stmts(xs, output);
        self.remove_unknown_effects(output);
        self.remove_noeffects(output);
    }
}

impl<'a> Analyzer<'a> for StubAnalyzer<'a> {
    fn new(
        parsed_module: &'a ParsedModule,
        exports: &'a Exports,
        import_graph: &'a ImportGraph,
        stubs: &'a Stubs,
        config: &'a AnalysisConfig,
    ) -> Self {
        let info = ModuleInfo::new(parsed_module, exports, import_graph, stubs, config);
        Self {
            info,
            parsed_module,
            cursor: Cursor::new(),
        }
    }

    fn analyze(mut self) -> AnalyzedModule {
        let mut output = ModuleEffects::new();
        self.cursor.enter_module_scope(&self.info.module_name);
        self.module(&self.parsed_module.ast.body, &mut output);
        AnalyzedModule {
            module_effects: output,
            definitions: self.info.definitions,
            classes: self.info.classes,
            implicit_imports: AHashSet::new(),
        }
    }
}

// Returns true if the effects list contains no-effects. Also validates that we only have
// `no-effects` and `unknown-effects` in that case; panic if `no-effects` is mixed with any
// other kind of effect.
fn has_noeffects(key: &ModuleName, effs: &[Effect]) -> bool {
    if effs.iter().any(|e| e.kind == EffectKind::NoEffects) {
        assert!(
            effs.iter()
                .all(|e| matches!(e.kind, EffectKind::NoEffects | EffectKind::UnknownEffects)),
            "{:?} has conflicting effects: {:?}",
            key,
            effs
        );
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::effects::EffectTable;
    use crate::module_parser::parse_pyi;
    use crate::test_lib::assert_str_keys;
    use crate::test_lib::run_module_analysis;

    pub fn run_file(code: &str) -> ModuleEffects {
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_pyi(code, mod_name, false);
        run_module_analysis(code, &parsed_module)
    }

    fn assert_keys(effs: &EffectTable, keys: Vec<&str>) {
        assert_str_keys(effs.keys(), keys);
    }

    fn get_scope_effects<'a>(effs: &'a EffectTable, key: &str) -> Vec<&'a Effect> {
        let eff = effs.get(&ModuleName::from_str(key));
        assert!(eff.is_some());
        eff.unwrap().iter().collect()
    }

    #[test]
    fn test_basic() {
        let code = r#"
imported_function_call("os.path.join")
"#;
        let out = run_file(code);
        let effs = &out.effects;
        let e = get_scope_effects(effs, "test");
        let e = e[0];
        assert!(matches!(e.kind, EffectKind::ImportedFunctionCall));
        assert_eq!(e.name.as_str(), "os.path.join");
    }

    #[test]
    fn test_effect_in_function_body() {
        let code = r#"
import a
def f():
    imported_var_mutation("a.x")
"#;
        let out = run_file(code);
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
    imported_var_mutation("a.x")
"#;
        let out = run_file(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test.A"]);
    }

    #[test]
    fn test_effect_in_method_body() {
        let code = r#"
import a
class A:
    def f():
        imported_var_mutation("a.x")
"#;
        let out = run_file(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test.A.f"]);
    }

    #[test]
    fn test_effects_in_multiple_scopes() {
        let code = r#"
import a
imported_var_mutation("a.x")
class A:
    imported_var_mutation("a.x")
    def f():
        imported_var_mutation("a.x")
    def g():
        imported_var_mutation("a.x")
"#;
        let out = run_file(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test", "test.A", "test.A.f", "test.A.g"]);
    }

    #[test]
    fn test_invalid_effect() {
        let code = r#"
no_such_effect("a")
"#;
        let out = run_file(code);
        let errs = &out.file_errors;
        let e = &errs[0];
        assert!(e.error.contains("Could not parse effect"));
        assert!(e.error.contains("no_such_effect"));
        assert_eq!(e.range.start(), 1.into());
    }

    #[test]
    fn test_invalid_arg() {
        let code = r#"
imported_var_mutation(a)
"#;
        let out = run_file(code);
        let errs = &out.file_errors;
        let e = &errs[0];
        assert!(e.error.contains("Could not parse effect"));
        assert!(e.error.contains("imported_var_mutation"));
        assert_eq!(e.range.start(), 1.into());
    }

    #[test]
    fn test_unknown_effects() {
        let code = r#"
def f():
    ...
"#;
        let out = run_file(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test.f"]);
        let e = get_scope_effects(effs, "test.f");
        let e = e[0];
        assert!(matches!(e.kind, EffectKind::UnknownEffects));
    }

    #[test]
    fn test_overloads() {
        let code = r#"
@overload
def f():
    mutation()

@overload
def f():
    dunder("__iter__")

@overload
def f():
    ...
"#;
        let out = run_file(code);
        let effs = &out.effects;
        let e = get_scope_effects(effs, "test.f");
        assert_eq!(e.len(), 2);
        assert!(!e.iter().any(|x| x.kind == EffectKind::UnknownEffects));
    }

    #[test]
    fn test_strip_noeffects() {
        let code = r#"
def f():
    ...

def g():
    no_effects()
"#;
        let out = run_file(code);
        let effs = &out.effects;
        // `g` should not appear in the effects table
        assert_keys(effs, vec!["test.f"]);
    }

    #[test]
    #[should_panic]
    fn test_noeffects_panics_when_mixed_with_effects() {
        let code = r#"
def f():
    ...

def g():
    imported_var_mutation("a")
    no_effects()
"#;
        // panics
        run_file(code);
    }

    #[test]
    fn test_noeffect_overloads() {
        let code = r#"
@overload
def f():
    ...

@overload
def f():
    no_effects()
"#;
        let out = run_file(code);
        let effs = &out.effects;
        // `f` should not appear in the effects table
        assert_keys(effs, vec![]);
    }

    #[test]
    #[should_panic]
    fn test_panic_with_overloads() {
        let code = r#"
@overload
def f():
    imported_var_mutation("a")

@overload
def f():
    no_effects()
"#;
        let out = run_file(code);
        let effs = &out.effects;
        // `f` should not appear in the effects table
        assert_keys(effs, vec![]);
    }
}
