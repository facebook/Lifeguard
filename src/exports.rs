/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Top level exports from all the modules in a project
// Used to do type inference. This is a lot simpler than pyrefly's exports.rs which attempts to
// calculate exports more completely and rigorously; we can switch to using that later on if we
// need the full complexity.

use pyrefly_python::module_name::ModuleName;
use pyrefly_python::symbol_kind::SymbolKind;
use pyrefly_util::visit::Visit;
use rayon::prelude::*;
use ruff_python_ast::ExceptHandler;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;
use serde::Deserialize;
use serde::Serialize;

use crate::config::AnalysisConfig;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::hasher::extend_nested;
use crate::hasher::merge_nested_larger;
use crate::imports::ImportGraph;
use crate::module_parser::ParsedModule;
use crate::pyrefly::definitions::Definition;
use crate::pyrefly::definitions::DefinitionStyle;
use crate::pyrefly::definitions::Definitions;
use crate::pyrefly::definitions::DunderAllEntry;
use crate::pyrefly::sys_info::SysInfo;
use crate::traits::ExprExt;
use crate::traits::ModuleNameExt;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ExportType {
    Class,
    Function,
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Attribute {
    pub module: ModuleName,
    pub attr: Name,
}

impl Attribute {
    pub fn new(module: ModuleName, attr: &str) -> Self {
        Self {
            module,
            attr: Name::new(attr),
        }
    }

    /// Split a fully-qualified ModuleName into module (parent) and attr (last component).
    /// A name with no `.` (e.g. `"foo"`) has an empty module and the whole name as attr.
    pub fn from_module_name(name: &ModuleName) -> Self {
        let (module, attr) = match name.as_str().rsplit_once('.') {
            Some((module, attr)) => (ModuleName::from_str(module), attr),
            None => (ModuleName::empty(), name.as_str()),
        };
        Self {
            module,
            attr: Name::new(attr),
        }
    }

    /// Reconstruct the fully-qualified ModuleName (module.attr).
    pub fn as_module_name(&self) -> ModuleName {
        if self.module.as_str().is_empty() {
            ModuleName::from_str(self.attr.as_str())
        } else {
            self.module.append_str(self.attr.as_str())
        }
    }
}

/// Follow a chain of `Attribute` mappings transitively, returning the final resolved attribute.
/// Returns `None` if a cycle is detected.
pub(crate) fn resolve_chain<F>(start: &Attribute, lookup: F) -> Option<Attribute>
where
    F: Fn(&Attribute) -> Option<Attribute>,
{
    let mut current = start.clone();
    let mut seen = AHashSet::new();
    while let Some(next) = lookup(&current) {
        if seen.contains(&next) {
            return None;
        }
        seen.insert(current);
        current = next;
    }
    Some(current)
}

/// A `from S import *` statement: its source module, its location, and whether it
/// sits in an `except` handler. Handler stars are `ImportError` fallbacks that only
/// run when the primary import failed, so they rank below any non-handler star.
#[derive(Debug, Clone, Copy)]
struct StarImport {
    source: ModuleName,
    range: TextRange,
    is_fallback: bool,
}

/// How a name currently bound in a module got there. Ordering is precedence:
/// a non-fallback star beats a fallback one, and a later star beats an earlier
/// one, matching Python's sequential rebinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct StarRank {
    is_primary: bool,
    offset: TextSize,
}

type StarMembers = AHashMap<ModuleName, AHashMap<Name, Option<StarRank>>>;

fn star_bound_names(
    source: &ModuleName,
    all: &AHashMap<ModuleName, Vec<Name>>,
    members: &StarMembers,
) -> Vec<Name> {
    match all.get(source) {
        Some(names) => names.clone(),
        None => members
            .get(source)
            .map(|members| {
                members
                    .keys()
                    .filter(|name| !name.starts_with('_'))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Star imports bind in execution order, so a later star overwrites a name an earlier one bound,
/// except that an `except`-handler fallback never displaces a primary star because only one branch
/// runs. A non-star binding always shadows, regardless of position; this is deliberately more
/// conservative than Python, where a star overwrites a definition written above it.
fn star_can_replace(existing: Option<&Option<StarRank>>, rank: StarRank) -> bool {
    match existing {
        Some(None) => false,
        Some(Some(bound)) => *bound < rank,
        None => true,
    }
}

/// Re-exported names of one module: attribute name -> what it resolves to.
type ModuleReExports = AHashMap<Name, (Attribute, TextRange)>;

/// Flatten one module's re-exports into `(module, attr, resolution)` triples.
fn flatten_module_re_exports<'a>(
    (module, names): (&'a ModuleName, &'a ModuleReExports),
) -> impl Iterator<Item = (ModuleName, &'a Name, &'a (Attribute, TextRange))> {
    names
        .iter()
        .map(move |(attr, resolved)| (*module, attr, resolved))
}

#[derive(Debug)]
pub struct Exports {
    /// Map of each definition's fully-qualified name to what kind of definition
    /// it is. Deliberately flat, unlike `re_exports`: every lookup already holds
    /// an interned FQN, so nesting would force a split and a re-intern per
    /// lookup on the analyzer's hottest path.
    exports: AHashMap<ModuleName, ExportType>,
    /// Map of imported objects to their resolved names and locations, nested by
    /// re-exporting module. Every entry a single module contributes shares that
    /// module, so nesting lets `merge_all` move one sub-map per module instead
    /// of rehashing millions of individual `Attribute` keys.
    re_exports: AHashMap<ModuleName, ModuleReExports>,
    /// Map of module name to the contents of that module's `__all__`.
    all: AHashMap<ModuleName, Vec<Name>>,
    /// Map of fully-qualified function names to their return types (class names).
    /// Populated from stub file function return type annotations.
    return_types: AHashMap<ModuleName, ModuleName>,
    /// Map of importing module to the modules it star-imports (`from S import *`),
    /// with the location of each star import and whether it sits in an `except`
    /// handler. Consumed by `expand_star_re_exports` once all per-module exports
    /// are merged.
    star_imports: AHashMap<ModuleName, Vec<StarImport>>,
}

impl Exports {
    pub fn empty() -> Self {
        Self {
            exports: AHashMap::new(),
            re_exports: AHashMap::new(),
            all: AHashMap::new(),
            return_types: AHashMap::new(),
            star_imports: AHashMap::new(),
        }
    }

    /// `re_exporting_modules` sizes the outer `re_exports` map, so it counts
    /// modules rather than individual re-exports.
    pub fn with_capacity(
        exports: usize,
        re_exporting_modules: usize,
        all: usize,
        return_types: usize,
        star_imports: usize,
    ) -> Self {
        Self {
            exports: AHashMap::with_capacity(exports),
            re_exports: AHashMap::with_capacity(re_exporting_modules),
            all: AHashMap::with_capacity(all),
            return_types: AHashMap::with_capacity(return_types),
            star_imports: AHashMap::with_capacity(star_imports),
        }
    }

    pub fn new(
        parsed_module: &ParsedModule,
        import_graph: &ImportGraph,
        sys_info: &SysInfo,
    ) -> Self {
        let module_name = parsed_module.name;
        ExportsBuilder::new(module_name, import_graph, sys_info).build(parsed_module)
    }

    /// Build exports without filtering by import graph. Re-exports that refer to
    /// modules should be filtered later via `filter_module_re_exports`.
    pub fn new_unfiltered(parsed_module: &ParsedModule, sys_info: &SysInfo) -> Self {
        let module_name = parsed_module.name;
        ExportsBuilder::new_unfiltered(module_name, sys_info).build(parsed_module)
    }

    /// Follow re-export chains transitively to find the ultimate definition.
    /// Returns `None` if a cycle is detected.
    pub fn resolve_transitive(&self, name: &Attribute) -> Option<Attribute> {
        resolve_chain(name, |attr| self.resolve_imported_name(attr))
    }

    /// Check if a symbol is a class, following re-export chains transitively if needed.
    pub fn is_class(&self, name: &ModuleName) -> bool {
        let is_class_export = |n: &ModuleName| {
            self.exports
                .get(n)
                .is_some_and(|typ| matches!(typ, ExportType::Class))
        };
        if is_class_export(name) {
            return true;
        }
        let attr = Attribute::from_module_name(name);
        self.resolve_transitive(&attr)
            .is_some_and(|resolved| is_class_export(&resolved.as_module_name()))
    }

    /// Check if a symbol is a global variable.
    pub fn is_global(&self, name: &ModuleName) -> bool {
        self.exports
            .get(name)
            .is_some_and(|typ| matches!(typ, ExportType::Global))
    }

    /// Check if a symbol is a function.
    pub fn is_function(&self, name: &ModuleName) -> bool {
        self.exports
            .get(name)
            .is_some_and(|typ| matches!(typ, ExportType::Function))
    }

    /// Get the return type of a function, if known from stub annotations.
    pub fn get_return_type(&self, func_name: &ModuleName) -> Option<ModuleName> {
        self.return_types.get(func_name).copied()
    }

    /// Get the class a call to `func_name` evaluates to, from the function's
    /// stub-annotated return type. Re-export chains are followed, so a function
    /// imported from the module that stubs it keeps its annotation.
    pub fn resolve_return_class(&self, func_name: &ModuleName) -> Option<ModuleName> {
        let return_type = match self.get_return_type(func_name) {
            Some(return_type) => return_type,
            None => {
                let attr = Attribute::from_module_name(func_name);
                let source = self.resolve_transitive(&attr)?;
                self.get_return_type(&source.as_module_name())?
            }
        };
        self.is_class(&return_type).then_some(return_type)
    }

    /// Get an iterator to all exported symbols and their export info.
    pub fn get_exports(&self) -> impl Iterator<Item = (&ModuleName, &ExportType)> {
        self.exports.iter()
    }

    /// Get an iterator to all re-exported symbols and their definitions.
    pub fn get_re_exports(
        &self,
    ) -> impl Iterator<Item = (ModuleName, &Name, &(Attribute, TextRange))> {
        self.re_exports.iter().flat_map(flatten_module_re_exports)
    }

    /// Parallel iterator over all re-exported symbols and their definitions.
    pub fn par_re_exports(
        &self,
    ) -> impl ParallelIterator<Item = (ModuleName, &Name, &(Attribute, TextRange))> {
        self.re_exports
            .par_iter()
            .flat_map_iter(flatten_module_re_exports)
    }

    /// Get a symbol re-export information, what its original name and location is, assuming it is a
    /// re-export.
    pub fn get_re_export(&self, name: &Attribute) -> Option<&(Attribute, TextRange)> {
        self.re_exports.get(&name.module)?.get(&name.attr)
    }

    /// Check if a symbol is a re-export of another symbol.
    pub fn is_re_export(&self, name: &Attribute) -> bool {
        self.get_re_export(name).is_some()
    }

    fn insert_re_export_entry(
        &mut self,
        module: ModuleName,
        attr: Name,
        imported: Attribute,
        range: TextRange,
    ) {
        self.re_exports
            .entry(module)
            .or_default()
            .insert(attr, (imported, range));
    }

    /// Merge `other` into `self`. Consume `other`.
    pub fn merge(&mut self, other: Exports) {
        self.exports.extend(other.exports);
        extend_nested(&mut self.re_exports, other.re_exports);
        self.all.extend(other.all);
        self.return_types.extend(other.return_types);
        self.star_imports.extend(other.star_imports);
    }

    /// Merge a collection of per-module Exports into a single Exports.
    pub fn merge_all(all_exports: Vec<Exports>) -> Self {
        // Each input holds one module's exports, so its `re_exports` length is
        // the module count this contributes to the merged outer map.
        let (
            total_exports,
            re_exporting_modules,
            total_all,
            total_return_types,
            total_star_imports,
        ) = all_exports
            .iter()
            .fold((0, 0, 0, 0, 0), |(e, re, a, rt, si), exports| {
                (
                    e + exports.exports.len(),
                    re + exports.re_exports.len(),
                    a + exports.all.len(),
                    rt + exports.return_types.len(),
                    si + exports.star_imports.len(),
                )
            });

        let mut result = Self::with_capacity(
            total_exports,
            re_exporting_modules,
            total_all,
            total_return_types,
            total_star_imports,
        );
        for exports in all_exports {
            result.merge(exports);
        }
        result
    }

    /// Remove re-exports that refer to modules in the import graph.
    /// Used to filter unfiltered exports after the import graph is built.
    /// The predicate interns a ModuleName per re-export (as_module_name), which
    /// dominates this pass, so modules are filtered in parallel.
    pub fn filter_module_re_exports(&mut self, import_graph: &ImportGraph) {
        self.re_exports.par_iter_mut().for_each(|(_, names)| {
            names.retain(|_, (imported_attr, _)| {
                !import_graph.contains(&imported_attr.as_module_name())
            });
        });
        self.re_exports.retain(|_, names| !names.is_empty());
    }

    /// Index the members every `relevant` module already binds without a star.
    ///
    /// Only a handful of modules are relevant, but finding their members means
    /// scanning every exported and re-exported name in the program, so the scan
    /// runs in parallel. Every insert records the same `None`, which is why the
    /// per-thread indices can be merged in any order.
    fn seed_star_members(&self, relevant: &AHashMap<&str, ModuleName>) -> StarMembers {
        let (from_exports, from_re_exports) = rayon::join(
            || {
                self.exports
                    .par_iter()
                    .filter_map(|(name, _)| {
                        let (parent, attr) = name.as_str().rsplit_once('.')?;
                        Some((*relevant.get(parent)?, Name::new(attr)))
                    })
                    .fold(StarMembers::new, |mut acc, (module, attr)| {
                        acc.entry(module).or_default().insert(attr, None);
                        acc
                    })
                    .reduce(StarMembers::new, merge_nested_larger)
            },
            || {
                self.re_exports
                    .par_iter()
                    .filter(|(module, _)| relevant.contains_key(module.as_str()))
                    .fold(StarMembers::new, |mut acc, (module, names)| {
                        acc.entry(*module)
                            .or_default()
                            .extend(names.keys().map(|attr| (attr.clone(), None)));
                        acc
                    })
                    .reduce(StarMembers::new, merge_nested_larger)
            },
        );
        merge_nested_larger(from_exports, from_re_exports)
    }

    pub fn expand_star_re_exports(&mut self, import_graph: &ImportGraph) {
        if self.star_imports.is_empty() {
            return;
        }

        // Deterministic edge list (importer, star), ordered by the star's position
        // within its module.
        let mut edges: Vec<(ModuleName, StarImport)> = self
            .star_imports
            .iter()
            .flat_map(|(m, stars)| stars.iter().map(move |star| (*m, *star)))
            .collect();
        edges.sort_by(|a, b| {
            a.0.as_str()
                .cmp(b.0.as_str())
                .then_with(|| a.1.range.start().cmp(&b.1.range.start()))
                .then_with(|| a.1.source.as_str().cmp(b.1.source.as_str()))
        });

        // Only modules a star import touches need a member index: the source, to
        // enumerate the names it binds, and the importer, to resolve shadowing.
        // Mapping the name back to its interned `ModuleName` lets the seeding scan
        // recognise a module without re-interning it.
        let mut relevant: AHashMap<&str, ModuleName> = AHashMap::new();
        for (m, star) in &edges {
            relevant.insert(m.as_str(), *m);
            relevant.insert(star.source.as_str(), star.source);
        }

        // Index of each relevant module's member names, seeded from real exports
        // and explicit re-exports.
        // The value records what bounded the name: `None` for a non-star binding,
        // `Some(rank)` for the star that bound it.
        let mut members = self.seed_star_members(&relevant);

        loop {
            let mut changed = false;
            for (m, star) in &edges {
                let s = &star.source;
                // Names that `from S import *` binds: S.__all__ if declared, else
                // every non-underscore member of S.
                let names = star_bound_names(s, &self.all, &members);
                let rank = StarRank {
                    is_primary: !star.is_fallback,
                    offset: star.range.start(),
                };
                for n in names {
                    if !star_can_replace(members.get(m).and_then(|set| set.get(&n)), rank) {
                        continue;
                    }
                    let imported = Attribute::new(*s, n.as_str());
                    // Don't re-export a name that resolves to a submodule
                    if import_graph.contains(&imported.as_module_name()) {
                        continue;
                    }
                    self.re_exports
                        .entry(*m)
                        .or_default()
                        .insert(n.clone(), (imported, star.range));
                    members.entry(*m).or_default().insert(n, Some(rank));
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Get the `__all__` contents for a module, if it has one.
    pub fn get_all(&self, module: &ModuleName) -> Option<&Vec<Name>> {
        self.all.get(module)
    }

    pub fn resolve_imported_name(&self, name: &Attribute) -> Option<Attribute> {
        self.get_re_export(name).map(|(imp, _)| imp).cloned()
    }

    /// Iterate over all `__all__` entries across modules.
    pub fn iter_all(&self) -> impl Iterator<Item = (&ModuleName, &Vec<Name>)> {
        self.all.iter()
    }

    /// Iterate over all return type mappings (function -> return type class).
    pub fn iter_return_types(&self) -> impl Iterator<Item = (&ModuleName, &ModuleName)> {
        self.return_types.iter()
    }

    #[cfg(test)]
    pub fn insert_re_export(&mut self, exported: Attribute, imported: Attribute) {
        self.insert_re_export_entry(
            exported.module,
            exported.attr,
            imported,
            TextRange::default(),
        );
    }
}

struct ExportsBuilder<'a> {
    module_name: ModuleName,
    inner: Exports,
    import_graph: Option<&'a ImportGraph>,
    sys_info: &'a SysInfo,
}

impl<'a> ExportsBuilder<'a> {
    pub fn new(
        module_name: ModuleName,
        import_graph: &'a ImportGraph,
        sys_info: &'a SysInfo,
    ) -> Self {
        Self {
            module_name,
            inner: Exports::empty(),
            import_graph: Some(import_graph),
            sys_info,
        }
    }

    pub fn new_unfiltered(module_name: ModuleName, sys_info: &'a SysInfo) -> Self {
        Self {
            module_name,
            inner: Exports::empty(),
            import_graph: None,
            sys_info,
        }
    }

    pub fn build(mut self, parsed_module: &ParsedModule) -> Exports {
        let config = AnalysisConfig::new(*self.sys_info, None);
        let definitions = Definitions::new(
            &parsed_module.ast.body,
            self.module_name,
            parsed_module.is_init,
            parsed_module.is_stub(),
            &config,
        );

        for (name, def) in definitions.definitions.iter() {
            self.process_definition(name, def);
        }

        if !definitions.dunder_all.is_empty() {
            let all_names = Self::convert_dunder_all(&definitions.dunder_all);
            self.inner.all.insert(self.module_name, all_names);
        }

        if !definitions.import_all.is_empty() {
            let fallbacks = Self::star_ranges_in_except_handlers(&parsed_module.ast.body);
            let stars: Vec<StarImport> = definitions
                .import_all
                .iter()
                .map(|(module, range)| StarImport {
                    source: *module,
                    range: *range,
                    is_fallback: fallbacks.contains(range),
                })
                .collect();
            self.inner.star_imports.insert(self.module_name, stars);
        }

        if parsed_module.is_stub() {
            self.extract_return_types(&parsed_module.ast.body, &definitions, self.module_name);
        }

        self.inner
    }

    /// Locations of `from S import *` statements sitting in an `except` handler.
    /// These are `ImportError` fallbacks guarding a primary import, so only one of
    /// the two ever runs and the fallback must not displace the primary.
    fn star_ranges_in_except_handlers(body: &[Stmt]) -> AHashSet<TextRange> {
        fn walk(body: &[Stmt], in_handler: bool, out: &mut AHashSet<TextRange>) {
            for stmt in body {
                match stmt {
                    Stmt::ImportFrom(x) if in_handler => {
                        for a in &x.names {
                            if &a.name == "*" {
                                out.insert(a.name.range);
                            }
                        }
                    }
                    Stmt::Try(x) => {
                        walk(&x.body, in_handler, out);
                        for handler in &x.handlers {
                            let ExceptHandler::ExceptHandler(h) = handler;
                            walk(&h.body, true, out);
                        }
                        walk(&x.orelse, in_handler, out);
                        walk(&x.finalbody, in_handler, out);
                    }
                    Stmt::If(x) => {
                        walk(&x.body, in_handler, out);
                        for clause in &x.elif_else_clauses {
                            walk(&clause.body, in_handler, out);
                        }
                    }
                    Stmt::With(x) => walk(&x.body, in_handler, out),
                    Stmt::For(x) => {
                        walk(&x.body, in_handler, out);
                        walk(&x.orelse, in_handler, out);
                    }
                    Stmt::While(x) => {
                        walk(&x.body, in_handler, out);
                        walk(&x.orelse, in_handler, out);
                    }
                    Stmt::Match(x) => {
                        for case in &x.cases {
                            walk(&case.body, in_handler, out);
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut out = AHashSet::new();
        walk(body, false, &mut out);
        out
    }

    fn convert_dunder_all(dunder_all: &[DunderAllEntry]) -> Vec<Name> {
        let mut names = Vec::new();
        for entry in dunder_all {
            match entry {
                DunderAllEntry::Name(_, name) => names.push(name.clone()),
                DunderAllEntry::Remove(_, name) => names.retain(|n| n != name),
                DunderAllEntry::Module(_, _) => {}
            }
        }
        names
    }

    fn add_export(&mut self, name: ModuleName, typ: ExportType) {
        self.inner.exports.insert(name, typ);
    }

    fn add_re_export(&mut self, exported: Attribute, imported: Attribute, range: TextRange) {
        let is_module = self
            .import_graph
            .is_some_and(|ig| ig.contains(&imported.as_module_name()));
        if !is_module {
            self.inner
                .insert_re_export_entry(exported.module, exported.attr, imported, range);
        }
    }

    fn symbol_kind_to_export_type(kind: &SymbolKind) -> ExportType {
        match kind {
            SymbolKind::Class => ExportType::Class,
            SymbolKind::Function | SymbolKind::Method => ExportType::Function,
            _ => ExportType::Global,
        }
    }

    fn extract_return_types(
        &mut self,
        body: &[Stmt],
        definitions: &Definitions,
        scope: ModuleName,
    ) {
        for stmt in body {
            match stmt {
                Stmt::FunctionDef(func) => {
                    if let Some(returns) = &func.returns {
                        if let Some(rt) = self.resolve_return_type(returns, definitions) {
                            let func_fqn = scope.append(&func.name.id);
                            self.inner.return_types.insert(func_fqn, rt);
                        }
                    }
                }
                Stmt::ClassDef(cls) => {
                    let class_scope = scope.append(&cls.name.id);
                    self.extract_return_types(&cls.body, definitions, class_scope);
                }
                _ => stmt.recurse(&mut |s| {
                    self.extract_return_types(std::slice::from_ref(s), definitions, scope);
                }),
            }
        }
    }

    fn resolve_return_type(
        &self,
        annotation: &Expr,
        definitions: &Definitions,
    ) -> Option<ModuleName> {
        match annotation {
            Expr::Name(name) => {
                if let Some(def) = definitions.definitions.get(&name.id) {
                    match &def.style {
                        DefinitionStyle::Unannotated(SymbolKind::Class)
                        | DefinitionStyle::Annotated(SymbolKind::Class, _) => {
                            Some(self.module_name.append(&name.id))
                        }
                        DefinitionStyle::Import(from_module) => Some(from_module.append(&name.id)),
                        DefinitionStyle::ImportAs(from_module, original_name) => {
                            Some(from_module.append(original_name))
                        }
                        DefinitionStyle::ImportAsEq(from_module) => {
                            Some(from_module.append(&name.id))
                        }
                        _ => None,
                    }
                } else {
                    // Name not in definitions — treat as a builtin (e.g. int, str, list).
                    // Validated via is_class() at lookup time.
                    Some(ModuleName::builtins().append(&name.id))
                }
            }
            Expr::Attribute(attr) => {
                let base_name = attr.value.as_name_expr()?;
                let def = definitions.definitions.get(&base_name.id)?;
                match &def.style {
                    DefinitionStyle::ImportModule(_) => annotation.full_name(),
                    DefinitionStyle::Import(from_module) => {
                        Some(from_module.append(&base_name.id).append(&attr.attr.id))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn process_definition(&mut self, name: &Name, def: &Definition) {
        match &def.style {
            DefinitionStyle::Unannotated(kind) | DefinitionStyle::Annotated(kind, _) => {
                let qualname = self.module_name.append(name);
                self.add_export(qualname, Self::symbol_kind_to_export_type(kind));
            }

            DefinitionStyle::Import(from_module) | DefinitionStyle::ImportAsEq(from_module) => {
                let exported = Attribute::new(self.module_name, name);
                let imported = Attribute::new(*from_module, name);
                self.add_re_export(exported, imported, def.range);
            }

            DefinitionStyle::ImportAs(from_module, original_name) => {
                let exported = Attribute::new(self.module_name, name);
                let imported = Attribute::new(*from_module, original_name);
                self.add_re_export(exported, imported, def.range);
            }

            DefinitionStyle::ImportModule(_)
            | DefinitionStyle::ImportInvalidRelative
            | DefinitionStyle::MutableCapture(_)
            | DefinitionStyle::ImplicitGlobal
            | DefinitionStyle::Delete => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use pyrefly_python::module_name::ModuleName;
    use ruff_python_ast::name::Name;

    use super::ExportsBuilder;
    use crate::imports::ImportGraph;
    use crate::module_parser::parse_source;
    use crate::pyrefly::sys_info::SysInfo;
    use crate::traits::SysInfoExt;

    fn get_dunder_all(code: &str) -> Option<Vec<Name>> {
        let module_name = ModuleName::from_str("test");
        let parsed = parse_source(code, module_name, false);
        let import_graph = ImportGraph::new();
        let sys_info = SysInfo::lg_default();
        let exports = ExportsBuilder::new(module_name, &import_graph, &sys_info).build(&parsed);
        exports.get_all(&module_name).cloned()
    }

    fn names(strs: &[&str]) -> Vec<Name> {
        strs.iter().map(Name::new).collect()
    }

    #[test]
    fn test_list_assignment() {
        assert_eq!(
            get_dunder_all("__all__ = ['foo', 'bar']"),
            Some(names(&["foo", "bar"]))
        );
    }

    #[test]
    fn test_tuple_assignment() {
        assert_eq!(
            get_dunder_all("__all__ = ('foo', 'bar')"),
            Some(names(&["foo", "bar"]))
        );
    }

    #[test]
    fn test_annotated_assignment() {
        assert_eq!(
            get_dunder_all("__all__: list[str] = ['foo', 'bar']"),
            Some(names(&["foo", "bar"]))
        );
    }

    #[test]
    fn test_aug_assign() {
        let code = "\
__all__ = ['foo']
__all__ += ['bar', 'baz']
";
        assert_eq!(get_dunder_all(code), Some(names(&["foo", "bar", "baz"])));
    }

    #[test]
    fn test_extend() {
        let code = "\
__all__ = ['foo']
__all__.extend(['bar', 'baz'])
";
        assert_eq!(get_dunder_all(code), Some(names(&["foo", "bar", "baz"])));
    }

    #[test]
    fn test_append() {
        let code = "\
__all__ = ['foo']
__all__.append('bar')
";
        assert_eq!(get_dunder_all(code), Some(names(&["foo", "bar"])));
    }

    #[test]
    fn test_empty_list() {
        assert_eq!(get_dunder_all("__all__ = []"), None);
    }

    #[test]
    fn test_no_dunder_all() {
        assert_eq!(get_dunder_all("x = 1"), None);
    }

    #[test]
    fn test_reassignment_overwrites() {
        let code = "\
__all__ = ['foo', 'bar']
__all__ = ['baz']
";
        assert_eq!(get_dunder_all(code), Some(names(&["baz"])));
    }

    #[test]
    fn test_non_string_elements_ignored() {
        assert_eq!(
            get_dunder_all("__all__ = ['foo', 42, 'bar']"),
            Some(names(&["foo", "bar"]))
        );
    }

    #[test]
    fn test_non_list_value() {
        assert_eq!(get_dunder_all("__all__ = some_function()"), None);
    }

    #[test]
    fn test_multiple_operations() {
        let code = "\
__all__ = ['a']
__all__ += ['b']
__all__.extend(['c'])
__all__.append('d')
";
        assert_eq!(get_dunder_all(code), Some(names(&["a", "b", "c", "d"])));
    }

    use super::Exports;
    use crate::module_parser::parse_pyi;

    fn get_stub_return_types(code: &str) -> Exports {
        let module_name = ModuleName::from_str("test");
        let parsed = parse_pyi(code, module_name, false);
        let import_graph = ImportGraph::new();
        let sys_info = SysInfo::lg_default();
        ExportsBuilder::new(module_name, &import_graph, &sys_info).build(&parsed)
    }

    #[test]
    fn test_return_type_local_class() {
        let code = r#"
class MyClass:
    pass

def make() -> MyClass: ...
"#;
        let exports = get_stub_return_types(code);
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.make")),
            Some(ModuleName::from_str("test.MyClass")),
        );
    }

    #[test]
    fn test_return_type_no_annotation() {
        let code = r#"
def make(): ...
"#;
        let exports = get_stub_return_types(code);
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.make")),
            None,
        );
    }

    #[test]
    fn test_return_type_imported_class() {
        let code = r#"
from other import Widget

def create() -> Widget: ...
"#;
        let exports = get_stub_return_types(code);
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.create")),
            Some(ModuleName::from_str("other.Widget")),
        );
    }

    #[test]
    fn test_return_type_aliased_import() {
        let code = r#"
from other import Original as Renamed

def make() -> Renamed: ...
"#;
        let exports = get_stub_return_types(code);
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.make")),
            Some(ModuleName::from_str("other.Original")),
        );
    }

    #[test]
    fn test_return_type_dotted_module_import() {
        let code = r#"
import other

def get() -> other.Result: ...
"#;
        let exports = get_stub_return_types(code);
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.get")),
            Some(ModuleName::from_str("other.Result")),
        );
    }

    #[test]
    fn test_return_type_method_in_class() {
        let code = r#"
class A:
    pass

class Factory:
    def create(self) -> A: ...
"#;
        let exports = get_stub_return_types(code);
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.Factory.create")),
            Some(ModuleName::from_str("test.A")),
        );
    }

    #[test]
    fn test_return_type_not_extracted_from_py() {
        let code = r#"
class MyClass:
    pass

def make() -> MyClass: ...
"#;
        let module_name = ModuleName::from_str("test");
        let parsed = parse_source(code, module_name, false);
        let import_graph = ImportGraph::new();
        let sys_info = SysInfo::lg_default();
        let exports = ExportsBuilder::new(module_name, &import_graph, &sys_info).build(&parsed);
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.make")),
            None,
        );
    }

    #[test]
    fn test_return_type_builtin() {
        let code = r#"
def make() -> int: ...
"#;
        let exports = get_stub_return_types(code);
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.make")),
            Some(ModuleName::from_str("builtins.int")),
        );
    }

    #[test]
    fn test_return_type_generic_skipped() {
        let code = r#"
class MyClass:
    pass

def make() -> list[MyClass]: ...
"#;
        let exports = get_stub_return_types(code);
        // Generic types (subscripts) are not resolved
        assert_eq!(
            exports.get_return_type(&ModuleName::from_str("test.make")),
            None,
        );
    }

    #[test]
    fn test_is_function() {
        let code = r#"
class MyClass:
    pass

def my_func(): ...

x = 1
"#;
        let exports = get_stub_return_types(code);
        assert!(exports.is_function(&ModuleName::from_str("test.my_func")));
        assert!(!exports.is_function(&ModuleName::from_str("test.MyClass")));
        assert!(!exports.is_function(&ModuleName::from_str("test.x")));
    }

    fn make_star_exports(modules: &[(&str, &str)]) -> Exports {
        use crate::config::AnalysisConfig;
        use crate::test_lib::TestSources;
        // Expansion is scoped to stubs, so the star cases are exercised as `.pyi`.
        let stub_names: Vec<&str> = modules.iter().map(|(name, _)| *name).collect();
        let sources = TestSources::new_with_stubs(modules, &stub_names);
        let config = AnalysisConfig::default();
        ImportGraph::make_with_exports(&sources, &config).1
    }

    fn attr(module: &str, name: &str) -> super::Attribute {
        super::Attribute::new(ModuleName::from_str(module), name)
    }

    #[test]
    fn test_star_reexport_basic() {
        // `from b import *` re-exports every public name of b under a.
        let b = "class C: ...\ndef f(): ...\nx = 1\n";
        let exports = make_star_exports(&[("a", "from b import *\n"), ("b", b)]);
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "C")),
            Some(attr("b", "C"))
        );
        assert!(exports.is_re_export(&attr("a", "f")));
        assert!(exports.is_re_export(&attr("a", "x")));
        assert!(exports.is_class(&ModuleName::from_str("a.C")));
    }

    #[test]
    fn test_star_reexport_respects_dunder_all() {
        // `from b import *` binds only b.__all__ when it is declared.
        let b = "__all__ = [\"C\"]\nclass C: ...\nclass D: ...\n";
        let exports = make_star_exports(&[("a", "from b import *\n"), ("b", b)]);
        assert!(exports.is_re_export(&attr("a", "C")));
        assert!(!exports.is_re_export(&attr("a", "D")));
    }

    #[test]
    fn test_star_reexport_skips_private() {
        // Without __all__, `import *` excludes underscore-prefixed names.
        let b = "def pub(): ...\ndef _hidden(): ...\n";
        let exports = make_star_exports(&[("a", "from b import *\n"), ("b", b)]);
        assert!(exports.is_re_export(&attr("a", "pub")));
        assert!(!exports.is_re_export(&attr("a", "_hidden")));
    }

    #[test]
    fn test_star_reexport_local_shadows() {
        // A local definition of C in a shadows the star import of b.C.
        let b = "class C: ...\n";
        let a = "from b import *\nclass C: ...\n";
        let exports = make_star_exports(&[("a", a), ("b", b)]);
        assert!(!exports.is_re_export(&attr("a", "C")));
        assert!(exports.is_class(&ModuleName::from_str("a.C")));
    }

    #[test]
    fn test_star_reexport_chain() {
        // Chained stars a <- b <- c resolve transitively to the definition in c.
        let exports = make_star_exports(&[
            ("a", "from b import *\n"),
            ("b", "from c import *\n"),
            ("c", "class Widget: ...\n"),
        ]);
        assert_eq!(
            exports.resolve_transitive(&attr("a", "Widget")),
            Some(attr("c", "Widget"))
        );
        assert!(exports.is_class(&ModuleName::from_str("a.Widget")));
    }

    #[test]
    fn test_star_reexport_duplicate_name_last_wins() {
        // When two star imports in the same module export the same name, the later
        // star overwrites the earlier binding, as it does at runtime.
        let exports = make_star_exports(&[
            ("a", "from b import *\nfrom c import *\n"),
            ("b", "class X: ...\n"),
            ("c", "class X: ...\n"),
        ]);
        assert!(exports.is_re_export(&attr("a", "X")));
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "X")),
            Some(attr("c", "X"))
        );
    }

    #[test]
    fn test_star_reexport_duplicate_name_resolved_late_still_last_wins() {
        // c only acquires X on a later fixpoint pass (via its own star). The later
        // star must still win once the name shows up, not lose to the earlier one
        // that bound X first.
        let exports = make_star_exports(&[
            ("a", "from b import *\nfrom c import *\n"),
            ("b", "class X: ...\n"),
            ("c", "from d import *\n"),
            ("d", "class X: ...\n"),
        ]);
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "X")),
            Some(attr("c", "X"))
        );
        assert_eq!(
            exports.resolve_transitive(&attr("a", "X")),
            Some(attr("d", "X"))
        );
    }

    #[test]
    fn test_star_reexport_except_fallback_loses_to_primary() {
        // `except ImportError: from c import *` guards the primary star in the try
        // body. Only one branch runs, so the fallback must not win the name despite
        // sitting later in the file.
        let a = "try:\n    from b import *\nexcept ImportError:\n    from c import *\n";
        let exports =
            make_star_exports(&[("a", a), ("b", "class X: ...\n"), ("c", "class X: ...\n")]);
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "X")),
            Some(attr("b", "X"))
        );
    }

    #[test]
    fn test_star_reexport_except_fallback_binds_names_primary_lacks() {
        // Deprioritizing a fallback must not silence it: it is still the only
        // source for names the primary star does not provide.
        let a = "try:\n    from b import *\nexcept ImportError:\n    from c import *\n";
        let exports = make_star_exports(&[
            ("a", a),
            ("b", "class X: ...\n"),
            ("c", "class X: ...\nclass Y: ...\n"),
        ]);
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "X")),
            Some(attr("b", "X"))
        );
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "Y")),
            Some(attr("c", "Y"))
        );
    }

    #[test]
    fn test_star_reexport_nested_except_fallback_stays_fallback() {
        // A `try` nested inside an `except` handler is still a fallback: its body only
        // runs because the primary import failed, so its stars must not displace the
        // primary despite sitting later in the file.
        let a = "try:\n    from b import *\nexcept ImportError:\n    try:\n        from c import *\n    except ImportError:\n        from d import *\n";
        let exports = make_star_exports(&[
            ("a", a),
            ("b", "class X: ...\n"),
            ("c", "class X: ...\n"),
            ("d", "class X: ...\n"),
        ]);
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "X")),
            Some(attr("b", "X"))
        );
    }

    #[test]
    fn test_star_reexport_match_arm_in_except_is_fallback() {
        // `match` is the other block that can wrap a module-level star. One inside an
        // `except` handler is still a fallback, like any other nesting.
        let a = "import sys\ntry:\n    from b import *\nexcept ImportError:\n    match sys.version_info:\n        case _:\n            from c import *\n";
        let exports =
            make_star_exports(&[("a", a), ("b", "class X: ...\n"), ("c", "class X: ...\n")]);
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "X")),
            Some(attr("b", "X"))
        );
    }

    #[test]
    fn test_star_reexport_multi_star_cycle_terminates() {
        // Cyclic star imports with several stars per module rebind the same name on
        // successive passes. Each rebind raises that name's rank, and a module's stars
        // have distinct ranks, so the fixpoint converges instead of looping.
        let exports = make_star_exports(&[
            ("a", "from b import *\nfrom c import *\n"),
            ("b", "from c import *\nfrom a import *\n"),
            ("c", "from a import *\nfrom b import *\nclass Z: ...\n"),
        ]);
        // Z is defined in c; a real definition is never displaced by a star.
        assert_eq!(
            exports.resolve_transitive(&attr("a", "Z")),
            Some(attr("c", "Z"))
        );
        assert_eq!(
            exports.resolve_transitive(&attr("b", "Z")),
            Some(attr("c", "Z"))
        );
    }

    #[test]
    fn test_star_reexport_mutual_cycle_terminates() {
        // Mutually star-importing modules must not loop forever. Each module's
        // public name flows to the other, and expansion reaches a fixpoint.
        let exports = make_star_exports(&[
            ("a", "from b import *\nclass A: ...\n"),
            ("b", "from a import *\nclass B: ...\n"),
        ]);
        assert_eq!(
            exports.resolve_imported_name(&attr("a", "B")),
            Some(attr("b", "B"))
        );
        assert_eq!(
            exports.resolve_imported_name(&attr("b", "A")),
            Some(attr("a", "A"))
        );
    }
}
