/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

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

use crate::config::AnalysisConfig;
use crate::exports::Exports;
use crate::graph::Graph;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::module_parser::ParsedModule;
use crate::source_map::ModuleProvider;
use crate::tracing::time;
use crate::traits::ModuleNameExt;

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

    /// Resolve the module name imported by an `importlib.import_module(...)` call.
    fn get_imported_module_name(self, call: &ExprCall) -> Option<ModuleName> {
        self.get_imported_module_name_mixed_args(call)
            .or_else(|| self.get_imported_module_name_kw_args(call))
            .or_else(|| self.get_imported_module_name_pos_args(call))
    }

    fn get_imported_module_name_mixed_args(self, call: &ExprCall) -> Option<ModuleName> {
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
        // Case where we have only keyword arguments
        let kw_name =
            call.arguments.keywords.iter().find(
                |kw| matches!(&kw.arg, Some(Identifier { id, .. }) if id.as_str() == "name"),
            )?;
        let Expr::StringLiteral(name) = &kw_name.value else {
            return None;
        };

        if let Some(kw_package) =
            call.arguments.keywords.iter().find(
                |kw| matches!(&kw.arg, Some(Identifier { id, .. }) if id.as_str() == "package"),
            )
            && let Expr::StringLiteral(package) = &kw_package.value
        {
            return self.get_relative_imported_module_name(name, package);
        }
        Some(ModuleName::from_str(name.value.to_str()))
    }

    fn get_imported_module_name_pos_args(self, call: &ExprCall) -> Option<ModuleName> {
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
        // For importlib.import_module, relative imports must have a leading '.' in `name`.
        let name_str = name.value.to_str();
        if !name_str.starts_with('.') {
            return None;
        }

        let package = ModuleName::from_str(package.value.to_str());
        // we take the actual dot count-1 because the name always has a leading dot
        // for example: in the foo.bar case where foo is the package, bar is passed in as ".bar"
        let dot_count: u32 = name_str
            .chars()
            .take_while(|c| *c == '.')
            .count()
            .saturating_sub(1) as u32;

        let suffix = Name::new(name_str.trim_start_matches('.'));

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
    ambiguous: AHashMap<ModuleName, AHashSet<ModuleName>>,
}

impl ImportGraph {
    pub fn new() -> Self {
        Self {
            graph: Graph::new(),
            missing: AHashMap::new(),
            ambiguous: AHashMap::new(),
        }
    }

    /// Build an import graph
    pub fn make(sources: &impl ModuleProvider, config: &AnalysisConfig) -> Self {
        ImportGraphBuilder::with_capacity(sources.len(), config).build(sources)
    }

    /// Build an import graph and collect exports in a single pass, and report which
    /// modules were parsed. Bundled stubs are parsed only when something reaches
    /// them, so the third value is the set the analysis pass should cover.
    pub fn make_with_exports(
        sources: &impl ModuleProvider,
        config: &AnalysisConfig,
    ) -> (Self, Exports, AHashSet<ModuleName>) {
        ImportGraphBuilder::with_capacity(sources.len(), config).build_with_exports(sources)
    }

    /// Get a parallel iterator over all modules in the graph.
    pub fn modules_par_iter(&self) -> impl ParallelIterator<Item = &ModuleName> {
        self.graph.nodes_par_iter().map(|(module, _)| module)
    }

    /// Get all modules imported by a module.
    pub fn get_imports(&self, name: &ModuleName) -> impl Iterator<Item = &ModuleName> {
        self.graph.neighbors(name)
    }

    /// Get all modules that directly import a module.
    pub fn get_importers(&self, name: &ModuleName) -> impl Iterator<Item = &ModuleName> {
        self.graph.reverse_neighbors(name)
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

    pub fn get_ambiguous_imports(&self, name: &ModuleName) -> Option<&AHashSet<ModuleName>> {
        self.ambiguous.get(name)
    }

    /// Check if a module has any imports to unidentified/missing modules.
    pub fn has_missing_import(&self, from: &ModuleName, module: &ModuleName) -> bool {
        self.missing
            .get(from)
            .is_some_and(|mods| mods.contains(module))
    }

    /// Re-resolve missing imports to their nearest known module (the nearest
    /// ancestor package present in the graph). A resolved import becomes a real edge to
    /// that ancestor; unresolvable ones stay missing.
    pub fn resolve_missing_to_known(&mut self) {
        let known: AHashSet<ModuleName> = self.graph.node_names().copied().collect();
        let missing = std::mem::take(&mut self.missing);
        for (from, targets) in missing {
            for target in targets {
                match resolve_to_known_module(&target, &known) {
                    Some(resolved) => {
                        self.graph.try_add_edge(&from, &resolved);
                    }
                    None => {
                        self.missing.entry(from).or_default().insert(target);
                    }
                }
            }
        }
    }
}

#[doc(hidden)]
pub fn resolve_to_known_module(
    name: &ModuleName,
    known: &AHashSet<ModuleName>,
) -> Option<ModuleName> {
    if known.contains(name) {
        return Some(*name);
    }
    name.iter_parents()
        .find(|(p, _)| known.contains(p))
        .map(|(p, _)| p)
}

type Imports = AHashSet<ModuleName>;

struct ModuleImportCollector<'a> {
    module: ModuleName,
    is_init: bool,
    graph: &'a Graph,
    config: &'a AnalysisConfig,
    imports: Imports,
    ambiguous_imports: Imports,
    has_importlib: bool,
    has_import_module: bool,
}

impl<'a> ModuleImportCollector<'a> {
    fn new(
        module: ModuleName,
        is_init: bool,
        graph: &'a Graph,
        config: &'a AnalysisConfig,
    ) -> Self {
        Self {
            module,
            is_init,
            graph,
            config,
            imports: Imports::new(),
            ambiguous_imports: Imports::new(),
            has_importlib: false,
            has_import_module: false,
        }
    }

    fn collect(mut self, ast: &ModModule) -> (Imports, Imports) {
        self.stmts(&ast.body);
        (self.imports, self.ambiguous_imports)
    }

    fn if_(&mut self, s: &StmtIf) {
        for (_, body) in self.config.lg_pruned_if_branches(s, self.module) {
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
        if let Expr::Call(call) = e {
            self.expr_call(call);
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
            let imp_str = imp.as_str();
            if imp_str == "importlib" || imp_str.starts_with("importlib.") {
                self.has_importlib = true;
            }
            // Insert parent modules; for "a.b.c.d" this adds "a", "a.b", "a.b.c".
            for (i, c) in imp_str.char_indices() {
                if c == '.' {
                    self.imports.insert(ModuleName::from_str(&imp_str[..i]));
                }
            }
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

        if self.graph.contains(&maybe_sub) || !self.graph.contains(&parent) {
            self.imports.insert(maybe_sub);
        } else {
            // Parent is in graph but child is not. Could be an attribute
            // of the parent or a submodule defined in a different library.
            // Record as ambiguous for cross-library resolution.
            self.ambiguous_imports.insert(maybe_sub);
        }
    }
}

struct CollectedImports {
    module: ModuleName,
    imports: Imports,
    ambiguous: Imports,
}

struct ImportGraphBuilder<'a> {
    graph: Graph,
    missing: AHashMap<ModuleName, AHashSet<ModuleName>>,
    ambiguous: AHashMap<ModuleName, AHashSet<ModuleName>>,
    config: &'a AnalysisConfig,
}

impl<'a> ImportGraphBuilder<'a> {
    fn with_capacity(node_count: usize, config: &'a AnalysisConfig) -> Self {
        Self {
            // 4x edge estimate: dotted imports like `a.b.c` expand into multiple edges
            graph: Graph::with_capacity(node_count, node_count * 4),
            missing: AHashMap::new(),
            ambiguous: AHashMap::new(),
            config,
        }
    }

    fn add_nodes<'b>(&mut self, keys: impl Iterator<Item = &'b ModuleName>) {
        time("  Adding import nodes to graph", || {
            for name in keys {
                self.graph.add_node(name);
            }
        });
    }

    fn collect_imports(&self, name: ModuleName, module: &ParsedModule) -> CollectedImports {
        let collector = ModuleImportCollector::new(name, module.is_init, &self.graph, self.config);
        let (imports, ambiguous) = collector.collect(&module.ast);
        CollectedImports {
            module: name,
            imports,
            ambiguous,
        }
    }

    fn remove_unparseable_nodes(&mut self, failures: Vec<ModuleName>) {
        for name in failures {
            self.graph.remove_node(&name);
        }
    }

    fn add_edges_and_finish(mut self, all_imports: Vec<CollectedImports>) -> ImportGraph {
        time("  Adding import edges to graph", || {
            for collected in all_imports {
                for to in collected.imports {
                    if !(self.graph.add_edge(&collected.module, &to)) {
                        self.missing.entry(collected.module).or_default().insert(to);
                    }
                }
                if !collected.ambiguous.is_empty() {
                    self.ambiguous
                        .entry(collected.module)
                        .or_default()
                        .extend(collected.ambiguous);
                }
            }
        });

        ImportGraph {
            graph: self.graph,
            missing: self.missing,
            ambiguous: self.ambiguous,
        }
    }

    fn build(mut self, sources: &impl ModuleProvider) -> ImportGraph {
        self.add_nodes(sources.module_names_iter());

        let results: Vec<Result<CollectedImports, ModuleName>> =
            time("  Collecting all import edges", || {
                sources
                    .module_names_par_iter()
                    .filter_map(|name| {
                        let ast_result = sources.parse(name)?;
                        Some(match ast_result.as_parsed() {
                            Ok(module) => Ok(self.collect_imports(*name, module)),
                            Err(_) => Err(*name),
                        })
                    })
                    .collect()
            });

        let mut all_imports = Vec::new();
        let mut failures = Vec::new();
        time("  Splitting results and removing unparseable nodes", || {
            for result in results {
                match result {
                    Ok(imports) => all_imports.push(imports),
                    Err(name) => failures.push(name),
                }
            }
            self.remove_unparseable_nodes(failures);
        });

        self.add_edges_and_finish(all_imports)
    }

    /// Modules the traversal starts from: everything the source DB provided, plus
    /// `builtins`, which the analyzers consult for every module without it being
    /// imported. Bundled stubs are reached only when something imports them.
    ///
    /// Drawn from `module_names_iter` so a seed is always a module the provider
    /// actually provides — `is_stub` may answer for names it does not.
    fn seed_modules(sources: &impl ModuleProvider) -> Vec<ModuleName> {
        let builtins = ModuleName::builtins();
        sources
            .module_names_iter()
            .filter(|name| {
                **name == builtins || !sources.is_stub(name) || sources.overrides_source(name)
            })
            .copied()
            .collect()
    }

    fn build_with_exports(
        mut self,
        sources: &impl ModuleProvider,
    ) -> (ImportGraph, Exports, AHashSet<ModuleName>) {
        self.add_nodes(sources.module_names_iter());

        // Every bundled stub stays a graph node, so import classification and
        // `resolve_missing_to_known` still see the whole name space. Only the stubs
        // something can actually reach get parsed.
        //
        // Reaching a module also reaches its stub submodules, because attribute
        // access resolves through them without an import edge: `import os` alone
        // must still resolve `os.path.join`.
        let mut stub_children: AHashMap<ModuleName, Vec<ModuleName>> = AHashMap::new();
        for name in sources.module_names_iter() {
            if sources.is_stub(name)
                && let Some(parent) = name.parent()
            {
                stub_children.entry(parent).or_default().push(*name);
            }
        }

        let mut reached: AHashSet<ModuleName> = AHashSet::new();
        let mut frontier: Vec<ModuleName> = Vec::new();
        for name in Self::seed_modules(sources) {
            if reached.insert(name) {
                frontier.push(name);
            }
        }

        let mut successes: Vec<(CollectedImports, Exports)> = Vec::new();
        let mut unparseable: Vec<ModuleName> = Vec::new();

        time("  Collecting imports and exports", || {
            while !frontier.is_empty() {
                let results: Vec<Result<(CollectedImports, Exports), ModuleName>> = frontier
                    .par_iter()
                    .filter_map(|name| {
                        let ast_result = sources.parse(name)?;
                        let module = match ast_result.as_parsed() {
                            Ok(module) => module,
                            Err(_) => return Some(Err(*name)),
                        };
                        let imports = self.collect_imports(*name, module);
                        let exports = Exports::new_unfiltered(module, &self.config.sys_info);
                        Some(Ok((imports, exports)))
                    })
                    .collect();

                frontier.clear();
                for result in results {
                    match result {
                        Ok((imports, exports)) => {
                            let children = stub_children.get(&imports.module);
                            for imported in
                                imports.imports.iter().chain(children.into_iter().flatten())
                            {
                                if self.graph.contains(imported) && reached.insert(*imported) {
                                    frontier.push(*imported);
                                }
                            }
                            successes.push((imports, exports));
                        }
                        Err(name) => unparseable.push(name),
                    }
                }
            }
        });

        self.remove_unparseable_nodes(unparseable);

        let (all_imports, all_exports): (Vec<_>, Vec<_>) = successes.into_iter().unzip();
        let import_graph = self.add_edges_and_finish(all_imports);

        let mut merged_exports = time("  Merging exports", || Exports::merge_all(all_exports));
        time("  Filtering module re-exports", || {
            merged_exports.filter_module_re_exports(&import_graph)
        });
        time("  Expanding star re-exports", || {
            merged_exports.expand_star_re_exports(&import_graph)
        });

        (import_graph, merged_exports, reached)
    }
}
