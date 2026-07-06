/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::config::AnalysisConfig;
    use lifeguard::imports::ImportGraph;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::test_lib::TestSources;
    use lifeguard::test_lib::module_names;

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
        let config = AnalysisConfig::default();
        ImportGraph::make(&sources, &config)
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

    #[test]
    fn test_parse_failed_module_treated_as_missing() {
        let sources = TestSources::new(&[("a", "import b")]).with_parse_errors(&["b"]);
        let b = ModuleName::from_str("b");

        let config = AnalysisConfig::default();
        let g = ImportGraph::make(&sources, &config);

        assert!(
            !g.contains(&b),
            "parse-failed module should not be a graph node"
        );

        assert!(
            g.has_missing_import(&ModuleName::from_str("a"), &b),
            "import of parse-failed module should be in the missing set"
        );
    }

    #[test]
    fn test_import_from_parse_failed_module() {
        let sources =
            TestSources::new(&[("a", "from broken import foo")]).with_parse_errors(&["broken"]);
        let broken = ModuleName::from_str("broken");

        let config = AnalysisConfig::default();
        let g = ImportGraph::make(&sources, &config);

        assert!(!g.contains(&broken));
        assert!(
            g.has_missing_import(&ModuleName::from_str("a"), &broken),
            "from-import of parse-failed module should be in the missing set"
        );
    }

    #[test]
    fn test_parse_failed_submodule_in_package() {
        let sources =
            TestSources::new(&[("a", "pass"), ("c", "import a.b")]).with_parse_errors(&["a.b"]);
        let a_b = ModuleName::from_str("a.b");

        let config = AnalysisConfig::default();
        let g = ImportGraph::make(&sources, &config);

        assert!(g.contains(&ModuleName::from_str("a")));
        assert!(!g.contains(&a_b));
        assert_deps(&g, "c", vec!["a"]);
        assert!(
            g.has_missing_import(&ModuleName::from_str("c"), &a_b),
            "import of parse-failed submodule should be in the missing set"
        );
    }

    #[test]
    fn test_resolve_missing_adds_edge_to_known_ancestor() {
        let mut g = ImportGraph::new();
        let a = ModuleName::from_str("a");
        let c = ModuleName::from_str("c");
        g.graph.add_node(&a);
        g.graph.add_node(&c);
        // `a` imports `c.d.e`, which is not a node; `c` is its nearest known ancestor.
        g.add_missing(&a, ModuleName::from_str("c.d.e"));
        g.resolve_missing_to_known();
        assert_deps(&g, "a", vec!["c"]);
        assert!(
            !g.has_missing_import(&a, &ModuleName::from_str("c.d.e")),
            "resolved import should no longer be missing"
        );
    }

    #[test]
    fn test_resolve_missing_unresolvable_stays_missing() {
        let mut g = ImportGraph::new();
        let a = ModuleName::from_str("a");
        g.graph.add_node(&a);
        // No ancestor of `zzz.qqq` is a node, so it cannot be resolved.
        g.add_missing(&a, ModuleName::from_str("zzz.qqq"));
        g.resolve_missing_to_known();
        assert_deps(&g, "a", vec![]);
        assert!(
            g.has_missing_import(&a, &ModuleName::from_str("zzz.qqq")),
            "unresolvable import should remain in the missing set"
        );
    }

    #[test]
    fn test_resolve_missing_does_not_create_self_loop() {
        let mut g = ImportGraph::new();
        let xy = ModuleName::from_str("x.y");
        g.graph.add_node(&xy);
        // A name-import like `from x.y import Foo`: `x.y.Foo` resolves to `x.y`, the
        // importing module itself. This must not add a self-loop edge.
        g.add_missing(&xy, ModuleName::from_str("x.y.Foo"));
        g.resolve_missing_to_known();
        assert_deps(&g, "x.y", vec![]);
        assert!(
            !g.has_missing_import(&xy, &ModuleName::from_str("x.y.Foo")),
            "self-resolving import should no longer be missing"
        );
    }

    #[test]
    fn test_resolve_missing_does_not_duplicate_existing_edge() {
        let mut g = ImportGraph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        g.graph.add_node(&a);
        g.graph.add_node(&b);
        // `a` already imports `b` directly.
        g.graph.add_edge(&a, &b);
        // A missing `b.sub` resolves to `b`, which is already a neighbor; resolving it
        // must not create a parallel edge.
        g.add_missing(&a, ModuleName::from_str("b.sub"));
        g.resolve_missing_to_known();
        // `get_imports` yields one entry per edge, so a parallel edge would show `b` twice.
        assert_deps(&g, "a", vec!["b"]);
    }
}
