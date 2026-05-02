/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::module_parser::parse_source;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::test_lib::check_imports;
    use lifeguard::test_lib::run_module_analysis;
    use lifeguard::test_lib::*;

    #[test]
    fn test_local_decorator() {
        let code = r#"
def dec(f):
    return f

@dec
def f(x):
    ...
"#;
        check(code);
    }

    #[test]
    fn test_local_decorator_effects() {
        let code = r#"
def dec(f):
    return f

@dec # E: decorator-call
def f(x):
    ...
"#;
        check_effects(code);
    }

    #[test]
    fn test_imported_function_decorator() {
        let code = r#"
from foo import dec

@dec  # E: unknown-decorator-call
def f(x):
    ...
"#;
        check(code);
    }

    #[test]
    fn test_imported_function_decorator_effects() {
        let code = r#"
from foo import dec

@dec  # E: imported-decorator-call
def f(x):
    ...
"#;
        check_effects(code);
    }

    #[test]
    fn test_imported_class_decorator() {
        let code = r#"
from foo import dec

@dec  # E: unknown-decorator-call
class A:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_imported_method_decorator() {
        let code = r#"
from foo import dec

class A:
    @dec  # E: unknown-decorator-call
    def f(x):
        ...
"#;
        check(code);
    }

    #[test]
    fn test_imported_method_decorator_effects() {
        let code = r#"
from foo import dec

class A:
    @dec  # E: imported-decorator-call
    def f(x):
        ...
"#;
        check_effects(code);
    }

    #[test]
    fn test_safe_imported_decorator() {
        let foo = r#"
def dec(f):
    return f
"#;
        let __main__ = r#"
from foo import dec

class A:
    @dec
    def f(x):
        ...
"#;
        check_all(vec![("foo", foo), ("__main__", __main__)])
    }

    #[test]
    fn test_unsafe_imported_decorator() {
        let foo = r#"
def dec(f):
    raise()
"#;
        let __main__ = r#"
from foo import dec
import foo

class A:
    @dec  # E: unsafe-decorator-call
    def f(x):
        ...

@dec(args)  # E: unsafe-decorator-call
def f(x):
    ...

@foo.dec  # E: unsafe-decorator-call
def g(x):
    ...
"#;
        check_all(vec![("foo", foo), ("__main__", __main__)])
    }

    #[test]
    fn test_unknown_decorator() {
        let code = r#"
    @dec # E: unknown-decorator-call
    def f():
        ...
        "#;
        check(code)
    }

    #[test]
    fn test_property() {
        let code = r#"
class A:
    @property
    def x(self):
        return self.x
"#;
        check(code)
    }

    #[test]
    fn test_subscript_decorator() {
        let code = r#"
decorators = [
    lambda fn: fn,
    lambda fn: fn,
]

@decorators[0]  # E: unknown-decorator-call
def foo(value):
    print(value)

foo
foo(37)
"#;
        check(code)
    }

    #[test]
    fn test_subscript_decorator_with_call() {
        let code = r#"
decorators = [lambda f: f]

@decorators[0]()  # E: unknown-decorator-call
def f(x):
    pass
"#;
        check(code)
    }

    #[test]
    fn test_decorator_adds_to_called_functions() {
        let code = r#"
def dec(f):
    import bar
    return f

@dec
def g():
    pass
"#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let out = run_module_analysis(code, &parsed_module);
        check_imports(
            out,
            vec![("test.dec", vec!["bar"])],
            vec![("test.dec", vec!["bar"])],
        );
    }

    #[test]
    fn test_property_setter_is_safe() {
        let code = r#"
class Foo:
    @property
    def bar(self):
        return self._bar

    @bar.setter
    def bar(self, value):
        self._bar = value
"#;
        check(code);
    }

    #[test]
    fn test_property_deleter_is_safe() {
        let code = r#"
class Foo:
    @property
    def bar(self):
        return self._bar

    @bar.deleter
    def bar(self):
        del self._bar
"#;
        check(code);
    }
}
