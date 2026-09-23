/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::effects::EffectKind;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::test_lib::analyze_tree;
    use lifeguard::test_lib::check_all;

    fn has_pending_import(
        result: &lifeguard::project::AnalysisMap,
        module: &str,
        scope: &str,
        imported: &str,
    ) -> bool {
        let module_name = ModuleName::from_str(module);
        let scope_name = ModuleName::from_str(scope);
        let imported_name = ModuleName::from_str(imported);
        result
            .get(&module_name)
            .and_then(|am| am.module_effects.pending_imports.get(&scope_name))
            .map(|set| set.contains(&imported_name))
            .unwrap_or(false)
    }

    fn has_effect(
        result: &lifeguard::project::AnalysisMap,
        module: &str,
        kind: EffectKind,
    ) -> bool {
        let module_name = ModuleName::from_str(module);
        result
            .get(&module_name)
            .map(|am| {
                am.module_effects
                    .effects
                    .values()
                    .flatten()
                    .any(|e| e.kind == kind)
            })
            .unwrap_or(false)
    }

    #[test]
    fn test_star_import_with_dunder_all_submodule_tracking() {
        // When module `a` defines __all__ = ["sub"] and `a.sub` is a known module,
        // `from a import *` should register `a.sub` as a pending import.
        let a = r#"
__all__ = ["sub"]
"#;
        let a_sub = r#"
x = 1
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![("a", a), ("a.sub", a_sub), ("main", main)];
        let result = analyze_tree(&modules);

        assert!(
            has_pending_import(&result, "main", "main", "a.sub"),
            "Star import should track a.sub as pending import via __all__"
        );
    }

    #[test]
    fn test_star_import_without_dunder_all() {
        // When the source module has no __all__, star import should not add
        // any submodule pending imports beyond the base module itself.
        let a = r#"
x = 1
"#;
        let a_sub = r#"
y = 2
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![("a", a), ("a.sub", a_sub), ("main", main)];
        let result = analyze_tree(&modules);

        assert!(
            !has_pending_import(&result, "main", "main", "a.sub"),
            "Without __all__, star import should not track a.sub"
        );
    }

    #[test]
    fn test_star_import_dunder_all_non_module_names() {
        // When __all__ contains names that are not known modules (just symbols),
        // they should not be added as pending imports.
        let a = r#"
__all__ = ["foo", "bar"]
foo = 1
bar = 2
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![("a", a), ("main", main)];
        let result = analyze_tree(&modules);

        // "a.foo" and "a.bar" are not known modules, so they should not appear
        assert!(
            !has_pending_import(&result, "main", "main", "a.foo"),
            "Non-module names in __all__ should not become pending imports"
        );
        assert!(
            !has_pending_import(&result, "main", "main", "a.bar"),
            "Non-module names in __all__ should not become pending imports"
        );
    }

    #[test]
    fn test_star_import_no_reassignment_with_from_import() {
        // `from b import foo` creates a re-export with a non-empty source module,
        // so `from a import *` with "foo" in __all__ does not trigger
        // ImportedVarReassignment (the check requires the prior re-export source
        // to have an empty module and attr).
        let a = r#"
__all__ = ["foo"]
foo = 1
"#;
        let b = r#"
foo = 2
"#;
        let main = r#"
from b import foo
from a import *
"#;
        let modules = vec![("a", a), ("b", b), ("main", main)];
        let result = analyze_tree(&modules);

        assert!(
            !has_effect(&result, "main", EffectKind::ImportedVarReassignment),
            "from-import re-exports have non-empty source, so star import does not trigger reassignment"
        );
    }

    #[test]
    fn test_star_import_no_reassignment_without_prior_import() {
        // Star import should not generate ImportedVarReassignment if the name
        // was not previously imported.
        let a = r#"
__all__ = ["foo"]
foo = 1
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![("a", a), ("main", main)];
        let result = analyze_tree(&modules);

        assert!(
            !has_effect(&result, "main", EffectKind::ImportedVarReassignment),
            "No reassignment effect when there is no prior import of the same name"
        );
    }

    #[test]
    fn test_star_import_errors_with_unsafe_source() {
        // A module with side effects imported via `from x import *` should still
        // be detected as an error.
        let a = r#"
__all__ = ["foo"]
input()  # E: prohibited-call
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![("a", a), ("main", main)];
        check_all(modules);
    }

    #[test]
    fn test_star_import_in_function_scope() {
        // Star imports inside functions should track submodules in the function scope.
        // Note: Python 3 forbids `from x import *` inside functions, but the analyzer
        // should still handle it if encountered.
        let a = r#"
__all__ = ["sub"]
"#;
        let a_sub = r#"
x = 1
"#;
        let main = r#"
def f():
    from a import *
"#;
        let modules = vec![("a", a), ("a.sub", a_sub), ("main", main)];
        let result = analyze_tree(&modules);

        assert!(
            has_pending_import(&result, "main", "main.f", "a.sub"),
            "Star import in function should track a.sub under function scope"
        );
    }

    #[test]
    fn test_dunder_all_augmented_assignment() {
        // __all__ += ["extra"] should extend the list.
        let a = r#"
__all__ = ["sub1"]
__all__ += ["sub2"]
"#;
        let a_sub1 = r#"
x = 1
"#;
        let a_sub2 = r#"
y = 2
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![
            ("a", a),
            ("a.sub1", a_sub1),
            ("a.sub2", a_sub2),
            ("main", main),
        ];
        let result = analyze_tree(&modules);

        assert!(
            has_pending_import(&result, "main", "main", "a.sub1"),
            "__all__ should include sub1 from initial assignment"
        );
        assert!(
            has_pending_import(&result, "main", "main", "a.sub2"),
            "__all__ should include sub2 from augmented assignment"
        );
    }

    #[test]
    fn test_dunder_all_extend() {
        // __all__.extend(["extra"]) should add to the list.
        let a = r#"
__all__ = ["sub1"]
__all__.extend(["sub2"])
"#;
        let a_sub1 = r#"
x = 1
"#;
        let a_sub2 = r#"
y = 2
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![
            ("a", a),
            ("a.sub1", a_sub1),
            ("a.sub2", a_sub2),
            ("main", main),
        ];
        let result = analyze_tree(&modules);

        assert!(
            has_pending_import(&result, "main", "main", "a.sub1"),
            "__all__ should include sub1 from initial assignment"
        );
        assert!(
            has_pending_import(&result, "main", "main", "a.sub2"),
            "__all__ should include sub2 from extend()"
        );
    }

    #[test]
    fn test_dunder_all_append() {
        // __all__.append("extra") should add a single name.
        let a = r#"
__all__ = ["sub1"]
__all__.append("sub2")
"#;
        let a_sub1 = r#"
x = 1
"#;
        let a_sub2 = r#"
y = 2
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![
            ("a", a),
            ("a.sub1", a_sub1),
            ("a.sub2", a_sub2),
            ("main", main),
        ];
        let result = analyze_tree(&modules);

        assert!(
            has_pending_import(&result, "main", "main", "a.sub1"),
            "__all__ should include sub1 from initial assignment"
        );
        assert!(
            has_pending_import(&result, "main", "main", "a.sub2"),
            "__all__ should include sub2 from append()"
        );
    }

    #[test]
    fn test_dunder_all_reassignment_overwrites() {
        // Reassigning __all__ should overwrite the previous value.
        let a = r#"
__all__ = ["sub1"]
__all__ = ["sub2"]
"#;
        let a_sub1 = r#"
x = 1
"#;
        let a_sub2 = r#"
y = 2
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![
            ("a", a),
            ("a.sub1", a_sub1),
            ("a.sub2", a_sub2),
            ("main", main),
        ];
        let result = analyze_tree(&modules);

        assert!(
            !has_pending_import(&result, "main", "main", "a.sub1"),
            "sub1 should be removed by reassignment of __all__"
        );
        assert!(
            has_pending_import(&result, "main", "main", "a.sub2"),
            "sub2 should be present after reassignment of __all__"
        );
    }

    #[test]
    fn test_dunder_all_annotated_assignment() {
        // __all__: list[str] = ["sub"] should be recognized.
        let a = r#"
__all__: list[str] = ["sub"]
"#;
        let a_sub = r#"
x = 1
"#;
        let main = r#"
from a import *
"#;
        let modules = vec![("a", a), ("a.sub", a_sub), ("main", main)];
        let result = analyze_tree(&modules);

        assert!(
            has_pending_import(&result, "main", "main", "a.sub"),
            "Annotated __all__ assignment should be recognized"
        );
    }
}
