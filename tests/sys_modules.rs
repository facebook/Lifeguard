/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::output::LifeGuardAnalysis;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::test_lib::assert_passing;
    use lifeguard::test_lib::check;
    use lifeguard::test_lib::check_all;
    use lifeguard::test_lib::run_lifeguard_analysis;

    fn loads_imports_eagerly(result: &LifeGuardAnalysis, module: &str) -> bool {
        result
            .output
            .load_imports_eagerly
            .contains(&ModuleName::from_str(module))
    }

    #[test]
    fn test_reading_a_module_the_interpreter_always_has() {
        let code = r#"
            import sys
            main_file = sys.modules["__main__"].__file__
        "#;
        check(code);

        let result = run_lifeguard_analysis(&vec![("test", code)]);
        assert!(
            !loads_imports_eagerly(&result, "test"),
            "`__main__` is always loaded, so the read cannot fail"
        );
    }

    #[test]
    fn test_reading_itself_or_an_ancestor() {
        let pkg_child = r#"
            import sys
            path = sys.modules["pkg"].__path__
        "#;
        let standalone = r#"
            import sys
            me = sys.modules["standalone"]
        "#;
        let modules = vec![
            ("pkg", ""),
            ("pkg.child", pkg_child),
            ("standalone", standalone),
        ];
        check_all(modules.clone());

        let result = run_lifeguard_analysis(&modules);
        assert_passing(&result, vec!["pkg", "pkg.child", "standalone"]);
        assert!(
            result.output.load_imports_eagerly.is_empty(),
            "a module reading itself or an ancestor cannot fail"
        );
    }

    /// A read of any other module can find nothing, so the reader keeps the
    /// conservative treatment.
    #[test]
    fn test_reading_another_module_stays_eager() {
        let code = r#"
            import sys
            x = sys.modules["loud"] # E: sys-modules-access
        "#;
        let modules = vec![("test", code), ("loud", "")];
        check_all(modules.clone());

        let result = run_lifeguard_analysis(&modules);
        assert!(
            loads_imports_eagerly(&result, "test"),
            "`loud` may not be loaded, so the read can still fail"
        );
    }

    #[test]
    fn test_computed_key_stays_eager() {
        let code = r#"
            import sys
            name = "sys"
            x = sys.modules[name] # E: sys-modules-access
        "#;
        check(code);

        let result = run_lifeguard_analysis(&vec![("test", code)]);
        assert!(
            loads_imports_eagerly(&result, "test"),
            "a computed key names no module, so nothing can be ruled out"
        );
    }

    /// A write registers a module under a key rather than looking one up, so it
    /// stays conservative even though the key is one that is always loaded.
    #[test]
    fn test_literal_key_write_stays_eager() {
        let code = r#"
            import sys
            import loud
            sys.modules["sys"] = loud # E: sys-modules-access
        "#;
        let modules = vec![("test", code), ("loud", "")];
        check_all(modules.clone());

        let result = run_lifeguard_analysis(&modules);
        assert!(
            loads_imports_eagerly(&result, "test"),
            "a write registers a module rather than looking one up"
        );
    }

    #[test]
    fn test_method_call_stays_eager() {
        let code = r#"
            import sys
            sys.modules.setdefault("sys", None) # E: sys-modules-access
        "#;
        check(code);

        let result = run_lifeguard_analysis(&vec![("test", code)]);
        assert!(
            loads_imports_eagerly(&result, "test"),
            "a method call is opaque, so nothing can be ruled out"
        );
    }

    #[test]
    fn test_del_call_still_triggers_eager() {
        let code = r#"
            import sys
            del sys.modules["builtins"] # E: sys-modules-access
        "#;
        check(code);

        let result = run_lifeguard_analysis(&vec![("test", code)]);
        assert!(
            loads_imports_eagerly(&result, "test"),
            "accessing 'safe' modules via del is still unsafe"
        );
    }
}
