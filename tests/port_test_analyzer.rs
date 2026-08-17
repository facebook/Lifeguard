/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::*;
    // Port over tests from safer_lazy_imports/analyzer/tests/test_analyzer.py
    // Omitted *imported_modules* and *import_attribute* tests
    // Kept as a conformance corpus against the original analyzer, so overlap
    // with the topical test files is deliberate.

    #[test]
    fn test_analyzer() {
        // The call to input should fail as it produces a side-effect
        let __main__ = r#"
            import a
        "#;
        let a = r#"
            import b
            import d
        "#;
        let b = r#"
            import c
            import e
        "#;
        let c = r#"
        "#;
        let d = r#"
            import e
            import f
        "#;
        let e = r#"
            input()  # E: prohibited-call
        "#;
        let f = r#"
        "#;
        check_all(vec![
            ("__main__", __main__),
            ("a", a),
            ("b", b),
            ("c", c),
            ("d", d),
            ("e", e),
            ("f", f),
        ])
    }

    #[test]
    fn test_safe_mutable_logging() {
        // TODO(T228941537): Conformance Opportunity: This should all be safe
        // Logging should pass as it is a "safe" side-effect
        let __main__ = r#"
            import logging
            import foo

            foo.logger.isEnabledFor(logging.DEBUG) # E: unknown-function-call
            foo.logger.setLevel(logging.DEBUG) # E: unknown-function-call

            handler = logging.NullHandler()
            formatter = logging.Formatter("%(asctime)s - %(message)s")
            handler.setFormatter(formatter)
            foo.logger.addHandler(handler) # E: unknown-function-call
       "#;
        let foo = r#"
            import logging
            logger = logging.getLogger(__name__)
        "#;
        check_all(vec![("__main__", __main__), ("foo", foo)])
    }

    #[test]
    fn test_os_chdir_call_at_module_scope() {
        // os.chdir is an unsafe side-effect
        let __main__ = r#"
            import os
            os.chdir("/home/") # E: unsafe-function-call
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_os_fchdir_call_at_module_scope() {
        // os.chdir is an unsafe side-effect
        let __main__ = r#"
            import os
            os.fchdir("/home/") # E: unsafe-function-call
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_os_chroot_call_at_module_scope() {
        // os.chdir is an unsafe side-effect
        let __main__ = r#"
            import os
            os.chroot("/home/") # E: unsafe-function-call
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_pending_call() {
        let __main__ = r#"
            def pending(p1, p2="!p2!", /, p3_or_kw="!p3_or_kw!", *, kw1, kw2="!kw2!", **kwargs):
                p1, kw1, kwargs
                if p2 != "!p2!": raise ValueError(p2)
                if p3_or_kw != "!p3_or_kw!": raise ValueError(p3_or_kw)
                if kw2 != "!kw2!": raise ValueError(kw2)
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_exec_import() {
        // We should catch the exec call as an addition to the load_imports_eagerly set
        // and for now a default addition to the lazy_eligible dict as well
        let __main__ = r#"
            import imp
            import imp_frm
        "#;
        let foo = r#"
            few = "a small number"
        "#;
        let bar = r#"
        "#;
        let imp = r#"
            exec("import bar") # E: exec-call
        "#;
        let imp_frm = r#"
            exec("from foo import few") # E: exec-call
        "#;
        check_all(vec![
            ("__main__", __main__),
            ("foo", foo),
            ("bar", bar),
            ("imp", imp),
            ("imp_frm", imp_frm),
        ])
    }

    #[test]
    fn test_exec_fn_import() {
        // We should catch the exec call as an addition to the load_imports_eagerly set
        // and for now a default addition to the lazy_eligible dict as well
        let __main__ = r#"
            import imp
        "#;
        let foo = r#"
        "#;
        let imp = r#"
            def fn(module):
                exec("import foo") # E: exec-call

        "#;
        check_all(vec![("__main__", __main__), ("foo", foo), ("imp", imp)])
    }

    #[test]
    fn test_exec_fn_import_unknown() {
        // We should catch the exec call as an addition to the load_imports_eagerly set
        // and for now a default addition to the lazy_eligible dict as well
        let __main__ = r#"
            import imp
        "#;
        let imp = r#"
            def fn(module):
                exec("import %s" % module) # E: exec-call

        "#;
        check_all(vec![("__main__", __main__), ("imp", imp)])
    }

    #[test]
    fn test_pending_function_import() {
        let __main__ = r#"
            def fn(module):
                return __import__(module)
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_pending_function_import_module() {
        let __main__ = r#"
            def fn(module):
                return __import_module__(module)
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_pending_function_exec() {
        let __main__ = r#"
            def fn(code):
                return exec(code)
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_fn_def_type_params() {
        let __main__ = r#"
            def fn[T](arg: T):
                print(arg)
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_async_fn_def_type_params() {
        let __main__ = r#"
           async def fn[T](arg: T):
                print(arg)
       "#;
        check_all(vec![("__main__", __main__)])
    }

    #[test]
    fn test_class_def_type_params() {
        let __main__ = r#"
            class C:
                def __init__(self, arg):
                    print(arg)
       "#;
        check_all(vec![("__main__", __main__)])
    }
}
