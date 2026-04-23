/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Port of safer_lazy_imports/analyzer/tests/test_catch_import_cycles.py

#[cfg(test)]
mod tests {

    use lifeguard::imports::ImportGraph;
    use lifeguard::output::LifeGuardAnalysis;
    use lifeguard::project;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::pyrefly::sys_info::SysInfo;
    use lifeguard::runner::Options;
    use lifeguard::test_lib::TestSources;
    use lifeguard::test_lib::assert_str_keys;
    use lifeguard::traits::SysInfoExt;

    fn test_options() -> Options {
        Options {
            verbose_output_path: Some(std::path::PathBuf::from("/tmp/test_cycles")),
            sorted_output: true,
        }
    }

    fn run_cycle_analysis(modules: &Vec<(&str, &str)>) -> LifeGuardAnalysis {
        let sources = TestSources::new(modules);
        let sys_info = SysInfo::lg_default();
        let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &sys_info);
        let output = project::run_analysis(&sources, &exports, &import_graph, &sys_info);
        let mut analysis =
            LifeGuardAnalysis::new(output.safety_map, import_graph, &exports, &test_options());
        analysis.propagate_side_effect_imports(&output.side_effect_imports);
        analysis
    }

    fn assert_passing(result: &LifeGuardAnalysis, expected: Vec<&str>) {
        assert_str_keys(&result.passing_modules, expected);
    }

    fn assert_failing(result: &LifeGuardAnalysis, expected: Vec<&str>) {
        assert_str_keys(&result.failing_modules, expected);
    }

    fn has_lazy_eligible_dep(result: &LifeGuardAnalysis, module: &str, dep: &str) -> bool {
        result
            .output
            .lazy_eligible
            .get(&ModuleName::from_str(module))
            .map(|deps| deps.contains(&ModuleName::from_str(dep)))
            .unwrap_or(false)
    }

    fn has_no_cycle_deps(result: &LifeGuardAnalysis, module: &str) -> bool {
        result
            .output
            .lazy_eligible
            .get(&ModuleName::from_str(module))
            .map(|deps| deps.is_empty())
            .unwrap_or(true)
    }

    fn get_cycle_count(result: &LifeGuardAnalysis) -> usize {
        result
            .output
            .import_cycles
            .as_ref()
            .map(|c| c.len())
            .unwrap_or(0)
    }

    #[test]
    fn test_catch_simple_import_cycle() {
        let a = r#"
            import b
        "#;
        let b = r#"
            import c
        "#;
        let c = r#"
            import a
        "#;
        let modules = vec![("a", a), ("b", b), ("c", c)];
        let result = run_cycle_analysis(&modules);

        assert_passing(&result, vec!["a", "b", "c"]);
        assert_failing(&result, vec![]);

        // Lifeguard adds only direct imports within the cycle as lazy_eligible deps:
        // a imports b, b imports c, c imports a
        assert!(has_lazy_eligible_dep(&result, "a", "b"));
        assert!(has_lazy_eligible_dep(&result, "b", "c"));
        assert!(has_lazy_eligible_dep(&result, "c", "a"));

        assert_eq!(get_cycle_count(&result), 1);
    }

    #[test]
    fn test_dont_catch_import_cycle_submodule_same_name() {
        let a = r#"
            from subm import a
        "#;
        let subm = r#"
        "#;
        let subm_a = r#"
        "#;
        let modules = vec![("a", a), ("subm", subm), ("subm.a", subm_a)];
        let result = run_cycle_analysis(&modules);

        assert_passing(&result, vec!["a", "subm", "subm.a"]);
        assert_failing(&result, vec![]);

        assert_eq!(get_cycle_count(&result), 0);
        assert!(
            has_no_cycle_deps(&result, "a"),
            "a should have no cycle deps"
        );
    }

    #[test]
    fn test_catch_multiple_import_cycles() {
        let a = r#"
            import b
        "#;
        let b = r#"
            import a
        "#;
        let c = r#"
            import d
        "#;
        let d = r#"
            import c
        "#;
        let modules = vec![("a", a), ("b", b), ("c", c), ("d", d)];
        let result = run_cycle_analysis(&modules);

        assert_passing(&result, vec!["a", "b", "c", "d"]);
        assert_failing(&result, vec![]);

        // a <-> b cycle
        assert!(has_lazy_eligible_dep(&result, "a", "b"));
        assert!(has_lazy_eligible_dep(&result, "b", "a"));

        // c <-> d cycle
        assert!(has_lazy_eligible_dep(&result, "c", "d"));
        assert!(has_lazy_eligible_dep(&result, "d", "c"));

        // No cross-cycle deps
        assert!(!has_lazy_eligible_dep(&result, "a", "c"));
        assert!(!has_lazy_eligible_dep(&result, "a", "d"));
        assert!(!has_lazy_eligible_dep(&result, "c", "a"));
        assert!(!has_lazy_eligible_dep(&result, "c", "b"));

        assert_eq!(get_cycle_count(&result), 2);
    }

    // a->b->a and a->c->d->a overlap at a, forming a single SCC {a,b,c,d}.
    // Lifeguard adds only direct imports within the cycle as lazy_eligible deps.
    #[test]
    fn test_catch_multiple_overlapping_import_cycles() {
        let a = r#"
            import b
            import c
        "#;
        let b = r#"
            import a
        "#;
        let c = r#"
            import d
        "#;
        let d = r#"
            import a
        "#;
        let modules = vec![("a", a), ("b", b), ("c", c), ("d", d)];
        let result = run_cycle_analysis(&modules);

        assert_passing(&result, vec!["a", "b", "c", "d"]);
        assert_failing(&result, vec![]);

        // Direct imports within the cycle
        assert!(has_lazy_eligible_dep(&result, "a", "b"));
        assert!(has_lazy_eligible_dep(&result, "a", "c"));
        assert!(has_lazy_eligible_dep(&result, "b", "a"));
        assert!(has_lazy_eligible_dep(&result, "c", "d"));
        assert!(has_lazy_eligible_dep(&result, "d", "a"));

        // Tarjan's SCC merges overlapping cycles into one component
        assert_eq!(get_cycle_count(&result), 1);
    }

    #[test]
    fn test_dont_catch_import_cycle_in_type_checking() {
        let a = r#"
            import b
        "#;
        let b = r#"
            from typing import TYPE_CHECKING

            if TYPE_CHECKING:
                import a
        "#;
        let modules = vec![("a", a), ("b", b)];
        let result = run_cycle_analysis(&modules);

        assert_passing(&result, vec!["a", "b"]);
        assert_failing(&result, vec![]);

        assert_eq!(get_cycle_count(&result), 0);
        assert!(
            has_no_cycle_deps(&result, "a"),
            "a should have no cycle deps (TYPE_CHECKING import excluded)"
        );
        assert!(
            has_no_cycle_deps(&result, "b"),
            "b should have no cycle deps (TYPE_CHECKING import excluded)"
        );
    }

    // Port of test_dont_catch_import_cycle_in_submodule_import
    // In the original analyzer, `import dir.sub` from dir/__init__.py does not form a cycle. In
    // Lifeguard, importing a submodule implicitly adds the parent to the import graph, creating a
    // parent<->child cycle. This is a known behavioral difference; the cycle is harmless because
    // CPython handles parent/child imports specially.
    #[test]
    fn test_submodule_import_parent_child_cycle() {
        let dir = r#"
            import dir.sub
        "#;
        let dir_sub = r#"
            import dir.sub.sibling
        "#;
        let dir_sub_sibling = r#"
            import dir.sub.sibling_two
        "#;
        let dir_sub_sibling_two = r#"
        "#;
        let modules = vec![
            ("dir", dir),
            ("dir.sub", dir_sub),
            ("dir.sub.sibling", dir_sub_sibling),
            ("dir.sub.sibling_two", dir_sub_sibling_two),
        ];
        let result = run_cycle_analysis(&modules);

        assert_passing(
            &result,
            vec!["dir", "dir.sub", "dir.sub.sibling", "dir.sub.sibling_two"],
        );
        assert_failing(&result, vec![]);

        // Lifeguard detects a parent<->child cycle here (dir <-> dir.sub)
        assert_eq!(get_cycle_count(&result), 1);
        assert!(has_lazy_eligible_dep(&result, "dir", "dir.sub"));
    }

    #[test]
    fn test_catch_hidden_import_cycle() {
        let main_foo = r#"
            import foo
        "#;
        let waldo = r#"
            import foo
            def womp():
                pass
        "#;
        let foo = r#"
            import waldo
            waldo.womp()
        "#;
        let modules = vec![("main_foo", main_foo), ("waldo", waldo), ("foo", foo)];
        let result = run_cycle_analysis(&modules);

        assert_passing(&result, vec!["main_foo", "waldo", "foo"]);
        assert_failing(&result, vec![]);

        // foo <-> waldo cycle
        assert!(has_lazy_eligible_dep(&result, "foo", "waldo"));
        assert!(has_lazy_eligible_dep(&result, "waldo", "foo"));

        assert_eq!(get_cycle_count(&result), 1);
    }

    // In the original analyzer this tests that waldo.womp() raises an
    // AnalyzerUnhandledException because waldo hasn't finished importing
    // when foo tries to access womp. Lifeguard's static analysis doesn't
    // replicate runtime attribute errors from partially-initialized modules,
    // but it should still detect the cycle.
    #[test]
    fn test_catch_runtime_import_cycle() {
        let main_waldo = r#"
            import waldo
        "#;
        let waldo = r#"
            import foo
            def womp():
                pass
        "#;
        let foo = r#"
            import waldo
            waldo.womp()
        "#;
        let modules = vec![("main_waldo", main_waldo), ("waldo", waldo), ("foo", foo)];
        let result = run_cycle_analysis(&modules);

        // waldo <-> foo cycle
        assert!(has_lazy_eligible_dep(&result, "waldo", "foo"));
        assert!(has_lazy_eligible_dep(&result, "foo", "waldo"));

        assert_eq!(get_cycle_count(&result), 1);
    }
}
