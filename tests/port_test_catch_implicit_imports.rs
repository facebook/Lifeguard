/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {

    use lifeguard::test_lib::*;
    // Port over tests from safer_lazy_imports/analyzer/tests/test_catch_implicit_imports.py

    #[test]
    fn test_catch_implicit_imports() {
        let __main__ = r#"
            import foo
            import waldo

            foo.bar

            def main():
                pass

            if __name__ == "__main__":
                main()
        "#;
        let waldo = r#"
            import foo.bar
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("waldo", waldo),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_catch_implicit_import_function_in_other_file() {
        // If qux is analzyed first foo.bar is not reported as an implicit
        // import (foo.bar is unknown)
        let __main__ = r#"
            import waldo
            from qux import resolver
            resolver()

            def main():
                pass

            if __name__ == "__main__":
                main()
       "#;
        let waldo = r#"
            import foo.bar
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let qux = r#"
            import foo

            def resolver():
                foo.bar
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("waldo", waldo),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
            ("qux", qux),
        ];

        let implicit_imports = vec![("qux", vec!["foo.bar"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_implicit_import_simple() {
        let __main__ = r#"
            import foo.bar
            foo.bar.Bar
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_explicit_import_function_in_other_file() {
        let __main__ = r#"
            from qux import resolver
            resolver()

            def main():
                pass

            if __name__ == "__main__":
                main()
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let qux = r#"
            import foo.bar
            def resolver():
                foo.bar.Bar
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
            ("qux", qux),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_catch_implicit_import_uncalled_function() {
        let __main__ = r#"
           import waldo
           import foo

           def womp_womp():
               x = foo.bar.Bar

           def main():
               pass

           if __name__ == "__main__":
               main()
        "#;
        let waldo = r#"
            import foo.bar
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("waldo", waldo),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_explicit_inner_import_inside_function() {
        let __main__ = r#"
            import foo

            def womp_womp():
                import foo.bar
                x = foo.bar.Bar

            def main():
                pass

            if __name__ == "__main__":
                main()
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_nested_explicit_inner_import_from_inside_function() {
        let __main__ = r#"
           import foo

           def womp():
               from foo import bar
               def womp_womp():
                   x = foo.bar.Bar

           def main():
               pass

           if __name__ == "__main__":
               main()
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_catch_implicit_import_function_in_other_unsafe_file() {
        // If qux is analzyed first foo.bar is not reported as an implicit
        // import (foo.bar is unknown)
        let __main__ = r#"
            from qux import resolver

            def func_one():
                import foo.bar

            func_one()
            resolver()

            def main():
                pass

            if __name__ == "__main__":
                main()
        "#;
        let foo_bar = r#"
            input() # unsafe! # E: prohibited-call 
        "#;
        let foo_init = r#"
        "#;
        let qux = r#"
            import foo

            def resolver():
                foo.bar
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
            ("qux", qux),
        ];

        let implicit_imports = vec![("qux", vec!["foo.bar"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_catch_implicit_imports_correct_position_parent_import() {
        let __main__ = r#"
            import unsafeOne
            import foo
            import unsafeTwo
            import waldo
            import unsafeThree

            foo.bar

            def main():
                pass

            if __name__ == "__main__":
                main()
        "#;
        let waldo = r#"
            import foo.bar
        "#;
        let foo_bar = r#"
        "#;
        let foo_init = r#"
        "#;
        let unsafe_one = r#"
            input() # unsafe! # E: prohibited-call 
        "#;
        let unsafe_two = r#"
            input() # unsafe! # E: prohibited-call 
        "#;
        let unsafe_three = r#"
            input() # unsafe! # E: prohibited-call 
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("waldo", waldo),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
            ("unsafeOne", unsafe_one),
            ("unsafeTwo", unsafe_two),
            ("unsafeThree", unsafe_three),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_catch_implicit_imports_correct_preload_list_position_sibling_import() {
        let __main__ = r#"
            import unsafeOne
            import foo.baz
            import unsafeTwo
            import waldo
            import unsafeThree

            foo.bar

            def main():
                pass

            if __name__ == "__main__":
                main()
       "#;
        let waldo = r#"
            import foo.bar
        "#;
        let foo_bar = r#"
        "#;
        let foo_baz = r#"
        "#;
        let foo_init = r#"
        "#;
        let unsafe_one = r#"
            input() # unsafe! # E: prohibited-call 
        "#;
        let unsafe_two = r#"
            input() # unsafe! # E: prohibited-call 
        "#;
        let unsafe_three = r#"
            input() # unsafe! # E: prohibited-call 
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("waldo", waldo),
            ("foo.bar", foo_bar),
            ("foo.baz", foo_baz),
            ("foo.__init__", foo_init),
            ("unsafeOne", unsafe_one),
            ("unsafeTwo", unsafe_two),
            ("unsafeThree", unsafe_three),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_dont_catch_explicitly_added_import() {
        // We shouldn’t report this implicit import because the attribute path
        // in module os is explicitly assigned to be posixpath (import posixpath as path)
        let __main__ = r#"
            import os
            os.path
        "#;
        let modules = vec![("__main__", __main__)];

        // TODO: We think os.path is an implicit import because of how we have set up os/path.pyi
        let implicit_imports = vec![("__main__", vec!["os.path"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_loaded_inside_called_function_in_other_module() {
        // We shouldn’t report this implicit import because of the import
        // foo.bar.baz in foo_bar(), which is called.
        let __main__ = r#"
            import foo.bar
            foo.bar.baz
       "#;
        let foo_bar = r#"
            def foo_bar():
                import foo.bar.baz
            foo_bar()
        "#;
        let foo_bar_baz = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_error_import_loaded_inside_uncalled_function() {
        // We should report an implicit import because the import of foo.bar.baz hasn’t happened.
        let __main__ = r#"
            import foo.bar
            import waldo
            foo.bar.baz
       "#;
        let foo_bar = r#"
            def foo_bar():
                import foo.bar.baz
        "#;
        let foo_bar_baz = r#"
        "#;
        let waldo = r#"
            import foo.bar.baz
        "#;

        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
            ("waldo", waldo),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar.baz"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_loaded_inside_called_function() {
        // We shouldn’t report this implicit import because foo_bar() is called in main, triggering the import.
        let __main__ = r#"
            import foo.bar
            foo.bar.foo_bar()
            foo.bar.baz
       "#;
        let foo_bar = r#"
            def foo_bar():
                import foo.bar.baz
        "#;
        let foo_bar_baz = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_error_import_in_parent_module_stays_lazy() {
        // We should report this implicit import because import foo.bar.baz remains lazy.
        let __main__ = r#"
            import foo.bar
            foo.bar.baz
       "#;
        let foo_bar = r#"
            import foo.bar.baz
        "#;
        let foo_bar_baz = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar.baz"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_catch_implicit_import_in_circular_reference() {
        // Module A imports B and uses B.C (implicit import)
        // Module B imports A, creating a circular reference
        let module_a = r#"
            import module_b

            # This creates an implicit import of module_c through module_b
            module_b.c
        "#;

        // Module B imports A (circular reference) and C
        let module_b = r#"
            import module_a
            import module_c

            c = module_c  # Make c an attribute of module_b
        "#;

        // Module C is the one being implicitly imported
        let module_c = r#"
            value = "I'm module C"
        "#;

        let modules = vec![
            ("module_a", module_a),
            ("module_b", module_b),
            ("module_c", module_c),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_is_attribute_of_imported_parent_module() {
        // We shouldn’t report this implicit import because baz is an actual explicit attribute of foo.bar.
        let __main__ = r#"
            import foo.bar
            foo.bar.baz
       "#;
        let foo_bar = r#"
            import foo.bar.baz as baz
        "#;
        let foo_bar_baz = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_was_attribute_of_imported_parent_module_now_none() {
        // We shouldn't report any error because baz still exists but is just set to None.
        let __main__ = r#"
            import foo.bar
            foo.bar.baz
       "#;
        let foo_bar = r#"
            import foo.bar.baz as baz
            baz = None
        "#;
        let foo_bar_baz = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_error_import_was_attribute_of_imported_parent_module_now_deleted() {
        // We shouldn't report any error implicit import error (just an attribute error) because baz no longer exists in foo.bar
        let __main__ = r#"
            import foo.bar
            foo.bar.baz # MISSING E: attribute-error
       "#;
        let foo_bar = r#"
            import foo.bar.baz as baz
            del baz
        "#;
        let foo_bar_baz = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_is_imported_in_parent_module_as_alias_and_loaded_in_main_one() {
        // We shouldn’t report this implicit import because we are triggering the import of foo.bar.baz while accessing foo_bar_baz
        let __main__ = r#"
            import foo.bar
            foo.bar.foo_bar_baz
            foo.bar.baz
       "#;
        let foo_bar = r#"
            import foo.bar.baz as foo_bar_baz
        "#;
        let foo_bar_baz = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_is_imported_in_parent_module_as_alias_and_loaded_in_main_two() {
        let __main__ = r#"
            import foo.bar
            foo.bar.foo_bar_baz
            foo.bar.baz
       "#;
        let foo_bar = r#"
            import foo.bar.baz as foo_bar_baz
        "#;
        let modules = vec![("__main__", __main__), ("foo.bar", foo_bar)];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_error_import_is_nested_and_uncalled() {
        // We should report this implicit import because import foo.bar.baz remains lazy.
        let __main__ = r#"
            import foo
            foo.bar.baz
       "#;
        let foo = r#"
            import waldo
            waldo.something()
        "#;
        let waldo = r#"
            import foo.bar.baz
            def something():
                pass
        "#;
        let foo_bar_baz = r#"
        "#;

        let modules = vec![
            ("__main__", __main__),
            ("foo", foo),
            ("waldo", waldo),
            ("foo.bar.baz", foo_bar_baz),
        ];
        let implicit_imports = vec![("__main__", vec!["foo.bar.baz"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_is_nested_and_called() {
        // We shouldn’t report this implicit import because the import foo.bar.baz is triggered by the call to something().
        let __main__ = r#"
            import foo
            foo.bar.baz
       "#;
        let foo = r#"
            import waldo
            waldo.something()
        "#;
        let waldo = r#"
            import foo.bar.baz
            def something():
                foo.bar.baz
        "#;
        let foo_bar_baz = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo/__init__", foo),
            ("waldo", waldo),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_child_explicitly() {
        // We shouldn't report this implicit import because foo.bar is imported when using the bar attribute.
        let __main__ = r#"
            import foo
            foo.bar
       "#;
        let foo = r#"
            import foo.bar as bar
        "#;
        let foo_bar = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo/__init__", foo),
            ("foo.bar", foo_bar),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_no_error_import_in_try_block() {
        // We shouldn't report this implicit import because import foo.bar is eagerly loaded when using waldo
        let __main__ = r#"
            import foo
            import waldo
            waldo

            foo.bar

            def main():
                pass

            if __name__ == "__main__":
                main()
       "#;
        let waldo = r#"
            try:
                import foo.bar
            except:
                print("womp womp")
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("waldo", waldo),
            ("foo/__init__", foo_init),
            ("foo.bar", foo_bar),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_import_module_no_implicit_import() {
        let __main__ = r#"
            from importlib import import_module
            A = import_module("foo.bar")
            A.Bar
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_catch_import_module_implicit_import() {
        let __main__ = r#"
           import importlib
           importlib.import_module("waldo")
           import foo

           def womp_womp():
               x = foo.bar.Bar

           def main():
               pass

           if __name__ == "__main__":
               main()
        "#;
        let waldo = r#"
            import foo.bar
        "#;
        let foo_bar = r#"
            Bar = "Bar"
        "#;
        let foo_init = r#"
        "#;
        let modules = vec![
            ("__main__", __main__),
            ("waldo", waldo),
            ("foo.bar", foo_bar),
            ("foo.__init__", foo_init),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_conditional_import_unknown() {
        let __main__ = r#"
        import os 
        if os.getpid() == 0:
            import foo.bar

        foo.bar.baz
        "#;
        let foo_bar = r#"
        import foo.bar.baz
        "#;
        let foo_bar_baz = r#"
        "#;

        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar.baz"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_conditional_import_true() {
        let __main__ = r#"
        if True:
            import foo.bar

        foo.bar.baz
        "#;
        let foo_bar = r#"
        import foo.bar.baz
        "#;
        let foo_bar_baz = r#"
        "#;

        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = vec![("__main__", vec!["foo.bar.baz"])];
        check_errors_and_implicit_imports(modules, implicit_imports);
    }

    #[test]
    fn test_conditional_import_false() {
        let __main__ = r#"
        if False:
            import foo.bar

        foo.bar.baz # E: unknown-object
        "#;
        let foo_bar = r#"
        import foo.bar.baz
        "#;
        let foo_bar_baz = r#"
        "#;

        let modules = vec![
            ("__main__", __main__),
            ("foo.bar", foo_bar),
            ("foo.bar.baz", foo_bar_baz),
        ];

        let implicit_imports = Vec::new();
        check_errors_and_implicit_imports(modules, implicit_imports);
    }
}
