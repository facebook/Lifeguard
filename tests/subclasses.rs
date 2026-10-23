/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::cache::CachedSafety;
    use lifeguard::cache::LibraryCache;
    use lifeguard::cache::ReduceWorkspace;
    use lifeguard::errors::ErrorKind;
    use lifeguard::output::LifeGuardAnalysis;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::runner::Options;
    use lifeguard::runner::default_python_version;
    use lifeguard::test_lib::CacheDelivery;
    use lifeguard::test_lib::TestSources;
    use lifeguard::test_lib::assert_passing;
    use lifeguard::test_lib::assert_paths_agree_sharded;
    use lifeguard::test_lib::assert_paths_agree_sharded_with_options;
    use lifeguard::test_lib::has_lazy_eligible_dep;
    use lifeguard::test_lib::run_analysis_on;
    use lifeguard::test_lib::run_incremental_analysis_on_groups;
    use lifeguard::test_lib::run_lifeguard_analysis;
    use lifeguard::test_lib::test_options;
    use lifeguard::test_lib::verbose_test_options;

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

    const REGISTRY_BASE: &str = r#"
        class Base: pass
        class Mid(Base): pass

        class Unwalked: pass

        def registered():
            return {c.__name__: c for c in Base.__subclasses__()}
    "#;
    const PLUGINS: &str = r#"
        from plugins import plugin_a  # noqa: F401
        from plugins import plugin_b  # noqa: F401
    "#;
    const PLUGIN_A: &str = r#"
        from base import Mid
        class A(Mid): pass
    "#;
    const PLUGIN_B: &str = r#"
        from base import Unwalked
        class B(Unwalked): pass
    "#;
    const MAIN: &str = r#"
        import plugins  # noqa: F401
        from base import registered

        def main():
            return registered()
    "#;

    fn registry_modules() -> Vec<(&'static str, &'static str)> {
        vec![
            ("base", REGISTRY_BASE),
            ("plugins", PLUGINS),
            ("plugins.plugin_a", PLUGIN_A),
            ("plugins.plugin_b", PLUGIN_B),
            ("main", MAIN),
        ]
    }

    fn assert_registry_loads_plugins(result: &LifeGuardAnalysis) {
        assert!(
            has_lazy_eligible_dep(result, "main", "plugins"),
            "the side-effect import feeding the registry must run eagerly"
        );
        assert!(
            has_lazy_eligible_dep(result, "plugins", "plugins.plugin_a"),
            "a subclass of a walked base must be loaded by its importer"
        );
        assert!(
            !has_lazy_eligible_dep(result, "plugins", "plugins.plugin_b"),
            "a subclass of a base nobody walks stays lazy"
        );
    }

    /// The registry's base, its plugins and the side-effect import that loads them
    /// live in three different modules, so the walking module imports no plugin.
    #[test]
    fn test_subclass_of_a_walked_base_is_loaded_by_its_importers() {
        assert_registry_loads_plugins(&run_lifeguard_analysis(&registry_modules()));
    }

    #[test]
    fn test_subclass_of_a_walked_base_in_another_library_is_loaded_by_its_importers() {
        let modules = registry_modules();
        let groups = vec![
            vec![modules[0]],
            vec![modules[1], modules[2], modules[3]],
            vec![modules[4]],
        ];
        let run =
            run_incremental_analysis_on_groups(&groups, CacheDelivery::Serialized, &test_options());
        assert_registry_loads_plugins(run.analysis());
    }

    /// The walking module has run before any walk, so subclasses it defines itself
    /// are always registered.
    #[test]
    fn test_subclass_defined_beside_its_walk_stays_safe() {
        let backends = r#"
            class Base: pass
            class Torch(Base): pass

            def get_backend():
                return Base.__subclasses__()
        "#;
        let modules = vec![("backends", backends), ("user", "import backends")];
        assert_passing(&run_lifeguard_analysis(&modules), vec!["backends", "user"]);
        assert_paths_agree_sharded(&modules);
    }

    /// A walker's imports already load eagerly, so a module it imports is registered
    /// before its walk runs.
    #[test]
    fn test_subclass_imported_by_its_walker_stays_safe() {
        let backends = r#"
            class Base: pass
            class Torch(Base): pass
        "#;
        let walker = r#"
            from backends import Base

            def all_backends():
                return Base.__subclasses__()
        "#;
        let modules = vec![("backends", backends), ("walker", walker)];
        assert_passing(
            &run_lifeguard_analysis(&modules),
            vec!["backends", "walker"],
        );
        assert_paths_agree_sharded(&modules);
    }

    /// `compare-paths` reduces a whole-program cache again; that must not add the errors twice.
    #[test]
    fn test_reducing_a_whole_program_cache_keeps_one_registration_per_class() {
        let (output, import_graph, exports) =
            run_analysis_on(&TestSources::new(&registry_modules()));
        let mut cache = LibraryCache::build(
            &output.safety_map,
            &import_graph,
            &exports,
            &output.side_effect_imports,
        );
        cache.set_class_facts(output.class_bases, output.class_properties);
        let resolved = ReduceWorkspace::single(cache, default_python_version()).resolve(None);
        let registrations: Vec<&str> = resolved
            .modules()
            .iter()
            .filter_map(|module| match &module.safety {
                CachedSafety::Ok(safety) => Some(&safety.errors),
                CachedSafety::AnalysisError { .. } => None,
            })
            .flatten()
            .filter(|error| error.kind == ErrorKind::SubclassRegistration)
            .map(|error| error.metadata.as_str())
            .collect();
        assert_eq!(registrations, vec!["plugins.plugin_a.A"]);
    }

    /// A `__main__` guard only runs in the entry module, so a subclass defined in another
    /// module's guard is never created.
    #[test]
    fn test_subclass_in_a_non_entry_main_guard_stays_safe() {
        let script = r#"
            from base import Base

            if __name__ == "__main__":
                class Cli(Base): pass
        "#;
        let modules = vec![
            ("base", REGISTRY_BASE),
            ("script", script),
            ("entry", "import script"),
        ];
        let options = Options {
            main_module: Some(ModuleName::from_str("entry")),
            ..verbose_test_options()
        };
        assert_paths_agree_sharded_with_options(&modules, &options);
    }
}
