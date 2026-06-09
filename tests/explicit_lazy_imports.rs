/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use ahash::AHashSet;
    use lifeguard::config::AnalysisConfig;
    use lifeguard::imports::ImportGraph;
    use lifeguard::project;
    use lifeguard::project::CachingMode;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::pyrefly::sys_info::PythonVersion;
    use lifeguard::test_lib::TestSources;

    fn py315() -> PythonVersion {
        PythonVersion::new(3, 15, 0)
    }

    fn side_effect_imports(modules: Vec<(&str, &str)>) -> Vec<(ModuleName, AHashSet<ModuleName>)> {
        let sources = TestSources::new_with_version(&modules, py315());
        let config = AnalysisConfig::with_python_version(py315(), None);
        let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &config);
        let output = project::run_analysis(
            &sources,
            &exports,
            &import_graph,
            &config,
            CachingMode::Disabled,
        );

        output
            .side_effect_imports
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .collect()
    }

    fn side_effects_for(modules: Vec<(&str, &str)>, module_name: &str) -> AHashSet<ModuleName> {
        let all = side_effect_imports(modules);
        let name = ModuleName::from_str(module_name);
        all.into_iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
            .unwrap_or_default()
    }

    fn has_side_effect(modules: Vec<(&str, &str)>, module_name: &str, import: &str) -> bool {
        side_effects_for(modules, module_name).contains(&ModuleName::from_str(import))
    }

    // --- Basic lazy import behavior ---

    #[test]
    fn test_eager_import_unused_is_side_effect() {
        assert!(has_side_effect(
            vec![("main", "import a"), ("a", "x = 1")],
            "main",
            "a",
        ));
    }

    #[test]
    fn test_lazy_import_unused_is_not_side_effect() {
        assert!(!has_side_effect(
            vec![("main", "lazy import a"), ("a", "x = 1")],
            "main",
            "a",
        ));
    }

    #[test]
    fn test_lazy_import_used_is_not_side_effect() {
        let code = "lazy import a\nx = a.foo";
        assert!(!has_side_effect(
            vec![("main", code), ("a", "foo = 1")],
            "main",
            "a",
        ));
    }

    #[test]
    fn test_eager_import_used_is_not_side_effect() {
        let code = "import a\nx = a.foo";
        assert!(!has_side_effect(
            vec![("main", code), ("a", "foo = 1")],
            "main",
            "a",
        ));
    }

    // --- Lazy + eager interaction (order independent) ---

    #[test]
    fn test_lazy_then_eager_import_unused_is_side_effect() {
        let code = "lazy import a\nimport a";
        assert!(has_side_effect(
            vec![("main", code), ("a", "x = 1")],
            "main",
            "a",
        ));
    }

    #[test]
    fn test_eager_then_lazy_import_unused_is_side_effect() {
        let code = "import a\nlazy import a";
        assert!(has_side_effect(
            vec![("main", code), ("a", "x = 1")],
            "main",
            "a",
        ));
    }

    // --- from-import variants ---

    #[test]
    fn test_lazy_from_import_unused_is_not_side_effect() {
        assert!(!has_side_effect(
            vec![("main", "lazy from a import b"), ("a", "b = 1")],
            "main",
            "a",
        ));
    }

    #[test]
    fn test_eager_from_import_unused_is_side_effect() {
        assert!(has_side_effect(
            vec![("main", "from a import b"), ("a", "b = 1")],
            "main",
            "a",
        ));
    }

    // --- Submodule interaction: lazy import a; import a.b ---

    #[test]
    fn test_lazy_parent_with_eager_submodule_import() {
        let code = "lazy import a\nimport a.b";
        let se = side_effects_for(
            vec![("main", code), ("a", "x = 1"), ("a.b", "y = 2")],
            "main",
        );
        // `a` is only imported lazily at module level, so not a side-effect import.
        // `a.b` is imported eagerly and unused, so it IS a side-effect import.
        // (a's side effects flow through the a.b import chain in the import graph)
        assert!(!se.contains(&ModuleName::from_str("a")));
        assert!(se.contains(&ModuleName::from_str("a.b")));
    }

    // --- Multiple lazy imports ---

    #[test]
    fn test_all_lazy_imports_unused_no_side_effects() {
        let code = "lazy import a\nlazy import b";
        let se = side_effects_for(vec![("main", code), ("a", "x = 1"), ("b", "y = 2")], "main");
        assert!(se.is_empty());
    }

    #[test]
    fn test_mixed_lazy_and_eager_unused() {
        let code = "lazy import a\nimport b";
        let se = side_effects_for(vec![("main", code), ("a", "x = 1"), ("b", "y = 2")], "main");
        assert!(!se.contains(&ModuleName::from_str("a")));
        assert!(se.contains(&ModuleName::from_str("b")));
    }
}
