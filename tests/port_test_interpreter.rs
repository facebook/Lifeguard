/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::*;
    // Port over some tests from safer_lazy_imports/analyzer/tests/test_interpreter.py
    // Kept as a conformance corpus against the original analyzer, so overlap
    // with the topical test files is deliberate.

    #[test]
    fn test_sys_path_modifications_module_scope() {
        let code = r#"
            import sys
            sys.path.append("test") # E: unknown-function-call
            sys.path.extend(["bar", "baz"]) # E: unknown-function-call
        "#;
        check(code);
    }

    #[test]
    fn test_sys_path_hooks_modifications_module_scope() {
        let code = r#"
            import sys
            sys.path_hooks.insert(0, "foo") # E: unknown-function-call
        "#;
        check(code);
    }

    #[test]
    fn test_sys_metapath_modifications_module_scope() {
        let code = r#"
            import sys
            sys.meta_path.append("eek") # E: unknown-function-call
            sys.meta_path.remove("eek") # E: unknown-function-call
        "#;
        check(code);
    }

    #[test]
    fn test_sys_modules_modifications_module_scope() {
        let code = r#"
            import sys
            sys.modules.setdefault("bop", "pop") # E: sys-modules-access
        "#;
        check(code);
    }

    #[test]
    fn test_sys_modules_assignment_module_scope() {
        let code = r#"
            import sys
            x = sys.modules
            x["bop"] = "pop" # E: imported-module-assignment
        "#;
        check(code);
    }

    #[test]
    fn test_sys_modules_subscript_read() {
        let code = r#"
            import sys
            x = sys.modules["some.module"] # E: sys-modules-access
        "#;
        check(code);
    }

    #[test]
    fn test_sys_modules_subscript_write() {
        let code = r#"
            import sys
            sys.modules["bop"] = "pop" # E: sys-modules-access
        "#;
        check(code);
    }

    #[test]
    fn test_sys_modules_method_call() {
        let code = r#"
            import sys
            sys.modules.pop("x") # E: sys-modules-access
        "#;
        check(code);
    }

    #[test]
    fn test_eval_one_scope() {
        let code = r#"
            d = {'x': 2}
            y = eval('x', d)
        "#;
        check(code);
    }

    #[test]
    fn test_eval_two_scopes() {
        let code = r#"
            d1 = {'x': 0}
            d2 = {'x': 3}
            y = eval('x', d1, d2)
        "#;
        check(code);
    }

    #[test]
    fn test_eval_default_globals_and_locals() {
        let code = r#"
            w = eval('42')
        "#;
        check(code);
    }

    #[test]
    fn test_eval_import_raises_exception() {
        let code = r#"
            code_str = 'import x'
            eval(code_str, {}) # E: exec-call
        "#;
        check(code);
    }

    #[test]
    fn test_eval_assignment_raises_syntax_error() {
        let code = r#"
            eval('x = 1', {}) # E: exec-call
        "#;
        check(code);
    }

    #[test]
    fn test_eval_multiple_expressions_raises_syntax_error() {
        let code = r#"
            eval('1+1; 2+2', {})
        "#;
        check(code);
    }
}
