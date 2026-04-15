/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use ahash::AHashMap;
use ahash::AHashSet;
use pyrefly_python::module_name::ModuleName;
use pyrefly_util::visit::Visit;
use rayon::prelude::*;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprCall;
use ruff_python_ast::ExprStringLiteral;
use ruff_python_ast::Identifier;
use ruff_python_ast::ModModule;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtAssign;
use ruff_python_ast::StmtIf;
use ruff_python_ast::StmtImport;
use ruff_python_ast::StmtImportFrom;
use ruff_python_ast::name::Name;

use crate::exports::Exports;
use crate::graph::Graph;
use crate::pyrefly::sys_info::SysInfo;
use crate::source_map::AstResult;
use crate::source_map::ModuleProvider;
use crate::tracing::time;

#[derive(Debug, Copy, Clone)]

pub struct ImportlibState {
    pub has_importlib: bool,
    pub has_import_module: bool,
}

impl ImportlibState {
    pub fn new(has_importlib: bool, has_import_module: bool) -> Self {
        Self {
            has_importlib,
            has_import_module,
        }
    }

    fn is_import_module_call(self, func: &Expr) -> bool {
        if self.has_import_module
            && let Expr::Name(name) = func
            && name.id.as_str() == "import_module"
        {
            return true;
        }

        if self.has_importlib
            && let Expr::Attribute(attr) = func
            && let Expr::Name(value) = &*attr.value
        {
            return value.id.as_str() == "importlib" && attr.attr.as_str() == "import_module";
        }
        false
    }

    fn get_imported_module_name(self, call: &ExprCall) -> Option<ModuleName> {
        // This computes an imported module name specifically for importlib's import_module

        self.get_imported_module_name_mixed_args(call)
            .or_else(|| self.get_imported_module_name_kw_args(call))
            .or_else(|| self.get_imported_module_name_pos_args(call))
    }

    fn get_imported_module_name_mixed_args(self, call: &ExprCall) -> Option<ModuleName> {
        // This computes an imported module name specifically for importlib's import_module

        // Case where we have both positional and keyword arguments. The positional argument will always be name
        if call.arguments.args.len() == 1
            && call.arguments.keywords.len() == 1
            && let Some(kw) = &call.arguments.keywords.first()
            && matches!(&kw.arg, Some(Identifier { id, .. }) if id.as_str() == "package")
            && let Expr::StringLiteral(package) = &kw.value
            && let Some(Expr::StringLiteral(name)) = call.arguments.args.first()
        {
            return self.get_relative_imported_module_name(name, package);
        }
        None
    }

    fn get_imported_module_name_kw_args(self, call: &ExprCall) -> Option<ModuleName> {
        // This computes an imported module name specifically for importlib's import_module

        // Case where we have only keyword arguments
        if let Some(kw_name) = call
            .arguments
            .keywords
            .iter()
            .find(|kw| matches!(&kw.arg, Some(Identifier { id, .. }) if id.as_str() == "name"))
            && let Expr::StringLiteral(name) = &kw_name.value
        {
            if let Some(kw_package) = call.arguments.keywords.iter().find(
                |kw| matches!(&kw.arg, Some(Identifier { id, .. }) if id.as_str() == "package"),
            ) && let Expr::StringLiteral(package) = &kw_package.value
            {
                return self.get_relative_imported_module_name(name, package);
            } else {
                return Some(ModuleName::from_str(name.value.to_str()));
            }
        }
        None
    }

    fn get_imported_module_name_pos_args(self, call: &ExprCall) -> Option<ModuleName> {
        // This computes an imported module name specifically for importlib's import_module

        // Case where we have only positional arguments
        if call.arguments.args.len() == 2
            && let Some(Expr::StringLiteral(name)) = call.arguments.args.first()
            && let Some(Expr::StringLiteral(package)) = call.arguments.args.last()
        {
            return self.get_relative_imported_module_name(name, package);
        } else if call.arguments.args.len() == 1
            && let Some(Expr::StringLiteral(arg)) = call.arguments.args.first()
        {
            return Some(ModuleName::from_str(arg.value.to_str()));
        }
        None
    }

    fn get_relative_imported_module_name(
        self,
        name: &ExprStringLiteral,
        package: &ExprStringLiteral,
    ) -> Option<ModuleName> {
        // This computes an imported module name specifically for importlib's import_module

        // For importlib.import_module, relative imports must have a leading '.' in `name`.
        if !name.value.to_str().starts_with('.') {
            return None;
        }

        let package = ModuleName::from_str(package.value.to_str());
        // we take the actual dot count-1 because the name always has a leading dot
        // for example: in the foo.bar case where foo is the package, bar is passed in as ".bar"
        let dot_count: u32 = name
            .value
            .to_str()
            .chars()
            .take_while(|c| *c == '.')
            .count()
            .saturating_sub(1) as u32;

        let suffix = Name::new(name.value.to_str().trim_start_matches('.'));

        if dot_count == 0 {
            Some(package.append(&suffix))
        } else {
            package.new_maybe_relative(false /* is_init */, dot_count, Some(&suffix))
        }
    }

    pub fn match_call(self, call: &ExprCall) -> Option<ModuleName> {
        if self.is_import_module_call(&call.func) {
            return self.get_imported_module_name(call);
        }
        None
    }
}

pub fn get_import_chain_string(
    obj: &Expr,
    attr: Option<&Identifier>,
    res_name: &Name,
) -> ModuleName {
    // return the string of the implicit import chain, ie "foo.bar.baz"
    let mut current_obj = obj;
    let mut parts = Vec::new();
    if let Some(ident) = attr {
        parts.push(&ident.id);
    }
    while let Expr::Attribute(attr_expr) = current_obj {
        parts.push(&attr_expr.attr.id);
        current_obj = &attr_expr.value;
    }
    parts.push(res_name);
    parts.reverse();

    ModuleName::from_parts(parts)
}

/// The graph of modules to all the modules they import.  Tracks modules by name.
///
/// Not all imports can be resolved.  Modules can be queried for the list of imports that themselves
/// do not have nodes in the graph.
#[derive(Debug)]
pub struct ImportGraph {
    pub graph: Graph,
    missing: AHashMap<ModuleName, AHashSet<ModuleName>>,
}

impl ImportGraph {
    pub fn new() -> Self {
        Self {
            graph: Graph::new(),
            missing: AHashMap::new(),
        }
    }

    /// Build an import graph
    pub fn make(sources: &impl ModuleProvider, sys_info: &SysInfo) -> Self {
        ImportGraphBuilder::with_capacity(sources.len(), sys_info).build(sources)
    }

    /// Build an import graph and collect exports in a single pass
    pub fn make_with_exports(sources: &impl ModuleProvider, sys_info: &SysInfo) -> (Self, Exports) {
        ImportGraphBuilder::with_capacity(sources.len(), sys_info).build_with_exports(sources)
    }

    /// Get a parallel iterator over all modules in the graph.
    pub fn modules_par_iter(&self) -> impl ParallelIterator<Item = &ModuleName> {
        self.graph.nodes_par_iter().map(|(module, _)| module)
    }

    /// Get all modules imported by a module.
    pub fn get_imports(&self, name: &ModuleName) -> impl Iterator<Item = &ModuleName> {
        self.graph.neighbors(name)
    }

    /// Check if a module name is found in the graph.
    pub fn contains(&self, name: &ModuleName) -> bool {
        self.graph.contains(name)
    }

    /// Get the set of modules imported by a module that do not exist in the graph.
    pub fn get_missing_imports(&self, name: &ModuleName) -> Option<&AHashSet<ModuleName>> {
        self.missing.get(name)
    }

    /// Add a missing import edge (for graph reconstruction from cache).
    pub fn add_missing(&mut self, from: &ModuleName, to: ModuleName) {
        self.missing.entry(*from).or_default().insert(to);
    }

    /// Check if a module has any imports to unidentified/missing modules.
    pub fn has_missing_import(&self, from: &ModuleName, module: &ModuleName) -> bool {
        self.missing
            .get(from)
            .is_some_and(|mods| mods.contains(module))
    }
}

type Imports = AHashSet<ModuleName>;

/// Generate all parent modules for a given module path.
/// For "a.b.c.d", returns ["a", "a.b", "a.b.c"] (not including the full path itself).
fn get_parent_modules(module: &ModuleName) -> Vec<ModuleName> {
    let module_str = module.as_str();
    let dot_count = module_str.matches('.').count();
    if dot_count == 0 {
        return Vec::new();
    }

    let mut parents = Vec::with_capacity(dot_count);
    for (i, c) in module_str.char_indices() {
        if c == '.' {
            parents.push(ModuleName::from_str(&module_str[..i]));
        }
    }
    parents
}

struct ModuleImportCollector<'a> {
    module: ModuleName,
    is_init: bool,
    graph: &'a Graph,
    sys_info: &'a SysInfo,
    imports: Imports,
    // keep track of whether importlib has been imported
    has_importlib: bool,
    // keep track of whether import_module has been imported from importlib
    // we need this to process import_module calls
    has_import_module: bool,
}

impl<'a> ModuleImportCollector<'a> {
    fn new(module: ModuleName, is_init: bool, graph: &'a Graph, sys_info: &'a SysInfo) -> Self {
        Self {
            module,
            is_init,
            graph,
            sys_info,
            imports: Imports::new(),
            has_importlib: false,
            has_import_module: false,
        }
    }

    fn collect(mut self, ast: &ModModule) -> Imports {
        self.stmts(&ast.body);
        self.imports
    }

    fn if_(&mut self, s: &StmtIf) {
        for (_, body) in self.sys_info.pruned_if_branches(s) {
            self.stmts(body);
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Import(x) => self.import(x),
            Stmt::ImportFrom(x) => self.import_from(x),
            Stmt::If(x) => self.if_(x),
            Stmt::Try(_) => s.recurse(&mut |stmt| self.stmt(stmt)),
            Stmt::Expr(x) => self.expr(&x.value),
            Stmt::Assign(x) => self.assign(x),
            Stmt::FunctionDef(x) => self.stmts(&x.body),
            Stmt::ClassDef(x) => self.stmts(&x.body),
            _ => {}
        }
    }

    fn assign(&mut self, e: &StmtAssign) {
        if let Expr::Call(call) = &*e.value {
            self.expr_call(call);
        }
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::Call(call) => self.expr_call(call),
            _ => {}
        }
    }

    fn expr_call(&mut self, call: &ExprCall) {
        let import_module_state = ImportlibState {
            has_importlib: self.has_importlib,
            has_import_module: self.has_import_module,
        };

        if let Some(imp) = import_module_state.match_call(call) {
            self.imports.insert(imp);
        }
    }

    fn import(&mut self, import: &StmtImport) {
        for name in &import.names {
            let imp = ModuleName::from_name(&name.name.id);
            if imp.as_str() == "importlib" || imp.as_str().starts_with("importlib.") {
                self.has_importlib = true;
            }
            // Add parent modules; For "a.b.c.d", this adds "a", "a.b", "a.b.c"
            for parent in get_parent_modules(&imp) {
                self.imports.insert(parent);
            }
            // Add the full module path
            self.imports.insert(imp);
        }
    }

    // from parent import a, b, c, ...
    fn import_from(&mut self, import: &StmtImportFrom) {
        // `parent` is a potentially relative name, we need to resolve it with the current module
        let rel = import.module.as_ref().map(|x| &x.id);
        if let Some(parent) = self
            .module
            .new_maybe_relative(self.is_init, import.level, rel)
        {
            if parent.as_str() != "" {
                self.imports.insert(parent);
            }

            for name in &import.names {
                self.import_from_single(parent, &name.name.id);
            }
        }
    }

    // Helper for `import_from`, handles a single import in `from parent import a, b, ...`
    fn import_from_single(&mut self, parent: ModuleName, name: &Name) {
        if parent.as_str() == "importlib" && *name == "import_module" {
            self.has_import_module = true;
        }
        if name == "*" {
            // TODO (T241416033): can * imports bring in a submodule dependency?
            return;
        }

        let maybe_sub = if parent.as_str() == "" {
            ModuleName::from_str(name)
        } else {
            parent.append(name)
        };

        // 1) If the graph contains the submodule `x.y` then we add
        // an edge to represent the submodule that is registered in
        // the ast map.
        // 2) If the source code for module `x` is missing (not in graph),
        // conservatively capture `x.y` as a submodule as we have
        // no way of determining if `x.y` is an attribute or a submodule
        if self.graph.contains(&maybe_sub) || !self.graph.contains(&parent) {
            self.imports.insert(maybe_sub);
        }
    }
}

struct ImportGraphBuilder<'a> {
    graph: Graph,
    missing: AHashMap<ModuleName, AHashSet<ModuleName>>,
    sys_info: &'a SysInfo,
}

impl<'a> ImportGraphBuilder<'a> {
    fn with_capacity(node_count: usize, sys_info: &'a SysInfo) -> Self {
        Self {
            // 4x edge estimate: dotted imports like `a.b.c` expand into multiple edges
            graph: Graph::with_capacity(node_count, node_count * 4),
            missing: AHashMap::new(),
            sys_info,
        }
    }

    fn add_nodes<'b>(&mut self, keys: impl Iterator<Item = &'b ModuleName>) {
        time("  Adding import nodes to graph", || {
            for name in keys {
                self.graph.add_node(name);
            }
        });
    }

    fn collect_imports(
        &self,
        name: ModuleName,
        ast_result: &AstResult,
    ) -> Option<(ModuleName, Imports)> {
        let module = ast_result.as_parsed().ok()?;
        let collector =
            ModuleImportCollector::new(name, module.is_init, &self.graph, self.sys_info);
        let imports = collector.collect(&module.ast);
        Some((name, imports))
    }

    fn add_edges_and_finish(mut self, all_imports: Vec<(ModuleName, Imports)>) -> ImportGraph {
        time("  Adding import edges to graph", || {
            for (from, imports) in all_imports {
                for to in imports {
                    if !(self.graph.add_edge(&from, &to)) {
                        self.missing.entry(from).or_default().insert(to);
                    }
                }
            }
        });

        ImportGraph {
            graph: self.graph,
            missing: self.missing,
        }
    }

    fn build(mut self, sources: &impl ModuleProvider) -> ImportGraph {
        self.add_nodes(sources.module_names_iter());

        let all_imports: Vec<(ModuleName, Imports)> = time("  Collecting all import edges", || {
            sources
                .module_names_par_iter()
                .filter_map(|name| {
                    let ast_result = sources.parse(name)?;
                    self.collect_imports(*name, &ast_result)
                })
                .collect()
        });

        self.add_edges_and_finish(all_imports)
    }

    fn build_with_exports(mut self, sources: &impl ModuleProvider) -> (ImportGraph, Exports) {
        self.add_nodes(sources.module_names_iter());

        let results: Vec<((ModuleName, Imports), Exports)> =
            time("  Collecting imports and exports", || {
                sources
                    .module_names_par_iter()
                    .filter_map(|name| {
                        let ast_result = sources.parse(name)?;
                        let imports = self.collect_imports(*name, &ast_result)?;
                        let module = ast_result.as_parsed().ok()?;
                        let exports = Exports::new_unfiltered(module, self.sys_info);
                        Some((imports, exports))
                    })
                    .collect()
            });

        let (all_imports, all_exports): (Vec<_>, Vec<_>) = results.into_iter().unzip();
        let import_graph = self.add_edges_and_finish(all_imports);

        let mut merged_exports = time("  Merging exports", || Exports::merge_all(all_exports));
        time("  Filtering module re-exports", || {
            merged_exports.filter_module_re_exports(&import_graph)
        });

        (import_graph, merged_exports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lib::*;
    use crate::traits::SysInfoExt;

    fn assert_deps(g: &ImportGraph, module: &str, expected: Vec<&str>) {
        let m = ModuleName::from_str(module);
        let mut exp = module_names(expected);
        let mut actual = g.get_imports(&m).cloned().collect::<Vec<_>>();
        exp.sort();
        actual.sort();
        assert_eq!(actual, exp);
    }

    fn assert_rdeps(g: &ImportGraph, module: &str, expected: Vec<&str>) {
        let m = ModuleName::from_str(module);
        let mut exp = module_names(expected);
        let mut actual = g.graph.reverse_neighbors(&m).cloned().collect::<Vec<_>>();
        exp.sort();
        actual.sort();
        assert_eq!(actual, exp);
    }

    fn build_import_graph(modules: &Vec<(&str, &str)>) -> ImportGraph {
        let sources = TestSources::new(modules);
        let sys_info = SysInfo::lg_default();
        ImportGraph::make(&sources, &sys_info)
    }

    #[test]
    fn test_basic() {
        let a = r#"
def f(): ...
"#;
        let b = r#"
import a
x = a.f()
"#;
        let g = build_import_graph(&vec![("a", a), ("b", b)]);
        assert_deps(&g, "a", vec![]);
        assert_deps(&g, "b", vec!["a"]);
        assert_rdeps(&g, "a", vec!["b"]);
        assert_rdeps(&g, "b", vec![]);
    }

    #[test]
    fn test_cycle() {
        let a = r#"import b"#;
        let b = r#"import a"#;
        let g = build_import_graph(&vec![("a", a), ("b", b)]);
        assert_deps(&g, "a", vec!["b"]);
        assert_deps(&g, "b", vec!["a"]);
        assert_rdeps(&g, "a", vec!["b"]);
        assert_rdeps(&g, "b", vec!["a"]);
    }

    #[test]
    fn test_complex() {
        let a = r#"
import b
import c
"#;
        let b = r"import a";
        let c = r#"
import b
import d
"#;
        let d = r"# no imports";
        let g = build_import_graph(&vec![("a", a), ("b", b), ("c", c), ("d", d)]);
        assert_deps(&g, "a", vec!["b", "c"]);
        assert_deps(&g, "b", vec!["a"]);
        assert_deps(&g, "c", vec!["b", "d"]);
        assert_deps(&g, "d", vec![]);
        assert_rdeps(&g, "a", vec!["b"]);
        assert_rdeps(&g, "b", vec!["a", "c"]);
        assert_rdeps(&g, "c", vec!["a"]);
        assert_rdeps(&g, "d", vec!["c"]);
    }

    #[test]
    fn test_missing_import() {
        let a = "import b";
        let b = "import c";
        let g = build_import_graph(&vec![("a", a), ("b", b)]);
        let expected: AHashSet<ModuleName> = AHashSet::from_iter(vec![ModuleName::from_str("c")]);
        assert_eq!(g.missing.get(&ModuleName::from_str("b")), Some(&expected));
    }

    #[test]
    fn test_import_from() {
        let a = "def f(): ...";
        let a_sub = "def g(): ...";
        let b = "from a import f";
        let c = "from a import sub";
        let d = "from a.sub import g";
        let g = build_import_graph(&vec![
            ("a", a),
            ("a.sub", a_sub),
            ("b", b),
            ("c", c),
            ("d", d),
        ]);
        assert_deps(&g, "b", vec!["a"]);
        assert_deps(&g, "c", vec!["a", "a.sub"]);
        assert_deps(&g, "d", vec!["a.sub"]);
    }

    #[test]
    fn test_relative_import_from() {
        let a = "def f(): ...";
        let a_sub = "from .. import b";
        let b = "from . import a";
        let c = "from .a import sub, f";
        let d = "from .a import f";
        let g = build_import_graph(&vec![
            ("a", a),
            ("a.sub", a_sub),
            ("b", b),
            ("c", c),
            ("d", d),
        ]);
        assert_deps(&g, "a.sub", vec!["b"]);
        assert_deps(&g, "b", vec!["a"]);
        assert_deps(&g, "c", vec!["a", "a.sub"]);
        assert_deps(&g, "d", vec!["a"]);
    }

    #[test]
    fn test_conditional_import() {
        let a = "def f(): ...";
        let b = "def g(): ...";
        let c = "def h(): ...";
        let d = r#"
if __random__:
    import a
else:
    try:
        import b
    except:
        import c
"#;
        let g = build_import_graph(&vec![("a", a), ("b", b), ("c", c), ("d", d)]);
        assert_deps(&g, "d", vec!["a", "b", "c"]);
    }

    #[test]
    fn test_import_module() {
        let a = "def f(): ...";
        let b = r#"
from importlib import import_module
import_module("a")
        "#;
        let g = build_import_graph(&vec![("a", a), ("b", b)]);
        assert_deps(&g, "b", vec!["a", "importlib"]);
    }

    #[test]
    fn test_import_module_with_keywords() {
        let a = "def f(): ...";
        let b = r#"
from importlib import import_module
import_module(package=None, name="a")
        "#;
        let g = build_import_graph(&vec![("a", a), ("b", b)]);
        assert_deps(&g, "b", vec!["a", "importlib"]);
    }

    #[test]
    fn test_import_module_assign() {
        let a = "def f(): ...";
        let b = r#"
from importlib import import_module
A = import_module("a")
        "#;
        let g = build_import_graph(&vec![("a", a), ("b", b)]);
        assert_deps(&g, "b", vec!["a", "importlib"]);
    }

    #[test]
    fn test_import_module_fully_qualified_name() {
        let a = "def f(): ...";
        let b = r#"
import importlib
importlib.import_module("a")
        "#;
        let g = build_import_graph(&vec![("a", a), ("b", b)]);
        assert_deps(&g, "b", vec!["a", "importlib"]);
    }

    #[test]
    fn test_type_checking_block() {
        let a = "def f(): ...";
        let b = "def g(): ...";
        let c = "def h(): ...";
        let d = r#"
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import a
else:
    import b

if not TYPE_CHECKING:
    import c
else:
    import d
"#;
        let g = build_import_graph(&vec![("a", a), ("b", b), ("c", c), ("d", d)]);
        assert_deps(&g, "d", vec!["b", "c", "typing"]);
    }

    #[test]
    fn test_typing_type_checking_block() {
        let a = "def f(): ...";
        let b = "def g(): ...";
        let c = r#"
import typing

if typing.TYPE_CHECKING:
    import a
else:
    import b
"#;
        let g = build_import_graph(&vec![("a", a), ("b", b), ("c", c)]);
        assert_deps(&g, "c", vec!["b", "typing"]);
    }

    #[test]
    fn test_import_tracks_parent_modules() {
        let a = "pass";
        let a_b = "pass";
        let a_b_c = "pass";
        let a_b_c_d = "pass";
        let main = "import a.b.c.d";

        let g = build_import_graph(&vec![
            ("a", a),
            ("a.b", a_b),
            ("a.b.c", a_b_c),
            ("a.b.c.d", a_b_c_d),
            ("main", main),
        ]);

        assert_deps(&g, "main", vec!["a", "a.b", "a.b.c", "a.b.c.d"]);
    }

    #[test]
    fn test_import_as_tracks_parent_modules() {
        let a = "pass";
        let a_b = "pass";
        let a_b_c = "pass";
        let main = "import a.b.c as abc";

        let g = build_import_graph(&vec![
            ("a", a),
            ("a.b", a_b),
            ("a.b.c", a_b_c),
            ("main", main),
        ]);

        assert_deps(&g, "main", vec!["a", "a.b", "a.b.c"]);
    }

    #[test]
    fn test_single_level_import_no_extra_deps() {
        let a = "pass";
        let main = "import a";

        let g = build_import_graph(&vec![("a", a), ("main", main)]);

        assert_deps(&g, "main", vec!["a"]);
    }
}
