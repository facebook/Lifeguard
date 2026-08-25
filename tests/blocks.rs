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

    /// One row per statement kind the analyzer walks, pinning the expression it is
    /// expected to reach. Exhaustiveness over `Stmt` is enforced by the `match` in
    /// `source_analyzer::stmt`, not here.
    #[test]
    fn test_expressions_are_analyzed_in_every_statement_kind() {
        let statements = [
            "x = f()  # E: imported-function-call",
            "x: int = f()  # E: imported-function-call",
            "x += f()  # E: imported-function-call",
            "del f()[f()]  # E: imported-function-call",
            "del f().x  # E: imported-function-call",
            "f()  # E: imported-function-call",
            "def g():\n    return f()  # E: imported-function-call",
            "def g(x=f()):  # E: imported-function-call\n    pass",
            "class C(f()):  # E: imported-function-call\n    pass",
            "raise f() from f()  # E: raise  # E: imported-function-call",
            "assert True, f()  # E: imported-function-call",
            "@f()  # E: imported-decorator-call\ndef g():\n    pass",
            // PEP 695 defers a type alias' value to first access.
            "type X = f()",
            "if f():  # E: imported-function-call\n    pass",
            "while f():  # E: imported-function-call\n    break",
            "for x in f():  # E: imported-function-call\n    pass",
            "with f():  # E: imported-function-call\n    pass",
            "try:\n    f()  # E: imported-function-call\nfinally:\n    pass",
            "match f():  # E: imported-function-call\n    case _:\n        pass",
        ];

        // Report every kind that was missed, not just the first.
        let missed: Vec<String> = statements
            .into_iter()
            .filter_map(|statement| {
                let code = format!("from foo import f\n{statement}\n");
                effects_mismatch(&code).map(|mismatch| format!("{statement}\n{mismatch}"))
            })
            .collect();

        assert!(
            missed.is_empty(),
            "expressions were not analyzed in:\n{}",
            missed.join("\n\n")
        );
    }

    #[test]
    fn test_del_attribute_effects() {
        let code = r#"
from foo import f
del f().x  # E: imported-function-call
"#;
        // Deleting an attribute evaluates the object it is on.
        check_effects(code);
    }

    #[test]
    fn test_del_subscript_effects() {
        // Expectations are matched per line, so the receiver and the key are split
        // across two of them to pin both.
        let code = r#"
from foo import f, g
del f()[  # E: imported-function-call
    g()  # E: imported-function-call
]
"#;
        // Deleting an element evaluates both the receiver and the key.
        check_effects(code);
    }

    #[test]
    fn test_class_base_effects() {
        let code = r#"
from foo import f, g
class C(f(), metaclass=g()):  # E: imported-function-call  # E: imported-function-call
    pass
"#;
        // Bases and keywords are evaluated where the class is defined.
        check_effects(code);
    }

    /// Looking up a name has no side effect
    #[test]
    fn test_class_base_name_is_not_an_effect() {
        let code = r#"
from foo import Base
class C(Base):
    pass
"#;
        check_effects(code);
    }

    #[test]
    fn test_parameter_default_effects() {
        let code = r#"
from foo import f
def g(a, b=f(), *args, c=f(), **kwargs):  # E: imported-function-call
    pass
"#;
        // The defaults run when `g` is defined, not when it is called.
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
