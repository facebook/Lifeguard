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

    /// The common real-world shape: use the module only if it is already loaded.
    #[test]
    fn test_membership_test_makes_the_read_a_probe() {
        let reader = r#"
            import sys

            def numpy_array(values):
                if "loud" in sys.modules:
                    loud = sys.modules["loud"]
                    return loud.array(values)
                return None
        "#;
        let modules = vec![("reader", reader), ("loud", "")];
        check_all(modules.clone());

        let result = run_lifeguard_analysis(&modules);
        assert_passing(&result, vec!["reader", "loud"]);
        assert!(
            !loads_imports_eagerly(&result, "reader"),
            "the code checks first, so the read cannot fail"
        );
    }

    /// A probed key is recognised anywhere in the module, not just next to the read.
    #[test]
    fn test_guard_elsewhere_in_the_module_makes_the_read_a_probe() {
        let reader = r#"
            import sys

            def get():
                if "loud" not in sys.modules:
                    return None
                loud = sys.modules["loud"]
                return loud
        "#;
        let result = run_lifeguard_analysis(&vec![("reader", reader), ("loud", "")]);
        assert!(
            !loads_imports_eagerly(&result, "reader"),
            "the code checks first, so the read cannot fail"
        );
    }

    /// The check need not be the whole condition, so the scan recurses.
    #[test]
    fn test_nested_membership_test_makes_the_read_a_probe() {
        let reader = r#"
            import sys

            def get():
                if bool("loud" in sys.modules):
                    loud = sys.modules["loud"]
                    return loud
                return None
        "#;
        let result = run_lifeguard_analysis(&vec![("reader", reader), ("loud", "")]);
        assert!(
            !loads_imports_eagerly(&result, "reader"),
            "the code checks first, so the read cannot fail"
        );
    }

    /// In `"x" in y in sys.modules` the key was tested against `y`.
    #[test]
    fn test_chained_membership_test_is_not_a_probe() {
        let reader = r#"
            import sys

            def get(y):
                if "loud" in y in sys.modules:
                    return None
                return sys.modules["loud"]
        "#;
        let result = run_lifeguard_analysis(&vec![("reader", reader), ("loud", "")]);
        assert!(
            loads_imports_eagerly(&result, "reader"),
            "the key was tested against `y`, not `sys.modules`"
        );
    }

    /// `<other>.modules` is not `sys.modules`, so testing it says nothing about
    /// what the code copes with.
    #[test]
    fn test_membership_test_on_another_object_is_not_a_probe() {
        let reader = r#"
            import sys

            def get(bar):
                if "loud" in bar.modules:
                    return None
                return sys.modules["loud"]
        "#;
        let result = run_lifeguard_analysis(&vec![("reader", reader), ("loud", "")]);
        assert!(
            loads_imports_eagerly(&result, "reader"),
            "the key was tested against `bar`, not `sys.modules`"
        );
    }

    #[test]
    fn test_caught_key_error_makes_the_read_a_probe() {
        let reader = r#"
            import sys

            def get():
                try:
                    loud = sys.modules["loud"]
                except KeyError:
                    loud = None
                return loud
        "#;
        let result = run_lifeguard_analysis(&vec![("reader", reader), ("loud", "")]);
        assert!(
            !loads_imports_eagerly(&result, "reader"),
            "the code checks first, so the read cannot fail"
        );
    }

    /// Catching `Exception` says the code survives the error, not that it expected
    /// it. Eliding here would silently swap which branch runs.
    #[test]
    fn test_catch_all_except_stays_eager() {
        let reader = r#"
            import sys

            def get():
                try:
                    setup()
                    loud = sys.modules["loud"]
                except Exception:
                    loud = None
                return loud
        "#;
        let result = run_lifeguard_analysis(&vec![("reader", reader), ("loud", "")]);
        assert!(
            loads_imports_eagerly(&result, "reader"),
            "a catch-all handler is not evidence the read was expected to fail"
        );
    }

    /// A `try` that cannot catch the `KeyError` leaves the read conservative.
    #[test]
    fn test_unrelated_except_stays_eager() {
        let reader = r#"
            import sys

            def get():
                try:
                    loud = sys.modules["loud"]
                except ValueError:
                    loud = None
                return loud
        "#;
        let result = run_lifeguard_analysis(&vec![("reader", reader), ("loud", "")]);
        assert!(
            loads_imports_eagerly(&result, "reader"),
            "this `try` cannot catch the `KeyError`, so the read can still fail"
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
