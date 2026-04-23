/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::PathBuf;
use std::sync::Arc;

use pyrefly_python::ast::Ast;
use pyrefly_python::module::Module;
use pyrefly_python::module_name::ModuleName;
use pyrefly_python::module_path::ModulePath;
use pyrefly_python::symbol_kind::SymbolKind;
use pyrefly_util::lined_buffer::DisplayPos;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprAttribute;
use ruff_python_ast::ExprName;
use ruff_python_ast::ExprSubscript;
use ruff_python_ast::ModModule;
use ruff_python_ast::PySourceType;
use ruff_python_ast::Stmt;
use ruff_python_ast::name::Name;
use ruff_python_parser::ParseError;
use ruff_text_size::TextSize;

use crate::pyrefly::definitions::Definition;
use crate::pyrefly::definitions::DefinitionStyle;
use crate::pyrefly::definitions::Definitions;
use crate::pyrefly::sys_info::PythonPlatform;
use crate::pyrefly::sys_info::PythonVersion;
use crate::pyrefly::sys_info::SysInfo;

pub trait AstExt {
    fn parse_py(contents: &str) -> (ModModule, Vec<ParseError>);
}

impl AstExt for Ast {
    fn parse_py(contents: &str) -> (ModModule, Vec<ParseError>) {
        // TODO: the third component is UnsupportedSyntaxError; do we need it?
        let (ret, err, _) = Self::parse(contents, PySourceType::Python);
        (ret, err)
    }
}

pub trait SysInfoExt {
    // Default configuration for lifeguard, since SysInfo::default() is pyrefly-specific.
    fn lg_default() -> Self;
}

impl SysInfoExt for SysInfo {
    fn lg_default() -> Self {
        SysInfo::new_without_type_checking(PythonVersion::default(), PythonPlatform::default())
    }
}

pub trait DefinitionExt {
    fn get_imported_module_name(&self) -> Option<ModuleName>;
    fn is_import(&self) -> bool;
    fn is_param(&self) -> bool;
}

impl DefinitionExt for Definition {
    fn get_imported_module_name(&self) -> Option<ModuleName> {
        match self.style {
            DefinitionStyle::ImportAs(module, _)
            | DefinitionStyle::ImportAsEq(module)
            | DefinitionStyle::Import(module)
            | DefinitionStyle::ImportModule(module) => Some(module),
            _ => None,
        }
    }

    fn is_import(&self) -> bool {
        self.get_imported_module_name().is_some()
    }

    fn is_param(&self) -> bool {
        matches!(
            self.style,
            DefinitionStyle::Annotated(SymbolKind::Parameter, _)
                | DefinitionStyle::Unannotated(SymbolKind::Parameter)
        )
    }
}

pub trait DefinitionsExt {
    fn make(x: &[Stmt], module_name: ModuleName, is_init: bool, sys_info: &SysInfo) -> Self;
}

impl DefinitionsExt for Definitions {
    fn make(x: &[Stmt], module_name: ModuleName, is_init: bool, sys_info: &SysInfo) -> Self {
        Self::new(x, module_name, is_init, false /* is_stub */, sys_info)
    }
}

pub struct ParentIter<'a> {
    s: &'a str,
    dot_positions: Vec<usize>,
    index: usize,
}

impl Iterator for ParentIter<'_> {
    type Item = (ModuleName, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.dot_positions.len() {
            return None;
        }
        let pos = self.dot_positions[self.dot_positions.len() - 1 - self.index];
        self.index += 1;
        Some((ModuleName::from_str(&self.s[..pos]), pos))
    }
}

pub trait ModuleNameExt {
    fn empty() -> Self;

    fn concat(&self, other: &Self) -> Self;

    fn append_str(&self, other: &str) -> Self;

    fn split_attr(&self) -> Option<(Self, Name)>
    where
        Self: Sized;

    /// Iterate over parent module names from longest to shortest.
    /// For "a.b.c.d", yields "a.b.c", "a.b", "a".
    fn iter_parents(&self) -> ParentIter<'_>;
}

impl ModuleNameExt for ModuleName {
    fn empty() -> Self {
        Self::from_str("")
    }

    fn concat(&self, other: &Self) -> Self {
        self.append_str(other.as_str())
    }

    fn append_str(&self, other: &str) -> Self {
        // This could be completed in one line with format, but this approach
        // is more performant
        let self_str = self.as_str();
        let mut s = String::with_capacity(self_str.len() + 1 + other.len());
        s.push_str(self_str);
        s.push('.');
        s.push_str(other);
        Self::from_string(s)
    }

    fn split_attr(&self) -> Option<(Self, Name)> {
        let (obj, attr) = self.as_str().rsplit_once(".")?;
        Some((Self::from_str(obj), Name::new(attr)))
    }

    fn iter_parents(&self) -> ParentIter<'_> {
        let s = self.as_str();
        let dot_positions: Vec<usize> = s.match_indices('.').map(|(i, _)| i).collect();
        ParentIter {
            s,
            dot_positions,
            index: 0,
        }
    }
}

pub trait ExprExt {
    fn base_name(&self) -> Option<Name>;
    fn full_name(&self) -> Option<ModuleName>;
    fn as_var_name(&self) -> Option<Name>;
}

impl ExprExt for Expr {
    fn base_name(&self) -> Option<Name> {
        match self {
            Expr::Name(ExprName { id, .. }) => Some(id.clone()),
            Expr::Attribute(ExprAttribute { value, .. })
            | Expr::Subscript(ExprSubscript { value, .. }) => value.base_name(),
            _ => None,
        }
    }

    fn full_name(&self) -> Option<ModuleName> {
        match self {
            Expr::Name(ExprName { id, .. }) => Some(ModuleName::from_name(id)),
            Expr::Attribute(ExprAttribute { value, attr, .. }) => {
                value.full_name().map(|x| x.append_str(&attr.id))
            }
            _ => None,
        }
    }

    fn as_var_name(&self) -> Option<Name> {
        match self {
            Expr::Name(ExprName { id, .. }) => Some(id.clone()),
            _ => None,
        }
    }
}

pub trait ModuleExt {
    fn make(name: &str, code: &str) -> Self;
    fn get_line_no(&self, offset: TextSize) -> usize;
}

impl ModuleExt for Module {
    fn make(name: &str, code: &str) -> Self {
        let module_name = ModuleName::from_str(name);
        let module_path = ModulePath::filesystem(PathBuf::from(format!("{}.py", name)));
        Self::new(module_name, module_path, Arc::new(code.to_string()))
    }

    fn get_line_no(&self, offset: TextSize) -> usize {
        let pos = self.display_pos(offset);
        let line_no = match pos {
            DisplayPos::Source { line, .. } | DisplayPos::Notebook { line, .. } => line.get(),
        };
        line_no as usize
    }
}

// We cannot implement AsRef<str> for ModuleName since we do not own the type (we are using the
// definition from pyrefly). So we define our own AsStr trait to write generic code on types with
// an .as_str() method.
pub trait AsStr {
    fn as_str(&self) -> &str;
}

impl AsStr for ModuleName {
    fn as_str(&self) -> &str {
        self.as_str()
    }
}

impl AsStr for Name {
    fn as_str(&self) -> &str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use pyrefly_python::ast::Ast;
    use ruff_python_ast::ExprCall;
    use ruff_python_ast::Stmt;
    use ruff_python_ast::StmtExpr;
    use starlark_map::small_set::SmallSet;

    use super::*;
    use crate::pyrefly::definitions::Definitions;
    use crate::pyrefly::sys_info::SysInfo;
    use crate::traits::SysInfoExt;

    fn calculate_definitions(
        contents: &str,
        module_name: ModuleName,
        is_init: bool,
    ) -> Definitions {
        Definitions::new(
            &Ast::parse_py(contents).0.body,
            module_name,
            is_init,
            false, /* is_stub */
            &SysInfo::lg_default(),
        )
    }

    #[test]
    fn test_get_imports() {
        let defs = calculate_definitions(
            r#"
 import a
 import derp.c.d
 import bar.b
 from os import path
 from os.path import join
 from path import *
 "#,
            ModuleName::from_str("test"),
            true,
        );
        let expected_module_names = SmallSet::from_iter([
            ModuleName::from_str("a"),
            ModuleName::from_str("derp.c.d"),
            ModuleName::from_str("bar.b"),
            ModuleName::from_str("os"),
            ModuleName::from_str("os.path"),
            ModuleName::from_str("path"),
        ]);
        let imports: SmallSet<ModuleName> = defs
            .definitions
            .values()
            .filter_map(|x| x.get_imported_module_name())
            .chain(defs.import_all.keys().copied())
            .collect();
        assert_eq!(expected_module_names, imports);
    }

    fn get_call_func(s: &Stmt) -> &Expr {
        let Stmt::Expr(e) = s else { panic!() };
        let StmtExpr { value, .. } = e;
        let Expr::Call(ExprCall { func, .. }, ..) = value.as_ref() else {
            panic!()
        };
        func
    }

    #[test]
    fn test_full_name() {
        let code = r#"
a.f(10)
b.g.h[i](1, 2)
"#;
        let ast = Ast::parse_py(code).0;
        let a = get_call_func(&ast.body[0]);
        let b = get_call_func(&ast.body[1]);
        assert_eq!(a.base_name().unwrap().as_str(), "a");
        assert_eq!(b.base_name().unwrap().as_str(), "b");
        assert_eq!(a.full_name().unwrap().as_str(), "a.f");
        assert_eq!(b.full_name(), None);
    }
}
