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
    fn test_precise_match_keyword_arg() {
        // Keyword args are now precisely matched by name.
        // A is passed to y, but only x is mutated → safe.
        let code = r#"
from foo import A

def f(x, y):
    x.attr = 10

f(y=A, x=1)
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
    // Mutation classification: non-mutating builtin methods on params
    //
    // When a param has no type annotation, method calls are checked against
    // all builtin types that define the method. If none have a Mutation
    // effect, both MethodCall and ParamMethodCall are suppressed.
    // =========================================================================

    #[test]
    fn test_non_mutating_method_on_param_is_safe() {
        let code = r#"
from foo import A

def f(x):
    y = x.copy()

f(A)
"#;
        check(code);
    }

    #[test]
    fn test_dict_get_on_param_is_safe() {
        let code = r#"
from foo import A

def f(d):
    return d.get("key")

f(A)
"#;
        check(code);
    }

    #[test]
    fn test_list_index_on_param_is_safe() {
        let code = r#"
from foo import A

def f(items):
    return items.index(42)

f(A)
"#;
        check(code);
    }

    #[test]
    fn test_mutating_builtin_method_on_param_is_unsafe() {
        // list.append() is a mutating method → function is unsafe
        let code = r#"
from foo import A

def f(x):
    x.append(1)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_custom_class_copy_suppressed_for_untyped_param() {
        // When the param has no type annotation, the builtin-safe heuristic
        // suppresses copy() since it's safe across all builtin types. This is
        // a known false negative for custom classes with side-effecting copy().
        let registry_mod = r#"
class Registry:
    def copy(self):
        print("side effect!")
        return Registry()

registry = Registry()
"#;
        let main_mod = r#"
from registry_mod import registry

def f(x):
    x.copy()

f(registry)
"#;
        check_all(vec![("registry_mod", registry_mod), ("main_mod", main_mod)]);
    }

    // =========================================================================
    // StmtDelete: del x[k] and del x.attr on function parameters
    //
    // Deleting a subscript or attribute of a parameter is a mutation.
    // Handled by Stmt::Delete arm in stmt() calling check_assign_target.
    // =========================================================================

    #[test]

    fn test_param_del_subscript_with_imported_arg() {
        // del x["key"] on a param is a mutation (calls __delitem__)
        let code = r#"
from foo import d

def f(x):
    del x["key"]

f(d)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]

    fn test_param_del_subscript_safe_without_imported_arg() {
        // del x["key"] with a non-imported arg is safe
        let code = r#"
def f(x):
    del x["key"]

f({})
"#;
        check(code);
    }

    #[test]

    fn test_param_del_attr_with_imported_arg() {
        // del x.attr on a param is a mutation (calls __delattr__)
        let code = r#"
from foo import obj

def f(x):
    del x.enabled

f(obj)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]

    fn test_param_del_attr_safe_without_imported_arg() {
        // del x.attr with a non-imported arg is safe
        let code = r#"
def f(x):
    del x.enabled

f(10)
"#;
        check(code);
    }

    #[test]

    fn test_param_del_subscript_effect() {
        // Effects-level: del x["key"] should generate param-method-call
        let code = r#"
def f(x):
    del x["key"]  # E: param-method-call
"#;
        check_effects(code);
    }

    #[test]

    fn test_param_del_attr_effect() {
        // Effects-level: del x.attr should generate param-method-call
        let code = r#"
def f(x):
    del x.enabled  # E: param-method-call
"#;
        check_effects(code);
    }

    #[test]

    fn test_param_del_subscript_precise_match() {
        // Only first param has del, imported var at second position → safe
        let code = r#"
from foo import A

def f(x, y):
    del x["key"]

f(1, A)
"#;
        check(code);
    }

    #[test]

    fn test_param_del_attr_precise_match() {
        // Only second param has del, imported var at second position → unsafe
        let code = r#"
from foo import A

def f(x, y):
    del y.attr

f(1, A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_del_bare_imported_name_is_safe() {
        // del on a bare imported name unbinds the name from the namespace.
        // It does NOT mutate the imported object — no effect should be raised.
        let code = r#"
from foo import bar

del bar
"#;
        check(code);
    }

    #[test]

    fn test_param_del_cross_module() {
        // Cross-module: imported function with del on param
        let setup = r#"
def cleanup(x):
    del x["temp"]
"#;
        let main = r#"
from setup import cleanup
from config import settings

cleanup(settings)  # E: imported-var-argument
"#;
        check_all(vec![("setup", setup), ("main", main)]);
    }

    // =========================================================================
    // Mixed positional + keyword args: regression test for project.rs continue bug
    //
    // When a call has BOTH positional and keyword imported args, the positional
    // check must NOT skip keyword matching on negative match.
    // =========================================================================

    #[test]
    fn test_mixed_positional_and_keyword_only_keyword_mutated() {
        // f mutates param `y` (keyword), imported var B passed to `y` by keyword.
        // Imported var A is at positional index 0, but x is not mutated.
        // The keyword match for y=B must fire even though positional check ran.
        let code = r#"
from foo import A
from foo import B

def f(x, y):
    y.attr = True

f(A, y=B)  # E: imported-var-argument
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
    fn test_keyword_arg_precise_matching() {
        // Keyword args matched to specific params.
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
    fn test_keyword_arg_precise_matching_safe() {
        // f mutates param `target`, but the imported var is passed to `source`.
        // Only unsafe-function-call fires, not imported-var-argument.
        let code = r#"
from foo import A

def f(source, target):
    target.extend(source)

f(A, target=[])  # E: unsafe-function-call
"#;
        check(code);
    }
}
