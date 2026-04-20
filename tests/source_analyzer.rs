/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::effects::Effect;
    use lifeguard::effects::EffectKind;
    use lifeguard::effects::EffectTable;
    use lifeguard::module_effects::ModuleEffects;
    use lifeguard::module_parser::parse_source;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::test_lib::assert_str_keys;
    use lifeguard::test_lib::check_imports;
    use lifeguard::test_lib::run_module_analysis;

    fn run_analysis(code: &str) -> ModuleEffects {
        let mod_name = ModuleName::from_str("test");
        let parsed_module = parse_source(code, mod_name, false);
        run_module_analysis(code, &parsed_module)
    }

    fn assert_keys(effs: &EffectTable, keys: Vec<&str>) {
        assert_str_keys(effs.keys(), keys);
    }

    #[test]
    fn test_effect_details() {
        let code = r#"
import os
os.path.join("a", "b")
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        let eff = effs.get(&ModuleName::from_str("test"));
        assert!(eff.is_some());
        let e: Vec<&Effect> = eff.unwrap().iter().collect();
        let e = e[0];
        assert!(matches!(e.kind, EffectKind::ImportedFunctionCall));
        assert_eq!(e.name.as_str(), "os.path.join");
        assert_eq!(e.range.start(), 11.into());
        assert_eq!(e.range.end(), 33.into());
    }

    #[test]
    fn test_effect() {
        let code = r#"
import a
a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        let eff = effs.get(&ModuleName::from_str("test"));
        assert!(eff.is_some());
        let e: Vec<&Effect> = eff.unwrap().iter().collect();
        assert!(matches!(e[0].kind, EffectKind::ImportedVarMutation));
    }

    #[test]
    fn test_effect_in_function_body() {
        let code = r#"
import a
def f():
    a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        let eff = effs.get(&ModuleName::from_str("test.f"));
        assert!(eff.is_some());
        let e: Vec<&Effect> = eff.unwrap().iter().collect();
        assert!(matches!(e[0].kind, EffectKind::ImportedVarMutation));
        let module_eff = effs.get(&ModuleName::from_str("test"));
        assert!(module_eff.is_none());
    }

    #[test]
    fn test_effect_in_class_body() {
        let code = r#"
import a
class A:
    a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test.A"]);
    }

    #[test]
    fn test_effect_in_method_body() {
        let code = r#"
import a
class A:
    def f():
        a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test.A.f"]);
    }

    #[test]
    fn test_effects_in_multiple_scopes() {
        let code = r#"
import a
a.x = 10
class A:
    a.x = 10
    def f():
        a.x = 10
    def g():
        a.x = 10
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec!["test", "test.A", "test.A.f", "test.A.g"]);
    }

    #[test]
    fn test_libfb_lazy_import() {
        let code = r#"
from libfb.py.lazy_import import lazy_import

foo = lazy_import("foo")
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec![]);
    }

    #[test]
    fn test_libfb_lazy_import_qualified() {
        let code = r#"
import libfb.py.lazy_import

foo = libfb.py.lazy_import.lazy_import("foo")
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec![]);
    }

    #[test]
    fn test_libfb_lazy_import_aliased() {
        let code = r#"
from libfb.py.lazy_import import lazy_import as cool_import

foo = cool_import("foo")
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec![]);
    }

    #[test]
    fn test_pytest_mark() {
        let code = r#"
import pytest

@pytest.mark.parametrize
def test_foo():
    pass
"#;
        let out = run_analysis(code);
        let effs = &out.effects;
        assert_keys(effs, vec![]);
    }

    #[test]
    fn test_called_and_pending_imports() {
        let code = r#"
import foo
import foo.bar
import foo.bar.baz

def f():
    import baz
    foo.bar

def g():
    import bar
    foo.bar.baz

g()
        "#;
        let out = run_analysis(code);
        check_imports(
            out,
            vec![
                ("test", vec!["foo", "foo.bar", "foo.bar.baz"]),
                ("test.f", vec!["baz"]),
                ("test.g", vec!["bar"]),
            ],
            vec![("test.g", vec!["bar"])],
        );
    }

    #[test]
    fn test_from_import_attribute_access_adds_called_import() {
        let code = r#"
import foo.bar.baz  # Add to import graph so it's recognized
from foo import bar
bar.baz  # Should add foo.bar.baz as a called import
        "#;
        let out = run_analysis(code);
        check_imports(
            out,
            vec![("test", vec!["foo", "foo.bar", "foo.bar.baz"])],
            vec![("test", vec!["foo.bar.baz"])],
        );
    }

    #[test]
    fn test_from_import_with_alias_attribute_access() {
        let code = r#"
import foo.bar.baz  # Add to import graph so it's recognized
from foo import bar as b
b.baz  # Should add foo.bar.baz as a called import
        "#;
        let out = run_analysis(code);
        check_imports(
            out,
            vec![("test", vec!["foo", "foo.bar", "foo.bar.baz"])],
            vec![("test", vec!["foo.bar.baz"])],
        );
    }

    #[test]
    fn test_from_import_nested_attribute_access() {
        let code = r#"
import foo.bar
import foo.bar.baz
import foo.bar.baz.quux
from foo import bar
bar.baz.quux  # Should add foo.bar.baz and foo.bar.baz.quux as called imports
        "#;
        let out = run_analysis(code);
        check_imports(
            out,
            vec![(
                "test",
                vec!["foo", "foo.bar", "foo.bar.baz", "foo.bar.baz.quux"],
            )],
            vec![("test", vec!["foo.bar.baz", "foo.bar.baz.quux"])],
        );
    }

    #[test]
    fn test_import_vs_from_import_attribute_access() {
        let code = r#"
import foo
import foo.bar
import foo.bar.baz
from baz import quux
import baz.quux
import baz.quux.sub

foo.bar.baz  # import: adds foo.bar.baz
quux.sub  # from import: should add baz.quux.sub
        "#;
        let out = run_analysis(code);
        let called = out.called_imports.get(&ModuleName::from_str("test"));
        assert!(called.is_some(), "Expected called imports for test scope");
        let called_set = called.unwrap();
        assert!(
            called_set.contains(&ModuleName::from_str("foo.bar.baz")),
            "Expected foo.bar.baz in called imports for `import foo; foo.bar.baz`"
        );
        assert!(
            called_set.contains(&ModuleName::from_str("baz.quux.sub")),
            "Expected baz.quux.sub in called imports for `from baz import quux; quux.sub`"
        );
    }
}
