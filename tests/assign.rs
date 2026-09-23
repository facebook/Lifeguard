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
    fn test_assign_to_module_var() {
        let code = r#"
import foo
foo.bar = 1 # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_assign_to_module_var_effect() {
        let code = r#"
import foo
foo.bar = 1  # E: imported-var-mutation
"#;
        check_effects(code);
    }

    #[test]
    fn test_augmented_assign_to_module_var() {
        let code = r#"
import foo
foo.bar += 1  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_augmented_assign_to_module_var_effects() {
        let code = r#"
import foo
foo.bar += 1  # E: imported-var-mutation
"#;
        check_effects(code);
    }

    #[test]
    fn test_annotated_assign_to_module_var() {
        let code = r#"
import foo
foo.bar: int = 1  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_annotated_assign_to_module_var_effects() {
        let code = r#"
import foo
foo.bar: int = 1  # E: imported-var-mutation
"#;
        check_effects(code);
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
    fn test_assign_to_import_var() {
        // Assignment shadows imported var, and so is safe
        let code = r#"
from foo import bar
bar = 1
"#;
        check(code);
    }

    #[test]
    fn test_augmented_assign_to_import_var() {
        // Assignment redefines bar and shadows imported var, and so is safe
        let code = r#"
from foo import bar
bar += 1
"#;
        check(code);
    }

    #[test]
    fn test_class_attribute_shadowing_import_is_not_a_mutation() {
        // The class attribute is itself the assignment target, so the target's
        // own lookup has to reach it rather than the import it shadows --
        // otherwise binding a class attribute reads as mutating the import.
        let code = r#"
from foo import bar

class C:
    bar = 1
"#;
        check_effects(code);
    }

    #[test]
    fn test_class_augmented_assign_shadowing_import_is_not_a_mutation() {
        // The half of `<=` its sibling above does not reach: an augmented target
        // reads and binds at the same site.
        let code = r#"
from foo import bar

class C:
    bar += 1
"#;
        check_effects(code);
    }

    #[test]
    fn test_subscript_assign_to_alias() {
        let code1 = r#"
A = []
"#;
        let code2 = r#"
import mod1
x = mod1.A
x[1] = 2  # E: imported-module-assignment
"#;
        let code = vec![("mod1", code1), ("mod2", code2)];
        check_all(code);
    }

    #[test]
    fn test_call_with_import_var_alias() {
        let code = r#"
import foo

baz = foo.bar

def f(x):
    x.a()

f(baz)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_call_with_import_var_alias_2() {
        let code = r#"
from foo import bar

baz = bar

def f(x):
    x.a()

f(baz)  # E: imported-var-argument  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_assign_to_builtins_import() {
        let code = r#"
import builtins

def hook(name, *args, **kwargs):
    return name

builtins.__import__ = hook  # E: builtins-import-override
"#;
        check(code);
    }

    #[test]
    fn test_assign_to_builtins_import_in_function() {
        // resolving a deferred import runs `install`, so reachability does not apply
        let code = r#"
import builtins

def install(hook):
    builtins.__import__ = hook  # E: builtins-import-override
"#;
        check(code);
    }

    #[test]
    fn test_assign_to_builtins_import_via_alias() {
        let code = r#"
import builtins as b

def install(hook):
    b.__import__ = hook  # E: builtins-import-override
"#;
        check(code);
    }

    #[test]
    fn test_setattr_builtins_import() {
        let code = r#"
import builtins

def install(hook):
    setattr(builtins, "__import__", hook)  # E: builtins-import-override
"#;
        check(code);
    }

    #[test]
    fn test_setattr_reports_the_store_once() {
        // The override covers the store, so `builtins` is not reported as a mutation too
        let code = r#"
import builtins

def hook(name, *args, **kwargs):
    return name

setattr(builtins, "__import__", hook)  # E: builtins-import-override
"#;
        check(code);
    }

    #[test]
    fn test_assign_to_other_builtins_attr() {
        // only `__import__` redirects the import machinery
        let code = r#"
import builtins
builtins.my_helper = 1  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_read_builtins_import() {
        // saving the original hook redirects nothing on its own
        let code = r#"
import builtins
original = builtins.__import__
"#;
        check(code);
    }

    #[test]
    fn test_tuple_destructuring_store_to_imported_attr() {
        let code = r#"
import builtins
builtins.my_helper, x = 1, 2  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_list_destructuring_store_to_imported_attr() {
        let code = r#"
import builtins
[builtins.my_helper, x] = 1, 2  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_starred_destructuring_store_to_imported_attr() {
        let code = r#"
import builtins
*builtins.my_helper, x = 1, 2, 3  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_nested_destructuring_store_to_imported_attr() {
        let code = r#"
import builtins
a, (builtins.my_helper, b) = 1, (2, 3)  # E: imported-module-assignment
"#;
        check(code);
    }

    #[test]
    fn test_destructuring_store_to_builtins_import() {
        let code = r#"
import builtins

def install(hook):
    builtins.__import__, x = hook, 1  # E: builtins-import-override
"#;
        check(code);
    }

    #[test]
    fn test_delete_builtins_import() {
        // removing the hook breaks deferred resolution as thoroughly as replacing it
        let code = r#"
import builtins

def uninstall():
    del builtins.__import__  # E: builtins-import-override
"#;
        check(code);
    }
}
