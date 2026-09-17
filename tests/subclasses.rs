/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::output::LifeGuardAnalysis;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::test_lib::run_lifeguard_analysis;

    fn loads_imports_eagerly(result: &LifeGuardAnalysis, module: &str) -> bool {
        result
            .output
            .load_imports_eagerly
            .contains(&ModuleName::from_str(module))
    }

    /// `class Sub(Base)` appends to `Base.__subclasses__()` with no user code to
    /// analyze, so a registry built from that walk only sees modules that were
    /// actually executed. The walking module's imports have to be eager.
    #[test]
    fn test_subclasses_walk_forces_eager_imports() {
        let base = "class Base: pass";
        let plugin_a = r#"
            from base import Base
            class A(Base): pass
        "#;
        let plugin_b = r#"
            from base import Base
            class B(Base): pass
        "#;
        let registry = r#"
            import plugin_a  # noqa: F401
            import plugin_b  # noqa: F401
            from base import Base

            def registered():
                return {c.__name__: c for c in Base.__subclasses__()}
        "#;
        let modules = vec![
            ("base", base),
            ("plugin_a", plugin_a),
            ("plugin_b", plugin_b),
            ("registry", registry),
        ];

        let result = run_lifeguard_analysis(&modules);
        assert!(
            loads_imports_eagerly(&result, "registry"),
            "a module walking __subclasses__ must load its imports eagerly"
        );
    }

    /// The real-world shape indirects through a helper, so the receiver is a
    /// parameter and cannot be resolved to a class.
    #[test]
    fn test_subclasses_walk_on_a_parameter_forces_eager_imports() {
        let base = "class Base: pass";
        let plugin_a = r#"
            from base import Base
            class A(Base): pass
        "#;
        let registry = r#"
            import plugin_a  # noqa: F401
            from base import Base

            def _all_subclasses(cls):
                out = set()
                for sub in cls.__subclasses__():
                    out.add(sub)
                    out |= _all_subclasses(sub)
                return out

            def registered():
                return {c.__name__: c for c in _all_subclasses(Base)}
        "#;
        let modules = vec![
            ("base", base),
            ("plugin_a", plugin_a),
            ("registry", registry),
        ];

        let result = run_lifeguard_analysis(&modules);
        assert!(
            loads_imports_eagerly(&result, "registry"),
            "an unresolved receiver must not hide the __subclasses__ walk"
        );
    }

    /// Defining a subclass is not itself a reason to give up laziness -- only
    /// reading the subclass list is.
    #[test]
    fn test_defining_a_subclass_does_not_force_eager_imports() {
        let base = "class Base: pass";
        let plugin_a = r#"
            from base import Base
            class A(Base): pass
        "#;
        let modules = vec![("base", base), ("plugin_a", plugin_a)];

        let result = run_lifeguard_analysis(&modules);
        assert!(
            result.output.load_imports_eagerly.is_empty(),
            "subclassing alone must stay lazy-eligible"
        );
    }
}
