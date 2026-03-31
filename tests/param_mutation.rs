/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::*;

    // -----------------------------------------------------------------------
    // Param method call: f(imported_var) where f calls x.method()
    // The method call itself makes the function unsafe (unresolved method),
    // and the imported arg triggers imported-var-argument.
    // -----------------------------------------------------------------------

    #[test]
    fn test_param_method_call_with_imported_arg() {
        let code = r#"
from foo import A

def f(x):
    x.bar()

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_param_method_call_unsafe_without_imported_arg() {
        // x.bar() is an unresolved method call, so f is unsafe regardless of args
        let code = r#"
def f(x):
    x.bar()

f(10)  # E: unsafe-function-call
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // Param subscript mutation: f(imported_var) where f does x[k] = v
    // The subscript assignment generates ParamMethodCall but doesn't make
    // the function inherently unsafe. Only imported-var-argument is raised.
    // -----------------------------------------------------------------------

    #[test]
    fn test_param_subscript_mutation_with_imported_arg() {
        let code = r#"
from foo import d

def f(x):
    x["key"] = "value"

f(d)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_param_subscript_mutation_safe_without_imported_arg() {
        let code = r#"
def f(x):
    x["key"] = "value"

f({})
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // Param attribute mutation: f(imported_var) where f does x.attr = v
    // Attribute assignment on a param generates ParamMethodCall.
    // -----------------------------------------------------------------------

    #[test]
    fn test_param_attr_mutation_with_imported_arg() {
        let code = r#"
from foo import obj

def f(x):
    x.enabled = True

f(obj)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_param_attr_mutation_safe_without_imported_arg() {
        let code = r#"
def f(x):
    x.enabled = True

f(10)
"#;
        check(code);
    }

    #[test]
    fn test_param_attr_mutation_nested_function() {
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

    // -----------------------------------------------------------------------
    // Effects-level tests: verify ParamMethodCall effect is generated
    // -----------------------------------------------------------------------

    #[test]
    fn test_param_attr_mutation_effect() {
        let code = r#"
def f(x):
    x.attr = 10  # E: param-method-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_param_subscript_mutation_effect() {
        let code = r#"
def f(x):
    x["key"] = "value"  # E: param-method-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_param_method_call_effect() {
        let code = r#"
def f(x):
    x.foo()  # E: method-call # E: param-method-call
"#;
        check_effects(code);
    }

    // -----------------------------------------------------------------------
    // Multiple param mutations in one function
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_param_attr_mutations() {
        let code = r#"
from foo import obj

def configure(x):
    x.debug = True
    x.verbose = False

configure(obj)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_param_mutations_with_method_call() {
        // x.items.append() is an unresolved method call, so the function is
        // also inherently unsafe
        let code = r#"
from foo import obj

def configure(x):
    x.debug = True
    x.items.append("new")

configure(obj)  # E: imported-var-argument # E: unsafe-function-call
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // Cross-module: imported function that mutates param
    // -----------------------------------------------------------------------

    #[test]
    fn test_imported_function_mutates_param() {
        let setup = r#"
def configure(x):
    x.enabled = True
"#;
        let main = r#"
from setup import configure
from config import settings

configure(settings)  # E: imported-var-argument
"#;
        check_all(vec![("setup", setup), ("main", main)]);
    }

    // -----------------------------------------------------------------------
    // Combination: method call + attr mutation on different params
    // -----------------------------------------------------------------------

    #[test]
    fn test_param_attr_mutation_multiple_params() {
        let code = r#"
from foo import A

def f(x, y):
    x.attr = 10
    y.method()

f(A, A)  # E: imported-var-argument # E: imported-var-argument # E: unsafe-function-call
"#;
        check(code);
    }

    // -----------------------------------------------------------------------
    // Precise arg-param matching
    // -----------------------------------------------------------------------

    #[test]
    fn test_precise_match_imported_at_unmutated_position() {
        // Imported var at position 1, but only position 0 (x) is mutated.
        // With precise matching, no imported-var-argument error.
        let code = r#"
from foo import A

def f(x, y, z):
    x["key"] = "value"

f(1, A, 2)
"#;
        check(code);
    }

    #[test]
    fn test_precise_match_imported_at_mutated_position() {
        // Imported var at position 0, and position 0 (x) is mutated.
        let code = r#"
from foo import A

def f(x, y, z):
    x["key"] = "value"

f(A, 1, 2)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_precise_match_safe_read_only_function() {
        // Function only reads param, no mutation.
        let a = r#"
def read_config(config):
    x = config["key"]
    return x
"#;
        let b = r#"
from a import read_config
from other import config
read_config(config)
"#;
        check_all(vec![("a", a), ("b", b)]);
    }

    #[test]
    fn test_precise_match_cross_module_attr_mutation() {
        let a = r#"
def configure(obj):
    obj.setting = True
"#;
        let b = r#"
from a import configure
from other import obj
configure(obj)  # E: imported-var-argument
"#;
        check_all(vec![("a", a), ("b", b)]);
    }

    #[test]
    fn test_precise_match_cross_module_method_call() {
        let a = r#"
def add_item(lst):
    lst.append(42)
"#;
        let b = r#"
from a import add_item
from other import items
add_item(items)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check_all(vec![("a", a), ("b", b)]);
    }

    #[test]
    fn test_precise_match_second_param_mutated() {
        // Only second param is mutated, imported var at second position.
        let code = r#"
from foo import B

def f(x, y):
    y.attr = 10

f(1, B)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_precise_match_second_param_safe() {
        // Only first param is mutated, imported var at second position.
        let code = r#"
from foo import B

def f(x, y):
    x.attr = 10

f(1, B)
"#;
        check(code);
    }

    #[test]
    fn test_precise_match_keyword_arg_fallback() {
        // Keyword args can't be precisely matched by index, so we fall back
        // to the coarse check.
        let code = r#"
from foo import A

def f(x, y):
    x.attr = 10

f(y=A, x=1)  # E: imported-var-argument
"#;
        check(code);
    }

    // =========================================================================
    // Additional edge cases
    // =========================================================================

    #[test]
    fn test_safe_function_no_mutations_with_imported_arg() {
        // Function body has pass → no mutations → safe with any args
        let code = r#"
from foo import A

def f(x):
    pass

f(A)
"#;
        check(code);
    }

    #[test]
    fn test_safe_subscript_read_with_imported_arg() {
        // Reading from a subscript on a param is not a mutation
        let code = r#"
from foo import A

def f(x):
    return x[0]

f(A)
"#;
        check(code);
    }

    #[test]
    fn test_safe_builtin_calls_with_imported_arg() {
        // Builtin functions like len() don't mutate their args
        let code = r#"
from foo import A

x = len(A)
y = str(A)
z = list(A)
"#;
        check(code);
    }

    #[test]
    fn test_mutation_in_function_scope_only() {
        // Mutation inside a nested function call at module level should only
        // flag when the outer function is called at module scope
        let code = r#"
from foo import A

def modify(x):
    x.append(1)

def caller():
    modify(A)
"#;
        check(code);
    }

    #[test]
    fn test_imported_var_alias_passed_to_mutating_function() {
        // Variable aliased from an import, then passed to a mutating function
        let code = r#"
from foo import bar

baz = bar

def modify(x):
    x.append(1)

modify(baz)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_module_attr_passed_to_mutating_function() {
        // Accessing an attribute of an imported module and passing to a
        // mutating function
        let code = r#"
import foo

def modify(x):
    x.append(1)

modify(foo.bar)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_star_args_with_imported_var() {
        // Imported var passed to *args, function doesn't mutate → safe
        let code = r#"
from foo import A

def f(*args):
    pass

f(A)
"#;
        check(code);
    }

    #[test]
    fn test_kwargs_with_imported_var() {
        // Imported var passed via **kwargs, function doesn't mutate → safe
        let code = r#"
from foo import A

def f(**kwargs):
    pass

f(x=A)
"#;
        check(code);
    }

    #[test]
    fn test_registry_pattern_global_mutation() {
        // Common pattern: registry as global list, register function mutates it.
        // The function is unsafe because it mutates a global, not because of
        // parameter mutation.
        let a = r#"
registry = []

def register(item):
    registry.append(item)
"#;
        let b = r#"
from a import register
register("my_item")  # E: unsafe-function-call
"#;
        check_all(vec![("a", a), ("b", b)]);
    }

    // =========================================================================
    // Future work: mutation classification (ignored until implemented)
    //
    // Non-mutating method calls on params should not trigger errors.
    // The stub system already classifies methods as Mutation via
    // may_mutate_receiver(). These should pass once that logic is applied
    // to param method calls.
    // =========================================================================

    #[test]
    #[ignore] // TODO(T237092592): Use mutation classification for param methods
    fn test_non_mutating_method_on_param_is_safe() {
        // list.copy() does not mutate the receiver.
        // Currently: x.copy() is an unknown method → function is unsafe.
        // Desired: classified as non-mutating → safe.
        let code = r#"
from foo import A

def f(x):
    y = x.copy()

f(A)
"#;
        check(code);
    }

    #[test]
    #[ignore] // TODO(T237092592): Use mutation classification for param methods
    fn test_dict_get_on_param_is_safe() {
        // dict.get() does not mutate the receiver
        let code = r#"
from foo import A

def f(d):
    return d.get("key")

f(A)
"#;
        check(code);
    }

    #[test]
    #[ignore] // TODO(T237092592): Use mutation classification for param methods
    fn test_list_index_on_param_is_safe() {
        // list.index() is a read-only operation
        let code = r#"
from foo import A

def f(items):
    return items.index(42)

f(A)
"#;
        check(code);
    }

    // =========================================================================
    // Future work: advanced patterns (ignored until implemented)
    // =========================================================================

    #[test]
    #[ignore] // TODO(T237092592): Track transitive param mutation
    fn test_transitive_param_mutation() {
        // f passes its param to g which mutates it.
        let code = r#"
from foo import A

def g(y):
    y.append(1)

def f(x):
    g(x)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    #[ignore] // TODO(T237092592): Distinguish copy from alias
    fn test_param_copied_then_mutated_is_safe() {
        // If the param is copied before mutation, the original is not affected.
        let code = r#"
from foo import A

def f(x):
    y = x.copy()
    y.append(1)

f(A)
"#;
        check(code);
    }

    #[test]
    #[ignore] // TODO(T237092592): Track param aliasing
    fn test_param_aliased_then_mutated_is_unsafe() {
        // If the param is aliased (not copied) and the alias is mutated,
        // the original is affected.
        let code = r#"
from foo import A

def f(x):
    y = x
    y.append(1)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    #[ignore] // TODO(T237092592): Precise keyword-to-param matching
    fn test_keyword_arg_precise_matching() {
        // Keyword args should be matchable to specific params.
        // f mutates param `target`, imported var passed to `target` by keyword.
        let code = r#"
from foo import A

def f(source, target):
    target.extend(source)

f([], target=A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    #[ignore] // TODO(T237092592): Precise keyword-to-param matching
    fn test_keyword_arg_precise_matching_safe() {
        // f mutates param `target`, but the imported var is passed to `source`.
        // Only unsafe-function-call should fire, not imported-var-argument.
        let code = r#"
from foo import A

def f(source, target):
    target.extend(source)

f(A, target=[])  # E: unsafe-function-call
"#;
        check(code);
    }
}
