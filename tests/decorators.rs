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
    fn test_decorator_arguments_on_definitions() {
        let code = r#"
def safe_factory(*args, **kwargs):
    return lambda f: f

def unsafe():
    raise()

@safe_factory(unsafe())  # E: unsafe-function-call
def function():
    ...

@safe_factory(value=unsafe())  # E: unsafe-function-call
class Class:
    ...

@safe_factory(*[unsafe()])  # E: unsafe-function-call
async def async_function():
    ...

class Container:
    @safe_factory(**{"value": unsafe()})  # E: unsafe-function-call
    def method(self):
        ...
"#;
        check(code);
    }

    #[test]
    fn test_declared_safe_decorator_still_checks_arguments() {
        let code = r#"
import pytest

def unsafe():
    raise()

@pytest.mark.parametrize(unsafe())  # E: unsafe-function-call
def test_parameterized():
    ...
"#;
        check(code);
    }

    #[test]
    fn test_property_accessor_arguments() {
        let code = r#"
def unsafe():
    raise()

class Foo:
    @property(unsafe())  # E: unsafe-function-call
    def value(self):
        return self._value

    @value.setter(unsafe())  # E: unsafe-function-call
    def value(self, value):
        self._value = value

    @value.getter(unsafe())  # E: unsafe-function-call
    def value(self):
        return self._value

    @value.deleter(unsafe())  # E: unsafe-function-call
    def value(self):
        del self._value
"#;
        check(code);
    }

    #[test]
    fn test_property_accessor_many_arguments_still_checks_safety() {
        let args = (0..64)
            .map(|i| format!("{}", i))
            .chain(std::iter::once("unsafe()".to_owned()))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            r#"
def unsafe():
    raise()

class Foo:
    @property
    def value(self):
        return self._value

    @value.setter({})  # E: unsafe-function-call
    def value(self, value):
        self._value = value

    @value.getter({})  # E: unsafe-function-call
    def value(self):
        return self._value

    @value.deleter({})  # E: unsafe-function-call
    def value(self):
        del self._value
"#,
            args, args, args
        );
        check(&code);
    }

    #[test]
    fn test_declared_safe_decorator_allows_many_arguments() {
        let args = (0..65)
            .map(|i| format!("{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            r#"
import pytest

@pytest.mark.parametrize({})
def test_parameterized():
    ...
"#,
            args
        );
        check(&code);
    }

    #[test]
    fn test_declared_safe_decorator_many_arguments_still_checks_safety() {
        let args = (0..64)
            .map(|i| format!("{}", i))
            .chain(std::iter::once("unsafe()".to_owned()))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            r#"
import pytest

def unsafe():
    raise()

@pytest.mark.parametrize({})  # E: unsafe-function-call
def test_parameterized():
    ...
"#,
            args
        );
        check(&code);
    }

    #[test]
    fn test_imported_mutating_decorator_arguments() {
        let foo = r#"
REGISTRY = {}
ARGS = (REGISTRY,)
KWARGS = {"value": REGISTRY}

def factory(value):
    value["seen"] = True
    return lambda f: f
"#;
        let __main__ = r#"
from foo import ARGS, KWARGS, REGISTRY, factory

@factory(REGISTRY)  # E: imported-var-argument
def positional():
    ...

@factory(value=REGISTRY)  # E: imported-var-argument
def keyword():
    ...

@factory(*ARGS)  # E: imported-var-argument
def star():
    ...

@factory(**KWARGS)  # E: imported-var-argument
def kwargs():
    ...
"#;
        check_all(vec![("foo", foo), ("__main__", __main__)]);
    }

    #[test]
    fn test_decorator_argument_safety_controls() {
        let code = r#"
def read_only(value):
    return lambda f: f

local = {}

@read_only(local)
def fresh_mutable():
    ...

@read_only(1)
def pure_argument():
    ...

def factory(value):
    def decorator(fn):
        raise()
    return decorator

@factory(1)  # E: unsafe-decorator-call
def returned_decorator_raises():
    ...
"#;
        check(code);
    }

    #[test]
    fn test_imported_read_only_and_local_mutating_decorator_arguments() {
        let foo = r#"
REGISTRY = {}

def read_only(value):
    return lambda f: f
"#;
        let __main__ = r#"
from foo import REGISTRY, read_only

@read_only(REGISTRY)
def imported_read_only():
    ...

def mutator(value):
    value["seen"] = True
    return lambda f: f

local = {}

@mutator(local)
def local_mutating():
    ...
"#;
        check_all(vec![("foo", foo), ("__main__", __main__)]);
    }

    #[test]
    fn test_decorator_argument_overflow() {
        let args = (0..65)
            .map(|i| format!("{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            r#"
def factory(*args):
    return lambda f: f

@factory({})  # E: too-many-args
def decorated():
    ...
"#,
            args
        );
        check(&code);
    }

    #[test]
    fn test_decorator_argument_overflow_stub_callee_allowed() {
        let args = (0..65)
            .map(|i| format!("{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            r#"
import lifeguard_test

@lifeguard_test.bar({})
def decorated():
    ...
"#,
            args
        );
        check(&code);
    }

    #[test]
    fn test_safe_and_deferred_decorator_arguments() {
        let code = r#"
def safe_factory(*args, **kwargs):
    return lambda f: f

def pure():
    return 1

def unsafe():
    raise()

@safe_factory(1)
def constant_argument():
    ...

@safe_factory(pure())
def pure_call_argument():
    ...

def outer():
    @safe_factory(unsafe())
    def nested():
        ...
"#;
        check(code);
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
