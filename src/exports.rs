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
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;

use crate::imports::ImportGraph;
use crate::module_parser::ParsedModule;
use crate::pyrefly::definitions::Definition;
use crate::pyrefly::definitions::DefinitionStyle;
use crate::pyrefly::definitions::Definitions;
use crate::pyrefly::definitions::DunderAllEntry;
use crate::pyrefly::sys_info::SysInfo;

#[derive(Debug, Clone, Copy)]
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
        let module = name.parent().unwrap_or_else(|| ModuleName::from_str(""));
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

#[derive(Debug)]
pub struct Exports {
    /// Map of definitions to the name of their containing module.
    exports: AHashMap<ModuleName, Export>,
    /// Map of imported objects to their resolved names and locations.
    re_exports: AHashMap<Attribute, (Attribute, TextRange)>,
    /// Map of module name to the contents of that module's `__all__`.
    all: AHashMap<ModuleName, Vec<Name>>,
}

impl Exports {
    pub fn empty() -> Self {
        Self {
            exports: AHashMap::new(),
            re_exports: AHashMap::new(),
            all: AHashMap::new(),
        }
    }

    /// Create with pre-allocated capacity based on expected number of modules.
    /// Estimates ~4 exports and ~10 re-exports per module based on profiling data.
    pub fn with_capacity(num_modules: usize) -> Self {
        Self {
            exports: AHashMap::with_capacity(num_modules * 4),
            re_exports: AHashMap::with_capacity(num_modules * 10),
            all: AHashMap::with_capacity(num_modules),
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

    /// Check if a symbol is a class.
    pub fn is_class(&self, name: &ModuleName) -> bool {
        self.exports
            .get(name)
            .is_some_and(|e| matches!(e.typ, ExportType::Class))
    }

    /// Check if a symbol is a global variable.
    pub fn is_global(&self, name: &ModuleName) -> bool {
        self.exports
            .get(name)
            .is_some_and(|e| matches!(e.typ, ExportType::Global))
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

    pub fn get_definition_source_name(&self, name: &Attribute) -> Option<Attribute> {
        // recurse through re-exports until we find the original name where the object was defined
        let mut current = name.clone();
        let mut seen = AHashSet::new();
        while let Some((next, _)) = self.re_exports.get(&current) {
            if seen.contains(next) {
                return None;
            }
            seen.insert(current);
            current = next.clone();
        }
        Some(current)
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
            self.sys_info,
        );

        for (name, def) in definitions.definitions.iter() {
            self.process_definition(name, def);
        }

        if !definitions.dunder_all.is_empty() {
            let all_names = Self::convert_dunder_all(&definitions.dunder_all);
            self.inner.all.insert(self.module_name, all_names);
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
}
