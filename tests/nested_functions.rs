/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Tests for nested function handling.
//!
//! The analyzer should distinguish between:
//! - Nested functions that are CALLED within their parent (effects propagate)
//! - Nested functions that are only DEFINED/RETURNED (effects do NOT propagate)
//!
//! Class bodies within functions are always eager: they execute when the enclosing
//! function runs, so their effects should propagate to the enclosing function.

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::*;

    // -----------------------------------------------------------------------
    // Uncalled nested functions: should NOT propagate effects
    // -----------------------------------------------------------------------

    #[test]
    fn test_nested_function_returned_not_called() {
        // f() only creates and returns g, never calls it.
        // The mutation in g does not execute when f() is called.
        let code = r#"
from foo import A

def f(x):
    def g(x):
        A[10] = 20
    return g
f()
"#;
        check(code);
    }

    #[test]
    fn test_closure_factory() {
        // make_handler creates a closure but does not call it.
        let code = r#"
from foo import obj

def make_handler():
    def handler():
        obj.x = 10
    return handler
make_handler()
"#;
        check(code);
    }

    #[test]
    fn test_decorator_factory_no_call() {
        // The decorator creates wrapper but does not call it.
        // Applying @dec only calls dec(), which returns wrapper.
        // The side effect in wrapper does not execute at import time.
        let foo = r#"
from bar import obj

def dec(f):
    def wrapper(*args):
        obj.x = 10
    return wrapper
"#;
        let main = r#"
from foo import dec

@dec
def my_func():
    pass
"#;
        check_all(vec![("foo", foo), ("__main__", main)]);
    }

    #[test]
    fn test_nested_function_stored_not_called() {
        // g is stored in a variable but never called in f.
        let code = r#"
from foo import obj

def f():
    def g():
        obj.x = 10
    h = g
    return h
f()
"#;
        check(code);
    }

    #[test]
    fn test_deeply_nested_uncalled() {
        // f defines g which defines h. None are called within f.
        let code = r#"
from foo import obj

def f():
    def g():
        def h():
            obj.x = 10
        return h
    return g
f()
"#;
        check(code);
    }

    #[test]
    fn test_multiple_nested_uncalled() {
        // f defines two nested functions, neither is called.
        let code = r#"
from foo import obj

def f():
    def g():
        obj.x = 10
    def h():
        obj.y = 20
    return (g, h)
f()
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // Called nested functions: effects SHOULD propagate via call graph
    // -----------------------------------------------------------------------

    #[test]
    fn test_nested_function_called_in_parent() {
        // g() is called within f, so its effects propagate to f.
        let code = r#"
from foo import obj

def f():
    def g():
        obj.x = 10
    g()
f()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_deeply_nested_called() {
        // f calls g, g calls h, h has the side effect.
        let code = r#"
from foo import obj

def f():
    def g():
        def h():
            obj.x = 10
        h()
    g()
f()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_nested_one_called_one_not() {
        // g is called (unsafe), h is not called (safe).
        // f() should be unsafe because of g().
        let code = r#"
from foo import obj

def f():
    def g():
        obj.x = 10
    def h():
        obj.y = 20
    g()
    return h
f()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_decorator_with_side_effect_in_body() {
        // The side effect is directly in dec's body (not in a nested function).
        let foo = r#"
from bar import obj

def dec(f):
    obj.x = 10
    return f
"#;
        let main = r#"
from foo import dec

@dec  # E: unsafe-decorator-call
def my_func():
    pass
"#;
        check_all(vec![("foo", foo), ("__main__", main)]);
    }

    #[test]
    fn test_decorator_calls_nested_function() {
        // The decorator calls setup() within its body — effects propagate.
        let foo = r#"
from bar import obj

def dec(f):
    def setup():
        obj.x = 10
    setup()
    return f
"#;
        let main = r#"
from foo import dec

@dec  # E: unsafe-decorator-call
def my_func():
    pass
"#;
        check_all(vec![("foo", foo), ("__main__", main)]);
    }

    #[test]
    fn test_parameterized_decorator_factory() {
        // Parameterized decorator: @register("name") first calls register("name")
        // which returns decorator, then calls decorator(cls). The nested function
        // decorator's body executes at import time.
        let registry = r#"
REGISTRY = {}
def register(name):
    def decorator(cls):
        REGISTRY[name] = cls
        return cls
    return decorator
"#;
        let main = r#"
from registry import register
@register("FiLMLayer")  # E: unsafe-decorator-call
class FiLMLayer:
    pass
"#;
        check_all(vec![("registry", registry), ("__main__", main)]);
    }

    #[test]
    fn test_parameterized_decorator_factory_imported_var() {
        // Same pattern but with imported variable mutation in the nested function.
        let foo = r#"
from bar import obj

def configure(key):
    def decorator(cls):
        obj[key] = cls
        return cls
    return decorator
"#;
        let main = r#"
from foo import configure
@configure("test")  # E: unsafe-decorator-call
class MyClass:
    pass
"#;
        check_all(vec![("foo", foo), ("__main__", main)]);
    }

    #[test]
    fn test_parameterized_decorator_transitive_call() {
        // The nested function in a parameterized decorator calls another
        // function that has side effects. The transitive unsafe call should
        // be detected.
        let foo = r#"
from bar import obj

def unsafe_helper():
    obj.x = 10

def register(name):
    def decorator(cls):
        unsafe_helper()
        return cls
    return decorator
"#;
        let main = r#"
from foo import register
@register("name")  # E: unsafe-decorator-call
class MyClass:
    pass
"#;
        check_all(vec![("foo", foo), ("__main__", main)]);
    }

    // -----------------------------------------------------------------------
    // Class bodies inside functions: effects SHOULD propagate (eager)
    // -----------------------------------------------------------------------

    #[test]
    fn test_class_body_in_function() {
        // Class body executes when f() is called (class definitions are eager).
        let code = r#"
from foo import obj

def f():
    class C:
        obj.x = 10
f()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_nested_class_body_in_function() {
        // Nested class bodies are also eager within the function.
        let code = r#"
from foo import obj

def f():
    class A:
        class B:
            obj.x = 10
f()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_class_body_in_nested_uncalled_function() {
        // Class C is inside g, but g is never called in f.
        // So C's body doesn't execute when f() is called.
        let code = r#"
from foo import obj

def f():
    def g():
        class C:
            obj.x = 10
    return g
f()
"#;
        check(code);
    }

    #[test]
    fn test_class_body_in_nested_called_function() {
        // Class C is inside g, and g IS called in f.
        // When f() runs, g() runs, class C is defined and its body executes.
        let code = r#"
from foo import obj

def f():
    def g():
        class C:
            obj.x = 10
    g()
f()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_method_in_class_in_function_not_called() {
        // Class C is defined in f's body (eager), but C's method m is not called.
        // The class body itself has no side effects, so f() is safe.
        let code = r#"
from foo import obj

def f():
    class C:
        def m(self):
            obj.x = 10
f()
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // Mixed patterns: functions and classes nested together
    // -----------------------------------------------------------------------

    #[test]
    fn test_function_in_class_in_function() {
        // method m is defined inside class C inside function f.
        // Class body is eager within f, but m's body is not.
        let code = r#"
from foo import obj

def f():
    class C:
        def m(self):
            obj.x = 10
        pass
f()
"#;
        check(code);
    }

    #[test]
    fn test_class_with_side_effect_and_method() {
        // The class body has a direct side effect AND a method.
        // The class-level side effect propagates, the method body does not.
        let code = r#"
from foo import obj

def f():
    class C:
        obj.x = 10
        def m(self):
            obj.y = 20
f()  # E: unsafe-function-call
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // Module-level: nested functions with no enclosing function
    // -----------------------------------------------------------------------

    #[test]
    fn test_module_level_function_with_side_effect() {
        // Top-level function that is called at module level.
        let code = r#"
from foo import obj

def f():
    obj.x = 10
f()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_module_level_function_not_called() {
        // Top-level function defined but not called — safe.
        let code = r#"
from foo import obj

def f():
    obj.x = 10
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // exec() and sys.modules in nested functions
    // -----------------------------------------------------------------------

    #[test]
    fn test_exec_in_nested_function() {
        // exec() in a nested function still triggers load_imports_eagerly
        // regardless of scope depth (checked independently).
        let code = r#"
def f(x):
    def g(x):
        exec("import foo")  # E: exec-call
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // Cross-module interactions with nested functions
    // -----------------------------------------------------------------------

    #[test]
    fn test_imported_closure_factory() {
        // Module foo defines a closure factory.
        // Module main imports and calls it.
        let foo = r#"
from bar import obj

def make_callback():
    def callback():
        obj.x = 10
    return callback
"#;
        let main = r#"
from foo import make_callback
cb = make_callback()
"#;
        check_all(vec![("foo", foo), ("__main__", main)]);
    }

    #[test]
    fn test_imported_function_that_calls_nested() {
        // Module foo defines a function that calls a nested function.
        // Module main imports and calls it.
        let foo = r#"
from bar import obj

def do_setup():
    def setup_impl():
        obj.x = 10
    setup_impl()
"#;
        let main = r#"
from foo import do_setup
do_setup()  # E: unsafe-function-call
"#;
        check_all(vec![("foo", foo), ("__main__", main)]);
    }

    // -----------------------------------------------------------------------
    // Param mutation in nested functions
    // -----------------------------------------------------------------------

    #[test]
    fn test_param_mutation_in_called_nested() {
        // Nested function mutates a parameter and is called.
        let code = r#"
from foo import obj

def outer(x):
    def inner():
        x.enabled = True
    inner()

outer(obj)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_param_mutation_in_uncalled_nested() {
        // Nested function captures and mutates a parameter from the enclosing
        // function. Even though inner() is not called within outer(), the param
        // mutation is recorded in outer's scope (where the param is defined)
        // because the closure captures the parameter and could mutate it when
        // eventually called.
        let code = r#"
from foo import obj

def outer(x):
    def inner():
        x.enabled = True
    return inner

outer(obj)  # E: imported-var-argument
"#;
        check(code);
    }
}
