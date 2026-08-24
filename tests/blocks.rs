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
    fn test_if_effects() {
        let code = r#"
from foo import f, g
if f():  # E: imported-function-call
    g()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_for_effects() {
        let code = r#"
from foo import f, g
for x in f():  # E: imported-function-call
    g()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_raise_expression_effects() {
        let code = r#"
from foo import f, g
try:
    raise f() from g()  # E: imported-function-call  # E: imported-function-call
except Exception:
    pass
"#;
        check_effects(code);
    }

    #[test]
    fn test_assert_effects() {
        let code = r#"
from foo import f, g
assert f()  # E: imported-function-call
assert True, g()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_assert_effects_in_a_called_function() {
        let code = r#"
from foo import f
def g():
    assert f()

g()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_return_effects() {
        let code = r#"
from foo import f
def h():
    return f()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_for_target() {
        let code = r#"
import foo
for foo.x in [1,2,3]:  # E: imported-module-assignment
    ...
"#;
        check(code);
    }

    #[test]
    fn test_for_target_effects() {
        let code = r#"
import foo
for foo.x in [1,2,3]:  # E: imported-var-mutation
    ...
"#;
        check_effects(code);
    }

    #[test]
    fn test_for_target_subscript_effects() {
        let code = r#"
import foo
for foo[0] in [1,2,3]:  # E: imported-var-mutation
    ...
"#;
        check_effects(code);
    }

    #[test]
    fn test_while_effects() {
        let code = r#"
from foo import f, g
while f():  # E: imported-function-call
    g()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_with_effects() {
        let code = r#"
from foo import f, g
with f() as x:  # E: imported-function-call
    g()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_match_effects() {
        let code = r#"
from foo import f, g
match f():  # E: imported-function-call
    case A:
        g()  # E: imported-function-call
    case _:
        g()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_with_block_import_marked_as_called() {
        let code = r#"
with open("f") as x:
    import foo
        "#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let out = run_module_analysis(code, &parsed_module);
        check_imports(
            out,
            vec![("test", vec!["foo"])],
            vec![("test", vec!["foo"])],
        );
    }

    #[test]
    fn test_with_block_from_import_marked_as_called() {
        let code = r#"
with open("f") as x:
    from foo import bar
        "#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let out = run_module_analysis(code, &parsed_module);
        check_imports(
            out,
            vec![("test", vec!["foo"])],
            vec![("test", vec!["foo"])],
        );
    }

    #[test]
    fn test_with_block_multiple_imports_marked_as_called() {
        let code = r#"
with open("f") as x:
    import foo
    import bar
    from baz import quux
        "#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let out = run_module_analysis(code, &parsed_module);
        check_imports(
            out,
            vec![("test", vec!["bar", "baz", "foo"])],
            vec![("test", vec!["bar", "baz", "foo"])],
        );
    }

    #[test]
    fn test_nested_with_block_import_marked_as_called() {
        let code = r#"
with open("f") as x:
    with open("g") as y:
        import foo
        "#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let out = run_module_analysis(code, &parsed_module);
        check_imports(
            out,
            vec![("test", vec!["foo"])],
            vec![("test", vec!["foo"])],
        );
    }

    #[test]
    fn test_with_block_import_in_function() {
        let code = r#"
def f():
    with open("f") as x:
        import foo
        "#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let out = run_module_analysis(code, &parsed_module);
        check_imports(out, vec![("test.f", vec!["foo"])], vec![]);
    }

    #[test]
    fn test_with_block_import_in_called_function() {
        let code = r#"
def f():
    with open("f") as x:
        import foo

f()
        "#;
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        let out = run_module_analysis(code, &parsed_module);
        check_imports(
            out,
            vec![("test.f", vec!["foo"])],
            vec![("test.f", vec!["foo"])],
        );
    }

    #[test]
    fn test_name_main_guard_pruned() {
        let code = r#"
from foo import f
if __name__ == '__main__':
    f()
"#;
        check_effects_not_main(code);
    }

    #[test]
    fn test_name_main_guard_reversed() {
        let code = r#"
from foo import f
if '__main__' == __name__:
    f()
"#;
        check_effects_not_main(code);
    }

    #[test]
    fn test_name_main_guard_double_quotes() {
        let code = r#"
from foo import f
if __name__ == "__main__":
    f()
"#;
        check_effects_not_main(code);
    }

    #[test]
    fn test_name_main_guard_else_analyzed() {
        let code = r#"
from foo import f
if __name__ == '__main__':
    f()
else:
    f()  # E: imported-function-call
"#;
        check_effects_not_main(code);
    }

    #[test]
    fn test_name_main_guard_not_eq_not_pruned() {
        let code = r#"
from foo import f
if __name__ != '__main__':
    f()  # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_name_main_guard_with_other_code() {
        let code = r#"
from foo import f
f()  # E: imported-function-call
if __name__ == '__main__':
    f()
"#;
        check_effects_not_main(code);
    }

    #[test]
    fn test_main_module_guard_not_pruned() {
        let code = r#"
from foo import f
if __name__ == '__main__':
    f()  # E: imported-function-call
"#;
        check_effects_as_main(code);
    }

    #[test]
    fn test_main_module_else_pruned() {
        let code = r#"
from foo import f
if __name__ == '__main__':
    f()  # E: imported-function-call
else:
    f()
"#;
        check_effects_as_main(code);
    }

    #[test]
    fn test_main_module_elif_pruned() {
        let code = r#"
from foo import f, g
if __name__ == '__main__':
    f()  # E: imported-function-call
elif g():
    f()
else:
    f()
"#;
        check_effects_as_main(code);
    }

    #[test]
    fn test_main_module_other_code_still_analyzed() {
        let code = r#"
from foo import f
f()  # E: imported-function-call
if __name__ == '__main__':
    f()  # E: imported-function-call
"#;
        check_effects_as_main(code);
    }

    #[test]
    fn test_non_main_module_guard_still_pruned() {
        let code = r#"
from foo import f
if __name__ == '__main__':
    f()
"#;
        check_effects_not_main(code);
    }

    #[test]
    fn test_no_main_module_guard_pruned_everywhere() {
        let code = r#"
from foo import f
if __name__ == '__main__':
    f()
"#;
        check_effects_no_main(code);
    }

    #[test]
    fn test_no_main_module_other_code_still_analyzed() {
        let code = r#"
from foo import f
f()  # E: imported-function-call
if __name__ == '__main__':
    f()
"#;
        check_effects_no_main(code);
    }
}
