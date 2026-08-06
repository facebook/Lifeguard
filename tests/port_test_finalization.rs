/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::*;
    // Port over tests from safer_lazy_imports/analyzer/tests/test_finalization.py
    // Kept as a conformance corpus against the original analyzer, so overlap
    // with the topical test files is deliberate.

    #[test]
    fn test_dunder_del_at_module_level() {
        let __main__ = r#"
            from foo import few
            import bar

            class A:
                def __init__(self):
                    self.is_alive = True

                def __del__(self):  # E: custom-finalizer
                    self.is_alive = False
        "#;
        let foo = r#"
            few = "a small number"
        "#;
        let bar = r#"
        "#;
        check_all(vec![("__main__", __main__), ("foo", foo), ("bar", bar)])
    }

    #[test]
    fn test_dunder_del_at_function_level() {
        let __main__ = r#"
            from foo import few
            import bar

            def call_this_func():
                class A:
                    def __init__(self):
                        self.is_alive = True

                    def __del__(self):  # E: custom-finalizer
                        self.is_alive = False
                return A()
        "#;
        let foo = r#"
            few = "a small number"
        "#;
        let bar = r#"
        "#;
        check_all(vec![("__main__", __main__), ("foo", foo), ("bar", bar)])
    }

    #[test]
    fn test_dunder_del_inner_class() {
        let __main__ = r#"
            from foo import few
            import bar

            class A:
                def __init__(self):
                    class B:
                        def __del__(self):  # E: custom-finalizer
                            self.is_alive = False
        "#;
        let foo = r#"
            few = "a small number"
        "#;
        let bar = r#"
        "#;
        check_all(vec![("__main__", __main__), ("foo", foo), ("bar", bar)])
    }

    #[test]
    fn test_dunder_del_inner_class_in_func() {
        let __main__ = r#"
            from foo import few
            import bar

            def foo():
                class A:
                    def __init__(self):
                        class B:
                            def __del__(self):  # E: custom-finalizer
                                self.is_alive = False
        "#;
        let foo = r#"
            few = "a small number"
        "#;
        let bar = r#"
        "#;
        check_all(vec![("__main__", __main__), ("foo", foo), ("bar", bar)])
    }
}
