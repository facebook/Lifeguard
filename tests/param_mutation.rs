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
    // Unbound method calls through the class (explicit receiver)
    // -----------------------------------------------------------------------

    #[test]
    fn test_unbound_method_call_imported_arg() {
        // `C.method(o, A)` passes the receiver explicitly, so the imported `A` is
        // at explicit arg 1, which maps to the mutated parameter `x`. The method
        // is safe in isolation (subscript mutation), so only the param-mutation
        // check can catch it — and it must account for the explicit receiver.
        let code = r#"
from foo import A

class C:
    def method(self, x):
        x["k"] = 1

o = {}
C.method(o, A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_unbound_classmethod_call_imported_arg() {
        // A classmethod called via the class still has an implicit `cls`, so the
        // imported `A` is at explicit arg 0, mapping to mutated parameter `x`.
        // The method's `FieldKind::ClassMethod` gives the correct offset of 1.
        let code = r#"
from foo import A

class C:
    @classmethod
    def make(cls, x):
        x["k"] = 1

C.make(A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_bound_staticmethod_call_imported_arg() {
        // A staticmethod has no implicit receiver even when called bound, so the
        // imported `A` is at explicit arg 0, mapping to mutated parameter `x`.
        // The method's `FieldKind::StaticMethod` gives the correct offset of 0.
        let code = r#"
from foo import A

class C:
    @staticmethod
    def sink(x):
        x["k"] = 1

c = C()
c.sink(A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_unbound_staticmethod_call_imported_arg() {
        // Staticmethod via class has no implicit receiver, offset 0.
        let code = r#"
from foo import A

class C:
    @staticmethod
    def sink(x):
        x["k"] = 1

C.sink(A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_bound_classmethod_call_imported_arg() {
        // Classmethod via instance still has implicit cls receiver, offset 1.
        let code = r#"
from foo import A

class C:
    @classmethod
    def make(cls, x):
        x["k"] = 1

c = C()
c.make(A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_class_alias_unbound_method_call_imported_arg() {
        // Alias to class should still be recognised as unbound call with explicit receiver.
        let code = r#"
from foo import A

class C:
    def method(self, x):
        x["k"] = 1

D = C
o = {}
D.method(o, A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_multihop_aliased_import_mutates_param() {
        // `c` aliases the imported `configure`, then `d = c` re-aliases it.
        // The re-alias must resolve to `setup.configure` (not a re-qualified
        // intermediate) to detect that the call mutates the imported argument.
        let setup = r#"
def configure(x):
    x.enabled = True
"#;
        let main = r#"
from setup import configure as c
from config import settings

d = c
d(settings)  # E: imported-var-argument
"#;
        check_all(vec![("setup", setup), ("main", main)]);
    }

    #[test]
    fn test_multihop_local_realias_import_mutates_param() {
        // `c` is the import alias, `d = c` re-aliases the import, and `e = d`
        // snapshots `d`'s binding, so `e` resolves to `setup.configure`.
        let setup = r#"
def configure(x):
    x.enabled = True
"#;
        let main = r#"
from setup import configure as c
from config import settings

d = c
e = d
e(settings)  # E: imported-var-argument
"#;
        check_all(vec![("setup", setup), ("main", main)]);
    }

    #[test]
    fn test_local_alias_survives_source_reassignment() {
        // `e = d` captures the import binding `d` holds at that point. Rebinding
        // `d` afterwards must not retroactively change `e`, which still refers to
        // `configure`.
        let setup = r#"
def configure(x):
    x.enabled = True
"#;
        let main = r#"
from setup import configure as c
from config import settings

d = c
e = d
d = lambda s: None
e(settings)  # E: imported-var-argument
"#;
        check_all(vec![("setup", setup), ("main", main)]);
    }

    #[test]
    fn test_reassigned_import_alias_is_safe() {
        // Reassigning `y` to a lambda drops its earlier import alias, so `y()` is
        // not flagged as imported-var-argument; unknown-function-call still fires
        // because a lambda is not a known-safe call.
        let setup = r#"
def configure(x):
    x.enabled = True
"#;
        let main = r#"
from setup import configure as c
from config import settings

y = c
y = lambda s: None
y(settings)  # E: unknown-function-call
"#;
        check_all(vec![("setup", setup), ("main", main)]);
    }

    #[test]
    fn test_self_assignment_preserves_alias() {
        // Self-assignment (`d = d`) is a no-op and must preserve the alias chain.
        let setup = r#"
def configure(x):
    x.enabled = True
"#;
        let main = r#"
from setup import configure as c
from config import settings

d = c
d = d
d(settings)  # E: imported-var-argument
"#;
        check_all(vec![("setup", setup), ("main", main)]);
    }

    #[test]
    fn test_shadowed_alias_in_nested_scope() {
        // Inner scope shadows outer alias name; outer alias should remain intact
        // for outer scope calls, inner should resolve separately.
        // clear_alias only affects current scope, so shadowing is safe.
        let setup = r#"
def configure(x):
    x.enabled = True
"#;
        let main = r#"
from setup import configure as c
from config import settings

def inner():
    c = lambda s: None
    c(settings)

c(settings)  # E: imported-var-argument
"#;
        check_all(vec![("setup", setup), ("main", main)]);
    }

    #[test]
    fn test_five_hop_aliased_import() {
        // Import binding propagated through a five-assignment chain.
        let setup = r#"
def configure(x):
    x.enabled = True
"#;
        let main = r#"
from setup import configure as a
from config import settings
b = a
c = b
d = c
e = d
e(settings)  # E: imported-var-argument
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
    // Too many positional args: >64 args exceeds the tracking bitset
    // =========================================================================

    #[test]
    fn test_too_many_args() {
        // A call with more than 64 positional args to a function we can't vouch
        // for triggers a too-many-args error: the args overflow the tracking
        // bitset and no stub provides the callee's effect.
        let args = (0..65)
            .map(|i| format!("{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            r#"
def f(*args):
    pass

f({})  # E: too-many-args
"#,
            args
        );
        check(&code);
    }

    #[test]
    fn test_too_many_args_stub_callee_is_fine() {
        // More than 64 args is fine when a stub provides the callee's effect:
        // the stub's effect governs instead of the conservative fallback. This
        // mirrors the module-scope `thrift.python.types.fill_specs(*specs)` call
        // emitted by generated thrift-python service modules.
        let args = (0..65)
            .map(|i| format!("{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            r#"
import lifeguard_test

lifeguard_test.bar({})
"#,
            args
        );
        check(&code);
    }

    #[test]
    fn test_64_args_is_fine() {
        // Exactly 64 positional args is within the bitset capacity — no error.
        let args = (0..64)
            .map(|i| format!("{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            r#"
def f(*args):
    pass

f({})
"#,
            args
        );
        check(&code);
    }

    // =========================================================================
    // Future work: advanced patterns (ignored until implemented)
    // =========================================================================

    #[test]
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
    fn test_three_level_transitive_param_forwarding() {
        // f -> g -> h chain, h mutates, should propagate to f.
        let code = r#"
from foo import A

def h(z):
    z.append(1)

def g(y):
    h(y)

def f(x):
    g(x)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_diamond_param_forwarding() {
        // f forwards to both g and i which both forward to h which mutates.
        // Tests fixpoint merge across diamond shape.
        let code = r#"
from foo import A

def h(z):
    z.append(1)

def g(y):
    h(y)

def i(y):
    h(y)

def f(x):
    g(x)
    i(x)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_cycle_param_forwarding() {
        // f and g forward to each other in a cycle, g mutates.
        // Fixpoint should terminate and mark both f and g params as mutated.
        let code = r#"
from foo import A

def g(y):
    y.append(1)
    f(y)

def f(x):
    g(x)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_nested_function_outer_param_is_not_inner_forwarded_param() {
        let code = r#"
from foo import A

def mutate(v):
    v.append(1)

def outer(x):
    def inner(**kwargs):
        mutate(x)
    inner(**A)

outer([])  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_reassigned_param_not_forwarded() {
        // Known false positive: the parameter is rebound to a fresh list before
        // being forwarded, so forwarding `x` to `g` cannot leak the imported value.
        // Parameter rebinding is not modeled in forwarding analysis (only alias-
        // variable reassignment is, via clear_alias), so this is still flagged.
        let code = r#"
from foo import A

def g(y):
    y.append(1)

def f(x):
    x = []
    g(x)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_star_args_forwarding_wrapped_in_list_not_tracked() {
        // The parameter `x` is wrapped in a list literal before unpacking
        // (`g(*[x])`), so the starred operand is the list, classified as a plain
        // argument rather than a forwarded parameter — container contents are not
        // inspected. The forwarding is therefore not tracked; the call is flagged
        // only because `g` itself is unsafe (the unresolved `y.append` method call).
        let code = r#"
from foo import A

def g(y):
    y.append(1)

def f(x):
    g(*[x])

f(A)  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_star_args_forwarding_param_tracked() {
        // `g(*x)` forwards the parameter `x` directly via star-unpacking. The
        // callee is safe in isolation (subscript mutation only), so flagging
        // depends entirely on the forwarding being tracked: `*x` is recorded as a
        // StarExpansion and conservatively matched against `g`'s mutated
        // positional parameter.
        let code = r#"
from foo import A

def g(y):
    y["k"] = 1

def f(x):
    g(*x)

f(A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_star_args_forwarding_after_prefix_no_false_positive() {
        // `g({}, *x)` fills `a` with `{}`; the star starts at position 1, so `*x`
        // cannot reach `a`. Mutating only `a` must not flag `x`.
        let code = r#"
from foo import A

def g(a, b):
    a["k"] = 1

def f(x):
    g({}, *x)

f(A)
"#;
        check(code);
    }

    #[test]
    fn test_star_args_forwarding_after_prefix_reaches_param() {
        // Same call shape, but the mutated param `b` is at position 1, which the
        // star (starting at position 1) does reach, so `x` is flagged.
        let code = r#"
from foo import A

def g(a, b):
    b["k"] = 1

def f(x):
    g({}, *x)

f(A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_star_import_after_prefix_no_false_positive() {
        // Imported value unpacked after a leading positional arg: `g({}, *A)` at
        // module scope. `*A` starts at position 1 and cannot reach mutated `a`.
        let code = r#"
from foo import A

def g(a, b):
    a["k"] = 1

g({}, *A)
"#;
        check(code);
    }

    #[test]
    fn test_star_import_after_prefix_reaches_param() {
        // `g({}, *A)` where the mutated param `b` is at position 1, reached by the
        // star, so the imported value is flagged.
        let code = r#"
from foo import A

def g(a, b):
    b["k"] = 1

g({}, *A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_star_args_import_in_list_literal_unsafe_callee() {
        // An imported var inside a list literal passed via `*[A]` is not detected
        // as an imported-var-argument: we unwrap only one Starred level to the list
        // and do not recurse into its elements (a known limitation). The
        // unsafe-function-call here is unrelated to the import — it comes from `f`
        // itself (`args[0].append(1)` is an unresolved method call) and would fire
        // for any argument. Future work could recurse into containers for precise
        // imported-var-argument flagging.
        let code = r#"
from foo import A

def f(*args):
    args[0].append(1)

f(*[A])  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_star_args_import_in_list_literal_safe_callee() {
        // Same list-literal limitation, with a safe callee: the imported var is not
        // detected and the body has no side effect, so nothing is flagged.
        let code = r#"
from foo import A

def f(*args):
    pass

f(*[A])
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
    fn test_aliased_param_forwarded_to_mutating_call() {
        // `y` aliases param `x`, then is forwarded to `g`, which mutates its
        // parameter. The alias must be followed when classifying the argument so
        // that `x` is recognized as the forwarded parameter. (`g` is safe in
        // isolation via subscript mutation, so only the forwarding path can flag
        // this.)
        let a = r#"
def g(item):
    item["k"] = 1
"#;
        let main = r#"
from a import g
from foo import A

def f(x):
    y = x
    g(y)

f(A)  # E: imported-var-argument
"#;
        check_all(vec![("a", a), ("main", main)]);
    }

    #[test]
    fn test_param_aliased_two_hops_then_mutated_is_unsafe() {
        // Two-hop local alias chain: z = y; y = x; then mutate z.
        // Should resolve through bindings.resolve to original parameter.
        let code = r#"
from foo import A

def f(x):
    y = x
    z = y
    z.append(1)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_param_aliased_three_hops_then_mutated_is_unsafe() {
        // Three-hop local alias chain
        let code = r#"
from foo import A

def f(x):
    y = x
    z = y
    w = z
    w.append(1)

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_aliased_param_forwarded_two_hops() {
        // Two-hop alias before forwarding should still be tracked.
        let a = r#"
def g(item):
    item["k"] = 1
"#;
        let main = r#"
from a import g
from foo import A

def f(x):
    y = x
    z = y
    g(z)

f(A)  # E: imported-var-argument
"#;
        check_all(vec![("a", a), ("main", main)]);
    }

    #[test]
    fn test_tuple_unpack_alias_not_tracked() {
        // Known limitation: tuple unpack alias is not handled by assign_single
        // which only tracks simple Name targets. Currently treats y as unknown
        // receiver, so flags unsafe-function-call but not imported-var-argument
        // (false negative for param tracking, true positive for unknown call).
        // Documenting for future work to make conservative and track tuple unpack.
        let code = r#"
from foo import A

def f(x):
    y, _ = x, 1
    y.append(1)

f(A)  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_closure_capture_param_alias_mutation() {
        // Nested function mutates captured parameter via local alias.
        // Current implementation attributes ParamMethodCall to enclosing scope
        // via add_param_method_call, so this should be caught.
        let code = r#"
from foo import A

def f(x):
    def g():
        y = x
        y.append(1)
    g()

f(A)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_param_reassigned_then_aliased_is_safe() {
        // Known false positive: the parameter is rebound to a fresh list, so the
        // later alias `y = x` refers to that list, not the imported value. Name
        // resolution still maps `x` to the original parameter (parameter rebinding
        // is not modeled), so mutating `y` is attributed to the parameter. Unlike
        // alias-variable reassignment, clear_alias does not help here.
        let code = r#"
from foo import A

def f(x):
    x = []
    y = x
    y.append(1)


f(A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_param_alias_then_reassigned_is_safe() {
        // `y` aliases the parameter, then is reassigned to a fresh local. The
        // reassignment invalidates the alias, so mutating `y` afterwards must
        // not be attributed to the parameter. (Subscript assignment isolates
        // the parameter-mutation path from unresolved-method-call effects.)
        let code = r#"
from foo import A

def f(x):
    y = x
    y = {}
    y["k"] = 1

f(A)
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

    #[test]
    fn test_bound_method_receiver_param_mutation() {
        // A bound method has `self` as parameter 0, so the imported argument is at
        // explicit position 0 but mutated parameter `x` is at param index 1. The
        // receiver offset must line them up.
        let code = r#"
from foo import A

class Box:
    def sink(self, x):
        x.attr = 1

c = Box()
c.sink(A)  # E: imported-var-argument
"#;
        check(code);
    }

    #[test]
    fn test_constructor_imported_param_mutation() {
        // A call to a class dispatches into `__init__`, whose `self` is parameter
        // 0, so the imported argument at explicit position 0 maps to `x` at param
        // index 1.
        let code = r#"
from foo import A

class Box:
    def __init__(self, x):
        x.attr = 1

Box(A)  # E: imported-var-argument
"#;
        check(code);
    }
}
