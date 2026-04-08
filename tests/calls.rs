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
    fn test_top_level_function_call() {
        let code = r#"
getattr(x, y)  # E: prohibited-call
"#;
        check(code);
    }

    #[test]
    fn test_top_level_function_call_effects() {
        let code = r#"
import os
os.path.join("a", "b")  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_top_level_function_call_in_assignment_effects() {
        let code = r#"
import os
a = os.path.join("a", "b")  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_top_level_function_call_from_import() {
        let a = r#"
def f(x, y):
    getattr(x, y)
"#;

        let b = r#"
from a import f
f(x, y)  # E: unsafe-function-call
"#;
        check_all(vec![("a", a), ("b", b)]);
    }

    #[test]
    fn test_top_level_function_call_from_import_effects() {
        let code = r#"
from os import path
path.join("a", "b")  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_definition_with_side_effect_call() {
        let code = r#"
import os

def foo():
    os.path.join("a", "b")
"#;
        check(code);
    }

    #[test]
    fn test_local_function_call() {
        let code = r#"
def f(x, y): ...
f("a", "b")
"#;
        check(code);
    }

    #[test]
    fn test_local_function_call_in_assignment() {
        let code = r#"
def f(x, y): ...
a = f("a", "b")
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_local_function_call() {
        // Only the call itself should be marked with an error, not also the
        // nested callsite
        let code = r#"
from foo import f
def g():
    f.x = 10

def h():
    g()

a = h()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_recursive_function_call() {
        // Only the call itself should be marked with an error, not also the
        // nested callsite
        let code = r#"
def f():
    g()

def g():
    f()

a = f()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_exec() {
        let code = r#"
def f(x):
    def g(x):
        exec("import foo")  # E: exec-call
"#;
        check(code);
    }

    #[test]
    fn test_method_call_effects() {
        let code1 = r#"
            class C:
                def m(self):
                    pass

            def f():
                pass
            a = C() # E: function-call
            a.m() # E: method-call
        "#;
        let code2 = r#"
            import m1
            from m1 import C
            c = C() # E: imported-function-call
            c.m() # E: method-call  # E: imported-type-attr
        "#;
        check_all_effects(vec![("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_method_call() {
        let code = r#"
            class C:
                def m(self):
                    raise Exception()
            a = C()
            a.m() # E: unsafe-method-call
        "#;
        check(code);
    }

    #[test]
    fn test_safe_method_call() {
        let code = r#"
            class C:
                def m(self):
                    pass
            a = C()
            a.m()
        "#;
        check(code);
    }

    #[test]
    fn test_imported_method_call() {
        let code1 = r#"
            class C:
                def m(self):
                    raise Exception()
        "#;
        let code2 = r#"
            from m1 import C
            c = C()
            c.m() # E: unsafe-method-call
        "#;
        check_all(vec![("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_dynamic_imports_dunder_import() {
        let code = r#"
c = __import__("sys") # E: prohibited-call

__import__("sys").path # E: prohibited-call
__import__("sys").path.append("my-dir") # E: prohibited-call # E: unknown-function-call
"#;
        check(code);
    }

    #[test]
    fn test_dynamic_imports_in_func_are_safe() {
        let code = r#"
def test_func():
    c = __import__("sys")

    __import__("sys").path
    __import__("sys").path.append("my-dir")
"#;
        check(code);
    }

    #[test]
    fn test_dynamic_imports_importlib() {
        // Once we handle builtins, these specific methods should likely be
        // processed as prohibited
        let code = r#"
import importlib
from importlib import import_module

a = importlib.import_module("sys")
b = importlib.__import__("math") # E: unsafe-function-call

import_module("bar")
"#;
        check(code);
    }

    #[test]
    fn test_dynamic_imports_importlib_submodule() {
        // `import importlib.util` makes `importlib` available in the namespace,
        // so `importlib.import_module()` should still be detected as unsafe.
        let code = r#"
import importlib.util

a = importlib.import_module("sys")
"#;
        check(code);
    }

    #[test]
    fn test_aliased_function_call() {
        let code = r#"
def f(x): pass
g = f
g(10)  # E: unknown-function-call
"#;
        check(code);
    }

    #[test]
    fn test_lambda_call() {
        let code = r#"
f = lambda x: x + 1
f(10)  # E: unknown-function-call
"#;
        check(code);
    }

    #[test]
    fn test_builtins() {
        let code = r#"
a = input() # E: prohibited-call
print(a)  # safe
"#;
        check(code);
    }

    #[test]
    fn test_bound_classmethod_ownership_unsafe() {
        // Extension of test_bound_method_ownership making the function call
        // use a prohibited call.
        let code1 = r#"
            class C:
                @classmethod
                def f(cls) -> None:
                    input()
        "#;
        let code2 = r#"
            from m1 import C
            x = C.f
            x() # E: unsafe-function-call
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_function_from_subscript() {
        let code = r#"
import foo
a = foo.funcs[1]() # E: unknown-function-call
"#;
        check(code);
    }

    #[test]
    fn test_param_method_call_effect() {
        let code = r#"
def f(x):
    x.foo()  # E: method-call # E: param-method-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_imported_var_arg_effect() {
        let code = r#"
from foo import A

def f(x, y, z):
    pass

f(1, A, 2)  # E: function-call # E: imported-var-argument
"#;
        check_effects(code);
    }

    #[test]
    fn test_multiple_imported_var_arg_effects() {
        let code = r#"
from foo import A
from bar import B

def f(x, y, z):
    pass

f(A, 1, B)  # E: function-call # E: imported-var-argument # E: imported-var-argument
f(A, y=B, z=1)  # E: function-call # E: imported-var-argument # E: imported-var-argument
"#;
        check_effects(code);
    }

    #[test]
    fn test_imported_var_arg() {
        let code = r#"
from foo import A

def f(x, y, z):
    x.bar()

f(1, A, 2)  # E: unsafe-function-call
f(1, y=A, z=2)  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_list_append() {
        let code = r#"
x = []
x.append(10)
"#;
        check(code);
    }

    #[test]
    fn test_safe_imported_var_arg() {
        let code = r#"
from foo import A

def f(x, y, z):
    pass

f(1, A, 2)  # safe because f does not call any methods on its params
"#;
        check(code);
    }

    #[test]
    fn test_safe_import_module_fully_qualified_name() {
        let foo = r#"
def f(x):
    pass
"#;
        let bar = r#"
import importlib
importlib.import_module("foo")
foo.f()
"#;
        check_all(vec![("foo", foo), ("bar", bar)]);
    }

    #[test]
    fn test_safe_import_module() {
        let foo = r#"
def f(x):
    pass
"#;
        let bar = r#"
from importlib import import_module
import_module("foo")
foo.f()
"#;
        check_all(vec![("foo", foo), ("bar", bar)]);
    }

    #[test]
    fn test_safe_import_module_assign() {
        let foo = r#"
def f(x):
    pass
"#;
        let bar = r#"
from importlib import import_module
Apple = import_module("foo")
Apple.f()
"#;
        check_all(vec![("foo", foo), ("bar", bar)]);
    }

    #[test]
    fn test_unsafe_importlib_reassigned_uncallable() {
        // TODO: This should not pass
        let code = r#"
import importlib
importlib = {}
importlib.import_module("foo")
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_importlib_reassigned_callable() {
        // TODO: This should not pass
        let code = r#"
from importlib import import_module

def import_module():
        pass

import_module("foo")
"#;
        check(code);
    }

    #[test]
    fn test_import_module_package_positional_args() {
        let foo_bar = r#"
def f():
    pass
"#;
        let __main__ = r#"
from importlib import import_module
import_module(".bar", "foo")
foo.bar.f()
"#;
        check_all(vec![("foo.bar", foo_bar), ("__main__", __main__)]);
    }

    #[test]
    fn test_import_module_package_kw_args() {
        let foo_bar = r#"
def f():
    pass
"#;
        let __main__ = r#"
from importlib import import_module
A = import_module(package="foo", name=".bar")
A.f()
"#;
        check_all(vec![("foo.bar", foo_bar), ("__main__", __main__)]);
    }

    #[test]
    fn test_import_module_package_combined_args() {
        let foo_bar = r#"
def f():
    pass
"#;
        let __main__ = r#"
from importlib import import_module
import_module("..bar", package="foo.baz")
foo.bar.f()
"#;
        check_all(vec![("foo.bar", foo_bar), ("__main__", __main__)]);
    }

    #[test]
    fn test_reexported_unsafe_class() {
        let foo = r#"
class Foo:
    def __init__(self) -> None:
        raise Exception
"#;
        let bar = r#"
from foo import Foo
"#;
        let baz = r#"
from bar import Foo
x = Foo()  # E: unsafe-function-call
"#;
        check_all(vec![("foo", foo), ("bar", bar), ("baz", baz)]);
    }

    #[test]
    fn test_reexported_safe_class() {
        let foo = r#"
class Foo:
    def __init__(self) -> None:
        pass
"#;
        let bar = r#"
from foo import Foo
"#;
        let baz = r#"
from bar import Foo
x = Foo()
"#;
        check_all(vec![("foo", foo), ("bar", bar), ("baz", baz)]);
    }

    #[test]
    fn test_reexported_chain_unsafe_class() {
        let foo = r#"
class Foo:
    def __init__(self) -> None:
        raise Exception
"#;
        let bar = r#"
from foo import Foo
"#;
        let baz = r#"
from bar import Foo
"#;
        let consumer = r#"
from baz import Foo
x = Foo()  # E: unsafe-function-call
"#;
        check_all(vec![
            ("foo", foo),
            ("bar", bar),
            ("baz", baz),
            ("consumer", consumer),
        ]);
    }

    #[test]
    fn test_reexport_cycle_terminates() {
        // `a.X` and `b.X` re-export each other with no real definition.
        // resolve_re_export must break out of the cycle rather than spin.
        let a = r#"
from b import X
"#;
        let b = r#"
from a import X
"#;
        let main = r#"
from a import X
x = X()
"#;
        check_all(vec![("a", a), ("b", b), ("main", main)]);
    }

    #[test]
    fn test_reexported_unsafe_function() {
        let foo = r#"
def f():
    raise Exception
"#;
        let bar = r#"
from foo import f
"#;
        let baz = r#"
from bar import f
f()  # E: unsafe-function-call
"#;
        check_all(vec![("foo", foo), ("bar", bar), ("baz", baz)]);
    }
}
