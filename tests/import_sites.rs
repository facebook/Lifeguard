/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::config::AnalysisConfig;
    use lifeguard::hasher::AHashSet;
    use lifeguard::imports::ImportGraph;
    use lifeguard::imports::ImportSite;
    use lifeguard::imports::ImportTarget;
    use lifeguard::imports::LazySyntax;
    use lifeguard::imports::NotAllowedReason;
    use lifeguard::imports::import_sites;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::pyrefly::sys_info::PythonVersion;
    use lifeguard::source_map::ModuleProvider;
    use lifeguard::test_lib::TestSources;

    fn version() -> PythonVersion {
        PythonVersion::new(3, 15, 0)
    }

    fn collect(modules: &[(&str, &str)], module: &str) -> (String, Vec<ImportSite>, ImportGraph) {
        collect_from(TestSources::new_with_version(modules, version()), module)
    }

    fn collect_from(sources: TestSources, module: &str) -> (String, Vec<ImportSite>, ImportGraph) {
        let config = AnalysisConfig::with_python_version(version(), None);
        let (graph, _, _) = ImportGraph::make_with_exports(&sources, &config);
        let name = ModuleName::from_str(module);
        let ast = sources.parse(&name).unwrap();
        let sites = import_sites(ast.as_parsed().unwrap(), &graph, &config);
        (sources.get_code(&name).unwrap().to_owned(), sites, graph)
    }

    fn assert_site(
        source: &str,
        site: &ImportSite,
        statement: &str,
        is_lazy: bool,
        lazy_syntax: LazySyntax,
        targets: &[(&str, bool)],
    ) {
        assert_eq!(&source[site.range], statement, "{source}");
        assert_eq!(site.is_lazy, is_lazy, "{source}");
        assert_eq!(site.lazy_syntax, lazy_syntax, "{source}");
        assert_eq!(
            site.targets,
            targets
                .iter()
                .map(|(name, ambiguous)| ImportTarget {
                    module: ModuleName::from_str(name),
                    ambiguous: *ambiguous,
                })
                .collect::<Vec<_>>(),
            "{source}",
        );
    }

    fn assert_placement(code: &str, expected: LazySyntax) {
        let (source, sites, _) = collect(&[("main", code), ("x", "")], "main");
        assert_eq!(sites.len(), 1, "{code}");
        assert_site(
            &source,
            &sites[0],
            "import x",
            false,
            expected,
            &[("x", false)],
        );
    }

    #[test]
    fn test_site_targets_match_graph_edges() {
        let code = "import a.b.c\nfrom pkg import sub\nfrom . import sibling\nfrom .mod import x\nfrom pkg import attr\nlazy import lazy_dep\nlazy from pkg import lazy_sub\ndef f():\n    import fn_dep\ntry:\n    import try_dep\nexcept Exception:\n    from pkg import error_attr\nimport importlib\nimportlib.import_module('dynamic_dep')";
        let (source, sites, graph) = collect(
            &[
                ("pkg.consumer", code),
                ("pkg", "attr = 1\nerror_attr = 2"),
                ("pkg.sub", ""),
                ("pkg.lazy_sub", ""),
                ("pkg.sibling", ""),
                ("pkg.mod", "x = 1"),
                ("a", ""),
                ("a.b", ""),
                ("a.b.c", ""),
                ("lazy_dep", ""),
                ("fn_dep", ""),
                ("try_dep", ""),
                ("importlib", ""),
                ("dynamic_dep", ""),
            ],
            "pkg.consumer",
        );
        let expected = [
            (
                "import a.b.c",
                false,
                LazySyntax::Allowed,
                vec![("a", false), ("a.b", false), ("a.b.c", false)],
            ),
            (
                "from pkg import sub",
                false,
                LazySyntax::Allowed,
                vec![("pkg", false), ("pkg.sub", false)],
            ),
            (
                "from . import sibling",
                false,
                LazySyntax::Allowed,
                vec![("pkg", false), ("pkg.sibling", false)],
            ),
            (
                "from .mod import x",
                false,
                LazySyntax::Allowed,
                vec![("pkg.mod", false), ("pkg.mod.x", true)],
            ),
            (
                "from pkg import attr",
                false,
                LazySyntax::Allowed,
                vec![("pkg", false), ("pkg.attr", true)],
            ),
            (
                "lazy import lazy_dep",
                true,
                LazySyntax::Allowed,
                vec![("lazy_dep", false)],
            ),
            (
                "lazy from pkg import lazy_sub",
                true,
                LazySyntax::Allowed,
                vec![("pkg", false), ("pkg.lazy_sub", false)],
            ),
            (
                "import fn_dep",
                false,
                LazySyntax::NotAllowed(NotAllowedReason::Function),
                vec![("fn_dep", false)],
            ),
            (
                "import try_dep",
                false,
                LazySyntax::NotAllowed(NotAllowedReason::Try),
                vec![("try_dep", false)],
            ),
            (
                "from pkg import error_attr",
                false,
                LazySyntax::NotAllowed(NotAllowedReason::Try),
                vec![("pkg", false), ("pkg.error_attr", true)],
            ),
            (
                "import importlib",
                false,
                LazySyntax::Allowed,
                vec![("importlib", false)],
            ),
        ];
        assert_eq!(sites.len(), expected.len());
        for (site, (statement, is_lazy, syntax, targets)) in sites.iter().zip(expected) {
            assert_site(&source, site, statement, is_lazy, syntax, &targets);
        }
        let targets = |ambiguous| {
            sites
                .iter()
                .flat_map(|site| &site.targets)
                .filter(|target| target.ambiguous == ambiguous)
                .map(|target| target.module)
                .collect::<AHashSet<_>>()
        };
        let module = ModuleName::from_str("pkg.consumer");
        let dynamic = ModuleName::from_str("dynamic_dep");
        assert!(graph.get_imports(&module).any(|name| *name == dynamic));
        assert_eq!(
            targets(false),
            graph
                .get_imports(&module)
                .copied()
                .filter(|name| *name != dynamic)
                .collect()
        );
        assert_eq!(
            targets(true),
            graph.get_ambiguous_imports(&module).unwrap().clone()
        );
    }

    #[test]
    fn test_unparseable_modules_keep_build_classification() {
        let sources = TestSources::new_with_version(
            &[
                ("main", "from pkg import broken\nfrom broken import foo"),
                ("pkg", ""),
            ],
            version(),
        )
        .with_parse_errors(&["pkg.broken", "broken"]);
        let (source, sites, graph) = collect_from(sources, "main");
        assert_eq!(sites.len(), 2);
        assert_site(
            &source,
            &sites[0],
            "from pkg import broken",
            false,
            LazySyntax::Allowed,
            &[("pkg", false), ("pkg.broken", false)],
        );
        assert_site(
            &source,
            &sites[1],
            "from broken import foo",
            false,
            LazySyntax::Allowed,
            &[("broken", false), ("broken.foo", true)],
        );
        let main = ModuleName::from_str("main");
        assert_eq!(
            graph.get_missing_imports(&main).unwrap().clone(),
            ["pkg.broken", "broken"]
                .map(ModuleName::from_str)
                .into_iter()
                .collect()
        );
        assert_eq!(
            graph.get_ambiguous_imports(&main).unwrap().clone(),
            [ModuleName::from_str("broken.foo")].into_iter().collect()
        );
    }

    #[test]
    fn test_targets_keep_repeats_and_missing_modules() {
        let (source, sites, graph) = collect(
            &[
                ("main", "import a.b, a.c\nimport ext.sub"),
                ("a", ""),
                ("a.b", ""),
                ("a.c", ""),
            ],
            "main",
        );
        assert_eq!(sites.len(), 2);
        assert_site(
            &source,
            &sites[0],
            "import a.b, a.c",
            false,
            LazySyntax::Allowed,
            &[("a", false), ("a.b", false), ("a", false), ("a.c", false)],
        );
        assert_site(
            &source,
            &sites[1],
            "import ext.sub",
            false,
            LazySyntax::Allowed,
            &[("ext", false), ("ext.sub", false)],
        );
        assert!(graph.has_missing_import(
            &ModuleName::from_str("main"),
            &ModuleName::from_str("ext.sub")
        ));
    }

    #[test]
    fn test_relative_imports_in_init() {
        let (source, sites, _) = collect(
            &[
                ("pkg", "from . import sibling\nfrom .mod import x"),
                ("pkg.sibling", ""),
                ("pkg.mod", "x = 1"),
            ],
            "pkg",
        );
        assert_eq!(sites.len(), 2);
        assert_site(
            &source,
            &sites[0],
            "from . import sibling",
            false,
            LazySyntax::Allowed,
            &[("pkg", false), ("pkg.sibling", false)],
        );
        assert_site(
            &source,
            &sites[1],
            "from .mod import x",
            false,
            LazySyntax::Allowed,
            &[("pkg.mod", false), ("pkg.mod.x", true)],
        );
    }

    #[test]
    fn test_allowed_module_placements() {
        for code in [
            "import x",
            "if condition:\n    import x",
            "with manager:\n    import x",
            "if __name__ == '__main__':\n    import x",
        ] {
            assert_placement(code, LazySyntax::Allowed);
        }
    }

    #[test]
    fn test_try_placements() {
        for code in [
            "try:\n    import x\nexcept Exception:\n    pass",
            "try:\n    pass\nexcept Exception:\n    import x",
            "try:\n    if condition:\n        import x\nexcept Exception:\n    pass",
        ] {
            assert_placement(code, LazySyntax::NotAllowed(NotAllowedReason::Try));
        }
    }

    #[test]
    fn test_function_and_class_placements() {
        for (code, reason) in [
            ("def f():\n    import x", NotAllowedReason::Function),
            ("async def f():\n    import x", NotAllowedReason::Function),
            ("class C:\n    import x", NotAllowedReason::Class),
        ] {
            assert_placement(code, LazySyntax::NotAllowed(reason));
        }
    }

    #[test]
    fn test_context_is_restored_after_recursion() {
        let code = "def f():\n    try:\n        import x\n    finally:\n        pass\n    import x\nclass C:\n    def method(self):\n        import x\n    import x\ntry:\n    class D:\n        import x\n    import x\nfinally:\n    pass\nimport x";
        let (source, sites, _) = collect(&[("main", code), ("x", "")], "main");
        let expected = [
            LazySyntax::NotAllowed(NotAllowedReason::Try),
            LazySyntax::NotAllowed(NotAllowedReason::Function),
            LazySyntax::NotAllowed(NotAllowedReason::Function),
            LazySyntax::NotAllowed(NotAllowedReason::Class),
            LazySyntax::NotAllowed(NotAllowedReason::Class),
            LazySyntax::NotAllowed(NotAllowedReason::Try),
            LazySyntax::Allowed,
        ];
        assert_eq!(sites.len(), expected.len());
        for (site, syntax) in sites.iter().zip(expected) {
            assert_site(&source, site, "import x", false, syntax, &[("x", false)]);
        }
    }

    #[test]
    fn test_star_follows_enclosing_context() {
        for (code, reason) in [
            ("from json import *", NotAllowedReason::Star),
            (
                "def f():\n    from json import *",
                NotAllowedReason::Function,
            ),
            (
                "try:\n    from json import *\nexcept Exception:\n    pass",
                NotAllowedReason::Try,
            ),
        ] {
            let (source, sites, _) = collect(&[("main", code), ("json", "")], "main");
            assert_eq!(sites.len(), 1, "{code}");
            assert_site(
                &source,
                &sites[0],
                "from json import *",
                false,
                LazySyntax::NotAllowed(reason),
                &[("json", false)],
            );
        }
    }

    #[test]
    fn test_future_import_is_not_allowed() {
        let (source, sites, _) = collect(
            &[
                ("main", "from __future__ import annotations"),
                ("__future__", "annotations = 1"),
            ],
            "main",
        );
        assert_eq!(sites.len(), 1);
        assert_site(
            &source,
            &sites[0],
            "from __future__ import annotations",
            false,
            LazySyntax::NotAllowed(NotAllowedReason::Future),
            &[("__future__", false), ("__future__.annotations", true)],
        );
    }

    #[test]
    fn test_relative_future_name_is_allowed() {
        let (source, sites, _) = collect(
            &[
                ("pkg", ""),
                ("pkg.main", "from .__future__ import x"),
                ("pkg.__future__", "x = 1"),
            ],
            "pkg.main",
        );
        assert_eq!(sites.len(), 1);
        assert_site(
            &source,
            &sites[0],
            "from .__future__ import x",
            false,
            LazySyntax::Allowed,
            &[("pkg.__future__", false), ("pkg.__future__.x", true)],
        );
    }

    #[test]
    fn test_version_pruned_branch_has_no_site() {
        let (source, sites, _) = collect(
            &[
                (
                    "main",
                    "import sys\nif sys.version_info < (3, 0):\n    import gone\nelse:\n    import kept",
                ),
                ("sys", ""),
                ("gone", ""),
                ("kept", ""),
            ],
            "main",
        );
        assert_eq!(sites.len(), 2);
        assert_site(
            &source,
            &sites[0],
            "import sys",
            false,
            LazySyntax::Allowed,
            &[("sys", false)],
        );
        assert_site(
            &source,
            &sites[1],
            "import kept",
            false,
            LazySyntax::Allowed,
            &[("kept", false)],
        );
    }
}
