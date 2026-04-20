/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::imports::ImportGraph;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::pyrefly::sys_info::SysInfo;
    use lifeguard::test_lib::TestSources;
    use lifeguard::test_lib::module_names;
    use lifeguard::traits::SysInfoExt;

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
        let missing = g.get_missing_imports(&ModuleName::from_str("b")).unwrap();
        assert_eq!(missing.len(), 1);
        assert!(missing.contains(&ModuleName::from_str("c")));
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
