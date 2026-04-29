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

use ahash::AHashMap;
use ahash::AHashSet;
use pyrefly_python::module_name::ModuleName;
use pyrefly_python::symbol_kind::SymbolKind;
use pyrefly_util::visit::Visit;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;
use serde::Deserialize;
use serde::Serialize;

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

#[derive(Debug)]
pub struct Export {
    pub typ: ExportType,
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
    pub fn from_module_name(name: &ModuleName) -> Self {
        let module = name.parent().unwrap_or_else(ModuleName::empty);
        let components = name.components();
        let attr = components
            .last()
            .map(Name::new)
            .unwrap_or_else(|| Name::new(""));
        Self { module, attr }
    }

    /// Reconstruct the fully-qualified ModuleName (module.attr).
    pub fn as_module_name(&self) -> ModuleName {
        if self.module.as_str().is_empty() {
            ModuleName::from_str(self.attr.as_str())
        } else {
            self.module.append(&self.attr)
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

#[derive(Debug)]
pub struct Exports {
    /// Map of definitions to the name of their containing module.
    exports: AHashMap<ModuleName, Export>,
    /// Map of imported objects to their resolved names and locations.
    re_exports: AHashMap<Attribute, (Attribute, TextRange)>,
    /// Map of module name to the contents of that module's `__all__`.
    all: AHashMap<ModuleName, Vec<Name>>,
    /// Map of fully-qualified function names to their return types (class names).
    /// Populated from stub file function return type annotations.
    return_types: AHashMap<ModuleName, ModuleName>,
}

impl Exports {
    pub fn empty() -> Self {
        Self {
            exports: AHashMap::new(),
            re_exports: AHashMap::new(),
            all: AHashMap::new(),
            return_types: AHashMap::new(),
        }
    }

    /// Create with pre-allocated capacity based on expected number of modules.
    /// Estimates ~4 exports and ~10 re-exports per module based on profiling data.
    pub fn with_capacity(num_modules: usize) -> Self {
        Self {
            exports: AHashMap::with_capacity(num_modules * 4),
            re_exports: AHashMap::with_capacity(num_modules * 10),
            all: AHashMap::with_capacity(num_modules),
            return_types: AHashMap::new(),
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
        resolve_chain(name, |attr| {
            self.re_exports.get(attr).map(|(a, _)| a.clone())
        })
    }

    /// Check if a symbol is a class, following re-export chains transitively if needed.
    pub fn is_class(&self, name: &ModuleName) -> bool {
        let is_class_export = |n: &ModuleName| {
            self.exports
                .get(n)
                .is_some_and(|e| matches!(e.typ, ExportType::Class))
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
            .is_some_and(|e| matches!(e.typ, ExportType::Global))
    }

    /// Check if a symbol is a function.
    pub fn is_function(&self, name: &ModuleName) -> bool {
        self.exports
            .get(name)
            .is_some_and(|e| matches!(e.typ, ExportType::Function))
    }

    /// Get the return type of a function, if known from stub annotations.
    pub fn get_return_type(&self, func_name: &ModuleName) -> Option<ModuleName> {
        self.return_types.get(func_name).copied()
    }

    /// Get an iterator to all exported symbols and their export info.
    pub fn get_exports(&self) -> impl Iterator<Item = (&ModuleName, &Export)> {
        self.exports.iter()
    }

    /// Get an iterator to all re-exported symbols and their definitions.
    pub fn get_re_exports(&self) -> impl Iterator<Item = (&Attribute, &(Attribute, TextRange))> {
        self.re_exports.iter()
    }

    /// Get a symbol re-export information, what its original name and location is, assuming it is a
    /// re-export.
    pub fn get_re_export(&self, name: &Attribute) -> Option<&(Attribute, TextRange)> {
        self.re_exports.get(name)
    }

    /// Check if a symbol is a re-export of another symbol.
    pub fn is_re_export(&self, name: &Attribute) -> bool {
        self.re_exports.contains_key(name)
    }

    /// Merge `other` into `self`. Consume `other`.
    pub fn merge(&mut self, other: Exports) {
        self.exports.extend(other.exports);
        self.re_exports.extend(other.re_exports);
        self.all.extend(other.all);
        self.return_types.extend(other.return_types);
    }

    /// Merge a collection of per-module Exports into a single Exports.
    pub fn merge_all(all_exports: Vec<Exports>) -> Self {
        let num_modules = all_exports.len();
        let mut result = Self::with_capacity(num_modules);
        for exports in all_exports {
            result.merge(exports);
        }
        result
    }

    /// Remove re-exports that refer to modules in the import graph.
    /// Used to filter unfiltered exports after the import graph is built.
    pub fn filter_module_re_exports(&mut self, import_graph: &ImportGraph) {
        self.re_exports.retain(|_, (imported_attr, _)| {
            !import_graph.contains(&imported_attr.as_module_name())
        });
    }

    /// Get the `__all__` contents for a module, if it has one.
    pub fn get_all(&self, module: &ModuleName) -> Option<&Vec<Name>> {
        self.all.get(module)
    }

    pub fn resolve_imported_name(&self, name: &Attribute) -> Option<Attribute> {
        self.re_exports.get(name).map(|(imp, _)| imp).cloned()
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
        self.re_exports
            .insert(exported, (imported, TextRange::default()));
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
        let definitions = Definitions::new(
            &parsed_module.ast.body,
            self.module_name,
            parsed_module.is_init,
            parsed_module.is_stub(),
            self.sys_info,
        );

        for (name, def) in definitions.definitions.iter() {
            self.process_definition(name, def);
        }

        if !definitions.dunder_all.is_empty() {
            let all_names = Self::convert_dunder_all(&definitions.dunder_all);
            self.inner.all.insert(self.module_name, all_names);
        }

        if parsed_module.is_stub() {
            self.extract_return_types(&parsed_module.ast.body, &definitions, self.module_name);
        }

        self.inner
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
        self.inner.exports.insert(name, Export { typ });
    }

    fn add_re_export(&mut self, exported: Attribute, imported: Attribute, range: TextRange) {
        let is_module = self
            .import_graph
            .is_some_and(|ig| ig.contains(&imported.as_module_name()));
        if !is_module {
            self.inner.re_exports.insert(exported, (imported, range));
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
        let qualname = self.module_name.append(name);

        match &def.style {
            DefinitionStyle::Unannotated(kind) | DefinitionStyle::Annotated(kind, _) => {
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
}
