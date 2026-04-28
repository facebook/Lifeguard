/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::*;

    #[test]
    fn test_update_to_import() {
        let code = r#"
from foo import bar
bar = 1
"#;
        check(code);
    }

    #[test]
    fn test_update_to_import_array() {
        let code = r#"
from foo import bar
bar[0] = 1  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_update_to_import_array_effects() {
        let code = r#"
from foo import bar
bar[0] = 1  # E: imported-var-mutation
"#;
        check_effects(code);
    }

    #[test]
    fn test_getattr_literal() {
        let code = r#"
def f(): ...
getattr(f, "__module__")
"#;
        check(code);
    }

    #[test]
    fn test_getattr_nonliteral() {
        let code = r#"
def f(): ...
x = "foo"
getattr(f, x)  # E: prohibited-call
"#;
        check(code);
    }

    #[test]
    fn test_getattr_in_assignment() {
        let code = r#"
def f(): ...
x = "foo"
a = getattr(f, x)  # E: prohibited-call
"#;
        check(code);
    }

    #[test]
    fn test_setattr_literal() {
        let code = r#"
def f(): ...
setattr(f, "__module__", "foo")
"#;
        check(code);
    }

    #[test]
    fn test_setattr_nonliteral() {
        let code = r#"
def f(): ...
x = "foo"
setattr(f, x, 1)  # E: prohibited-call
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_subexpression() {
        let code = r#"
def f(x, y):
    raise(ValueError())
def g(x, y): return x + y
a = g(1, 2) + g(4, f(4, 5))  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_setattr_global_assign() {
        let code = r#"
def f(): ...
def g():
    setattr(f, "__module__", "foo")  # E: global-var-mutation
"#;
        check_effects(code);
    }

    #[test]
    fn test_unsafe_subexpression_effects() {
        let code = r#"
from foo import f
def g(x, y): return x + y
a = g(1, 2) + g(4, f(4, 5))  # E: imported-function-call  # E: function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_nested_class() {
        let code = r#"
class A:
    class B:
        def __del__(self): ...  # E: custom-finalizer
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_classvar_initializer() {
        let code = r#"
from foo import f
class A:
    x = getattr(a, b)  # E: prohibited-call
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_classvar_initializer_effects() {
        let code = r#"
from foo import f
class A:
    x = f()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_safe_classvar_initializer() {
        let code = r#"
def f(): ...
class A:
    x = f()
"#;
        check(code);
    }

    #[test]
    fn test_imported_subscript_access() {
        let code = r#"
from foo import bar
bar['hello']
"#;
        check(code);
    }

    #[test]
    fn test_imported_subscript_assignment() {
        let code = r#"
from foo import bar
bar['hello'] = 10  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_binop_side_effect_left() {
        let code = r#"
from foo import bar
if bar != baz: # E: unknown-value-binary-op
    pass
"#;
        check_effects(code);
    }

    #[test]
    fn test_binop_side_effect_right() {
        let code = r#"
from foo import bar
if baz != bar: # E: unknown-value-binary-op
    pass
"#;
        check_effects(code);
    }

    #[test]
    fn test_global_var_assign() {
        let code = r#"
a = 1
def f():
    global a
    a = 2  # E: global-var-assign
"#;
        check_effects(code);
    }
}
