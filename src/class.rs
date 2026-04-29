/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use ahash::AHashMap;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_python_ast::name::Name;

use crate::traits::ModuleNameExt;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    InstanceVar,
    ClassVar,
    InstanceMethod,
    ClassMethod,
    StaticMethod,
    Property,
    PropertySetter,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub kind: FieldKind,
    pub name: Name,
}

#[derive(Debug, Clone)]
pub struct Class {
    pub module: ModuleName,
    pub name: ModuleName,
    pub decorators: Vec<ModuleName>,
    pub bases: Vec<ModuleName>,
    pub metaclass: Option<ModuleName>,
    pub fields: Vec<Field>,
    // We should mark a class unsafe if we cannot resolve one of its bases or its metaclass
    pub has_unknown_base: bool,
    pub has_unknown_metaclass: bool,
}

impl Class {
    pub fn empty(module: ModuleName) -> Self {
        Self {
            module,
            name: ModuleName::empty(),
            decorators: vec![],
            bases: vec![],
            metaclass: None,
            fields: vec![],
            has_unknown_base: false,
            has_unknown_metaclass: false,
        }
    }

    pub fn get_field(&self, name: &Name) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == *name)
    }
}

#[derive(Debug, Clone)]
pub struct ClassTable {
    table: AHashMap<ModuleName, Class>,
}

impl ClassTable {
    pub fn empty() -> Self {
        Self {
            table: AHashMap::new(),
        }
    }

    pub fn new(table: AHashMap<ModuleName, Class>) -> Self {
        Self { table }
    }

    /// Create with pre-allocated capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            table: AHashMap::with_capacity(capacity),
        }
    }

    /// Consume the ClassTable and return the underlying map.
    pub fn into_inner(self) -> AHashMap<ModuleName, Class> {
        self.table
    }

    pub fn contains(&self, name: &ModuleName) -> bool {
        self.table.contains_key(name)
    }

    pub fn lookup(&self, name: &ModuleName) -> Option<&Class> {
        self.table.get(name)
    }

    pub fn lookup_str(&self, name: &str) -> Option<&Class> {
        self.table.get(&ModuleName::from_str(name))
    }

    pub(crate) fn par_keys(&self) -> impl ParallelIterator<Item = &ModuleName> {
        self.table.par_iter().map(|(k, _)| k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_info::build_definitions_and_classes;
    use crate::module_parser::parse_source;
    use crate::pyrefly::sys_info::SysInfo;
    use crate::traits::SysInfoExt;

    pub fn make_class_table(code: &str) -> ClassTable {
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let sys_info = SysInfo::lg_default();
        let (_definitions, classes) = build_definitions_and_classes(&parsed_module, &sys_info);
        classes
    }

    #[test]
    fn test_basic() {
        let code = r#"
class B:
    pass

class C:
    pass

class A(B, metaclass=C):
    x = 10

    def f(self):
        pass

    @classmethod
    def g(self):
        pass

    @staticmethod
    def h(self):
        pass
"#;
        let cls_table = make_class_table(code);
        let cls = cls_table.lookup_str("test.A").unwrap();
        assert_eq!(cls.name, ModuleName::from_str("A"));
        assert_eq!(cls.bases, vec![ModuleName::from_str("test.B")]);
        assert_eq!(cls.metaclass, Some(ModuleName::from_str("test.C")));
        assert_eq!(
            cls.fields[0],
            Field {
                kind: FieldKind::ClassVar,
                name: "x".into()
            }
        );
        assert_eq!(
            cls.fields[1],
            Field {
                kind: FieldKind::InstanceMethod,
                name: "f".into()
            }
        );
        assert_eq!(
            cls.fields[2],
            Field {
                kind: FieldKind::ClassMethod,
                name: "g".into()
            }
        );
        assert_eq!(
            cls.fields[3],
            Field {
                kind: FieldKind::StaticMethod,
                name: "h".into()
            }
        );
    }

    #[test]
    fn test_properties() {
        let code = r#"
class A:
    @property
    def x(self):
        pass

    @x.setter
    def x(self, value):
        pass
"#;
        let cls_table = make_class_table(code);
        let cls = cls_table.lookup_str("test.A").unwrap();
        assert_eq!(
            cls.fields[0],
            Field {
                kind: FieldKind::Property,
                name: "x".into()
            }
        );
        assert_eq!(
            cls.fields[1],
            Field {
                kind: FieldKind::PropertySetter,
                name: "x".into()
            }
        );
    }
}
