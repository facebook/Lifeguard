/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::errors::ErrorKind;
    use lifeguard::errors::ErrorMetadata;
    use lifeguard::errors::SafetyError;
    use lifeguard::exports::Exports;
    use lifeguard::format::ErrorString;
    use lifeguard::imports::ImportGraph;
    use lifeguard::module_safety::ModuleSafety;
    use lifeguard::module_safety::SafetyResult;
    use lifeguard::output::LifeGuardAnalysis;
    use lifeguard::output::LifeGuardOutput;
    use lifeguard::project;
    use lifeguard::project::SafetyMap;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::pyrefly::sys_info::SysInfo;
    use lifeguard::runner::Options;
    use lifeguard::test_lib::TestSources;
    use lifeguard::test_lib::assert_str_keys;
    use lifeguard::traits::SysInfoExt;
    use ruff_text_size::TextRange;
    use starlark_map::small_set::SmallSet;

    fn test_options() -> Options {
        Options {
            verbose_output_path: None,
            sorted_output: true,
        }
    }

    fn make_module_safety(
        errors: &[ErrorKind],
        excludes: &[ErrorKind],
        implicit_imports: &[ModuleName],
    ) -> ModuleSafety {
        fn make_error(i: usize, e: ErrorKind) -> SafetyError {
            let n = u32::try_from(i).unwrap();
            // We don't care about the text range here
            SafetyError::new(e, e.error_string(), TextRange::new(n.into(), n.into()))
        }
        let mut out = ModuleSafety::new();
        for (i, err) in errors.iter().enumerate() {
            out.add_error(make_error(i, *err));
        }
        for (i, err) in excludes.iter().enumerate() {
            out.add_force_import_override(make_error(i, *err));
        }
        out.add_implicit_imports(&implicit_imports.iter().cloned().collect());
        out
    }

    // input: [(module_name, errors, excludes, implicit_imports)]
    fn make_safety_map(
        modules: Vec<(&str, Vec<ErrorKind>, Vec<ErrorKind>, Vec<ModuleName>)>,
    ) -> SafetyMap {
        let safety_map = SafetyMap::new();
        for (mod_name, errs, excludes, implicit_imports) in &modules {
            let key = ModuleName::from_str(mod_name);
            let val = SafetyResult::Ok(make_module_safety(errs, excludes, implicit_imports));
            safety_map.insert(key, val);
        }
        safety_map
    }

    fn assert_passing(result: &LifeGuardAnalysis, expected: Vec<&str>) {
        assert_str_keys(&result.passing_modules, expected);
    }

    fn assert_failing(result: &LifeGuardAnalysis, expected: Vec<&str>) {
        assert_str_keys(&result.failing_modules, expected);
    }

    fn assert_error_counts(
        result: &LifeGuardAnalysis,
        error_counts: Vec<((ErrorKind, ErrorMetadata), usize)>,
    ) {
        for (err, count) in &error_counts {
            assert_eq!(result.aggregated_errors.get(err).unwrap_or(&0), count);
        }
    }

    fn make_error_key(kind: ErrorKind, metadata: &str) -> (ErrorKind, ErrorMetadata) {
        (kind, metadata.parse().unwrap())
    }

    fn has_lazy_eligible_dep(result: &LifeGuardAnalysis, module: &str, dep: &str) -> bool {
        result
            .output
            .lazy_eligible
            .get(&ModuleName::from_str(module))
            .map(|deps| deps.contains(&ModuleName::from_str(dep)))
            .unwrap_or(false)
    }

    fn run_lifeguard_analysis(modules: &Vec<(&str, &str)>) -> LifeGuardAnalysis {
        let sources = TestSources::new(modules);
        let sys_info = SysInfo::lg_default();
        let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &sys_info);
        let (safety_map, side_effect_imports) =
            project::run_analysis(&sources, &exports, &import_graph, &sys_info);
        let mut analysis =
            LifeGuardAnalysis::new(safety_map, import_graph, &exports, &test_options());
        analysis.propagate_side_effect_imports(&side_effect_imports);
        analysis
    }

    #[test]
    fn test_empty_lifeguard_output() {
        let lifeguard_output = LifeGuardOutput::new(true);
        assert!(lifeguard_output.load_imports_eagerly.is_empty());
        assert!(lifeguard_output.lazy_eligible.is_empty());
    }

    #[test]
    fn test_expected_output_format() {
        let mut lifeguard_output = LifeGuardOutput::new(true);

        let mut unsafe_set = SmallSet::new();
        unsafe_set.insert(ModuleName::from_str("buoy"));
        unsafe_set.insert(ModuleName::from_str("swim.safe"));

        lifeguard_output
            .lazy_eligible
            .insert(ModuleName::from_str("os"), unsafe_set);
        lifeguard_output
            .lazy_eligible
            .insert(ModuleName::from_str("sys"), SmallSet::new());
        lifeguard_output
            .lazy_eligible
            .insert(ModuleName::from_str("_stat.*"), SmallSet::new());
        lifeguard_output
            .lazy_eligible
            .insert(ModuleName::from_str("stat"), SmallSet::new());
        lifeguard_output
            .load_imports_eagerly
            .insert(ModuleName::from_str("os"));
        let expected_output = serde_json::json!({
            "LOAD_IMPORTS_EAGERLY": ["os"],
            "LAZY_ELIGIBLE": {
                "os": ["buoy", "swim.safe"],
                "sys": [],
                "_stat.*": [],
                "stat": []
            }
        });
        let actual_output = serde_json::to_value(&lifeguard_output).unwrap();
        assert_eq!(actual_output, expected_output);
    }

    #[test]
    fn test_lifeguard_analysis_new_empty_input() {
        let safety_map = SafetyMap::new();
        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let result = LifeGuardAnalysis::new(safety_map, import_graph, &exports, &test_options());

        assert!(result.failing_modules.is_empty());
        assert!(result.passing_modules.is_empty());
        assert!(result.aggregated_errors.is_empty());
        assert!(result.output.load_imports_eagerly.is_empty());
        assert!(result.output.lazy_eligible.is_empty());
    }

    #[test]
    fn test_lifeguard_analysis_new_all_passing_modules() {
        let import_graph = ImportGraph::new();
        let safety_map = make_safety_map(vec![
            ("safe_module1", vec![], vec![], vec![]),
            ("safe_module2", vec![], vec![], vec![]),
        ]);
        let exports = Exports::empty();
        let result = LifeGuardAnalysis::new(safety_map, import_graph, &exports, &test_options());
        assert_passing(&result, vec!["safe_module1", "safe_module2"]);
        assert_failing(&result, vec![]);
        assert_eq!(result.output.lazy_eligible.len(), 0);
    }

    #[test]
    fn test_lifeguard_analysis_new_all_failing_modules() {
        let import_graph = ImportGraph::new();
        let safety_map = make_safety_map(vec![
            (
                "unsafe_module1",
                vec![ErrorKind::UnsafeFunctionCall, ErrorKind::UnsafeFunctionCall],
                vec![],
                vec![],
            ),
            (
                "unsafe_module2",
                vec![],
                vec![ErrorKind::CustomFinalizer],
                vec![],
            ),
        ]);
        let exports = Exports::empty();
        let result = LifeGuardAnalysis::new(safety_map, import_graph, &exports, &test_options());
        assert_failing(&result, vec!["unsafe_module1"]);
        assert_passing(&result, vec!["unsafe_module2"]);
        assert_eq!(result.output.lazy_eligible.len(), 0);

        // Check error counts
        assert_error_counts(
            &result,
            vec![
                (
                    make_error_key(ErrorKind::UnsafeFunctionCall, "unsafe-function-call"),
                    1,
                ),
                (
                    make_error_key(ErrorKind::CustomFinalizer, "custom-finalizer"),
                    1,
                ),
            ],
        );
    }

    #[test]
    fn test_lifeguard_analysis_new_mixed_modules() {
        let import_graph = ImportGraph::new();

        let safety_map = make_safety_map(vec![
            (
                "safe_module",
                vec![],
                vec![],
                vec![ModuleName::from_str("implicit_safe")],
            ),
            (
                "unsafe_module",
                vec![
                    ErrorKind::UnsafeFunctionCall,
                    ErrorKind::UnknownDecoratorCall,
                    ErrorKind::UnsafeFunctionCall,
                    ErrorKind::UnhandledException,
                ],
                vec![],
                vec![ModuleName::from_str("implicit_unsafe")],
            ),
        ]);
        let exports = Exports::empty();
        let result = LifeGuardAnalysis::new(safety_map, import_graph, &exports, &test_options());

        // Check module categorization
        assert_eq!(result.passing_modules.len(), 1);
        assert!(
            result
                .passing_modules
                .contains(&ModuleName::from_str("safe_module"))
        );
        assert_eq!(result.failing_modules.len(), 1);
        assert!(
            result
                .failing_modules
                .contains(&ModuleName::from_str("unsafe_module"))
        );
        assert_eq!(result.output.lazy_eligible.len(), 1);
        assert!(has_lazy_eligible_dep(
            &result,
            "safe_module",
            "implicit_safe"
        ));

        // Check error counts
        assert_error_counts(
            &result,
            vec![
                (
                    make_error_key(ErrorKind::UnsafeFunctionCall, "unsafe-function-call"),
                    1,
                ),
                (
                    make_error_key(ErrorKind::UnknownDecoratorCall, "unknown-decorator-call"),
                    1,
                ),
                (
                    make_error_key(ErrorKind::UnhandledException, "unhandled-exception"),
                    1,
                ),
                (
                    make_error_key(ErrorKind::CustomFinalizer, "custom-finalizer"),
                    0,
                ),
            ],
        );
    }

    #[test]
    fn test_lifeguard_analysis_new_error_counting() {
        let import_graph = ImportGraph::new();
        let safety_map = make_safety_map(vec![(
            "error_module",
            vec![
                ErrorKind::UnsafeFunctionCall,
                ErrorKind::UnsafeFunctionCall,
                ErrorKind::UnsafeFunctionCall,
                ErrorKind::ProhibitedCall,
                ErrorKind::ProhibitedCall,
                ErrorKind::UnknownMethodCall,
            ],
            vec![],
            vec![],
        )]);
        let exports = Exports::empty();
        let result = LifeGuardAnalysis::new(safety_map, import_graph, &exports, &test_options());

        // Verify error counts
        assert_error_counts(
            &result,
            vec![
                (
                    make_error_key(ErrorKind::UnsafeFunctionCall, "unsafe-function-call"),
                    1,
                ),
                (
                    make_error_key(ErrorKind::ProhibitedCall, "prohibited-call"),
                    1,
                ),
                (
                    make_error_key(ErrorKind::UnknownMethodCall, "unknown-method-call"),
                    1,
                ),
                (
                    make_error_key(ErrorKind::CustomFinalizer, "custom-finalizer"),
                    0,
                ),
                (make_error_key(ErrorKind::ExecCall, "exec-call"), 0),
            ],
        );
    }

    #[test]
    fn test_lifeguard_safety_map() {
        let code1 = r#"
            l = s
            class C:
                def l(self):
                    l.getattr(1)
                    return l
        "#;
        let code2 = r#"
            from m1 import C
            c = C()
            C[4] = 3 # E: imported-module-assignment
        "#;
        let code3 = r#"
            import m1
            import m2
            def f():
                exec("foo")
        "#;
        let code4 = r#"
            import m2
        "#;
        let code5 = r#"
            import m2
            # reachable exec() marks the module unsafe as well as adding to load_imports_eagerly
            exec("foo")
        "#;
        let modules = vec![
            ("m1", code1),
            ("m2", code2),
            ("m3", code3),
            ("m4", code4),
            ("m5", code5),
        ];

        let result = run_lifeguard_analysis(&modules);

        let expected_output = serde_json::json!({
            "LOAD_IMPORTS_EAGERLY": ["m3", "m5"],
            "LAZY_ELIGIBLE": {
                "m1": [],
                "m3": ["m2"],
                "m4": ["m2"]
            }
        });

        let actual_output = serde_json::to_value(&result.output).unwrap();
        assert_eq!(actual_output, expected_output);
    }

    #[test]
    fn test_safety_map_with_missing_modules() {
        // Same test as above but omitting m2 instead of analyzing it as unsafe
        let code1 = r#"
            l = s
            class C:
                def l(self):
                    l.getattr(1)
                    return l
        "#;
        let code3 = r#"
            import m1
            import m2
            def f():
                exec("foo")
        "#;
        let code4 = r#"
            import m2
        "#;
        let code5 = r#"
            import m2
            # reachable exec() marks the module unsafe as well as adding to load_imports_eagerly
            exec("foo")
        "#;
        let modules = vec![("m1", code1), ("m3", code3), ("m4", code4), ("m5", code5)];

        let result = run_lifeguard_analysis(&modules);

        let expected_output = serde_json::json!({
            "LOAD_IMPORTS_EAGERLY": ["m3", "m5"],
            "LAZY_ELIGIBLE": {
                "m1": [],
                "m3": ["m2"],
                "m4": ["m2"]
            }
        });

        let actual_output = serde_json::to_value(&result.output).unwrap();
        assert_eq!(actual_output, expected_output);
    }

    #[test]
    fn test_safety_map_with_all_missing_modules() {
        let code1 = r#"
            import a, b
            import c

            from x import y
            from z import *
        "#;
        let modules = vec![("m1", code1)];

        let result = run_lifeguard_analysis(&modules);

        let expected_output = serde_json::json!({
            "LOAD_IMPORTS_EAGERLY": [],
            "LAZY_ELIGIBLE": {
                "m1": ["a", "b", "c", "x", "x.y", "z"],
            }
        });

        let actual_output = serde_json::to_value(&result.output).unwrap();
        assert_eq!(actual_output, expected_output);
    }

    #[test]
    fn test_safety_map_with_aliased_imports() {
        let code1 = r#"
            import m2
            import m2 as moo
            import m2.sub as m3
            from m2 import X as xx

            m2.f()
            val1 = moo.f()
            val2 = xx()
            val3 = m3.m3()
        "#;
        let code2 = r#"
            def f():
                return "hello"
            def X():
                return "hi"
        "#;
        let code3 = r#"
            def m3():
                return "hi :)"
        "#;
        let modules = vec![("m1", code1), ("m2", code2), ("m2.sub", code3)];

        let result = run_lifeguard_analysis(&modules);

        let expected_output = serde_json::json!({
            "LOAD_IMPORTS_EAGERLY": [],
            "LAZY_ELIGIBLE": {
                "m1": [],
                "m2": [],
                "m2.sub": [],
            }
        });

        let actual_output = serde_json::to_value(&result.output).unwrap();
        assert_eq!(actual_output, expected_output);
    }

    #[test]
    fn test_safety_map_with_aliased_imports_missing_import() {
        let code1 = r#"
            import m2
            import m2 as m3
            from m2 import X as xx

            m2.f()
            val1 = m3.f()
            val2 = xx()
        "#;
        let modules = vec![("m1", code1)];
        let result = run_lifeguard_analysis(&modules);

        let expected_output = serde_json::json!({
            "LOAD_IMPORTS_EAGERLY": [],
            "LAZY_ELIGIBLE": {
            }
        });

        let actual_output = serde_json::to_value(&result.output).unwrap();
        assert_eq!(actual_output, expected_output);
    }

    #[test]
    fn test_re_export_lazy_eligible_dependency() {
        // Tests that lazy_eligible dependencies propagate through longer chains: A -> B -> C -> D (unsafe)

        let module_d = r#"
            from os import path
            def foo():
                path[0] = 1 # E: imported-module-assignment
            def bar():
                pass
            foo()
        "#;

        let module_c = r#"
            import module_d
            module_d.bar()
        "#;

        let module_b = r#"
            from module_d import foo
        "#;

        let module_a = r#"
            from module_b import foo
        "#;

        let modules = vec![
            ("module_a", module_a),
            ("module_b", module_b),
            ("module_c", module_c),
            ("module_d", module_d),
        ];

        let result = run_lifeguard_analysis(&modules);

        // module_d should be in failing_modules
        assert!(
            result
                .failing_modules
                .contains(&ModuleName::from_str("module_d")),
            "module_d should be in failing_modules"
        );

        // All other modules should be passing
        assert!(
            result
                .passing_modules
                .contains(&ModuleName::from_str("module_a"))
        );
        assert!(
            result
                .passing_modules
                .contains(&ModuleName::from_str("module_b"))
        );
        assert!(
            result
                .passing_modules
                .contains(&ModuleName::from_str("module_c"))
        );

        // All passing modules should have module_d in their lazy_eligible lists (transitively)
        for module_name in ["module_a", "module_b", "module_c"] {
            assert!(
                has_lazy_eligible_dep(&result, module_name, "module_d"),
                "{} should have module_d in its lazy_eligible list",
                module_name
            );
        }
    }

    #[test]
    fn test_submodule_re_export_not_in_lazy_eligible_set() {
        // When a package re-exports from its own submodule via __all__, the
        // submodule must NOT be in the LAZY_ELIGIBLE dict.
        let pkg_init = r#"
            from pkg.sub import val
            __all__ = ["val"]
        "#;
        let pkg_sub = r#"
            val = 42
        "#;
        let consumer = r#"
            from pkg import val
            x = val
        "#;
        let modules = vec![
            ("pkg", pkg_init),
            ("pkg.sub", pkg_sub),
            ("consumer", consumer),
        ];
        let result = run_lifeguard_analysis(&modules);
        assert!(
            !result
                .output
                .lazy_eligible
                .contains_key(&ModuleName::from_str("pkg.sub")),
            "pkg.sub should NOT be in the LAZY_ELIGIBLE dict (re-exported submodule)"
        );
        assert!(
            result
                .failing_modules
                .contains(&ModuleName::from_str("pkg.sub")),
            "pkg.sub should be in failing_modules"
        );

        assert!(
            result
                .output
                .lazy_eligible
                .contains_key(&ModuleName::from_str("pkg")),
            "pkg should be in the LAZY_ELIGIBLE dict"
        );
        assert!(
            result
                .output
                .lazy_eligible
                .contains_key(&ModuleName::from_str("consumer")),
            "consumer should be in the LAZY_ELIGIBLE dict"
        );
    }

    #[test]
    fn test_cycle_deps_propagate_to_subpackages() {
        // Test that cycle dependencies propagate to child modules.

        // cycle_a and cycle_b form a cycle
        let cycle_a = r#"
            import cycle_b
            def func_a():
                pass
        "#;

        let cycle_b = r#"
            import cycle_a
            def func_b():
                pass
        "#;

        // cycle_a.sub is a subpackage of cycle_a
        let cycle_a_sub = r#"
            def sub_func():
                pass
        "#;

        let modules = vec![
            ("cycle_a", cycle_a),
            ("cycle_b", cycle_b),
            ("cycle_a.sub", cycle_a_sub),
        ];

        let result = run_lifeguard_analysis(&modules);

        // All modules should be passing (no errors)
        assert_passing(&result, vec!["cycle_a", "cycle_b", "cycle_a.sub"]);
        assert_failing(&result, vec![]);

        // cycle_a should have cycle_b in its lazy_eligible list (direct cycle dep)
        assert!(
            has_lazy_eligible_dep(&result, "cycle_a", "cycle_b"),
            "cycle_a should have cycle_b in its lazy_eligible list"
        );

        // cycle_b should have cycle_a in its lazy_eligible list (direct cycle dep)
        assert!(
            has_lazy_eligible_dep(&result, "cycle_b", "cycle_a"),
            "cycle_b should have cycle_a in its lazy_eligible list"
        );

        // cycle_a.sub should ALSO have cycle_b in its lazy_eligible list (propagated from parent)
        assert!(
            has_lazy_eligible_dep(&result, "cycle_a.sub", "cycle_b"),
            "cycle_a.sub should have cycle_b in its lazy_eligible list (propagated from parent cycle_a)"
        );
    }

    #[test]
    fn test_side_effect_imports_propagate_failing_deps() {
        // When module A does a bare `import B` that is never accessed (side-effect import),
        // and B is a passing module with non-empty failing deps, B should appear in A's
        // failing deps so B is eagerly imported.

        let registry = r#"
            REGISTRY = {}
            def register(name):
                def decorator(cls):
                    REGISTRY[name] = cls
                    return cls
                return decorator
        "#;

        let layer_film = r#"
            from registry import register
            @register("FiLMLayer")  # E: unknown-decorator-call
            class FiLMLayer:
                pass
        "#;

        // gen_layers: bare import of layer_film (side-effect import, never accessed)
        let gen_layers = r#"
            import layer_film
        "#;

        // test_module: bare import of gen_layers (side-effect import) + uses registry
        let test_module = r#"
            import gen_layers
            from registry import REGISTRY
            x = REGISTRY
        "#;

        let modules = vec![
            ("registry", registry),
            ("layer_film", layer_film),
            ("gen_layers", gen_layers),
            ("test_module", test_module),
        ];

        let result = run_lifeguard_analysis(&modules);

        assert!(
            result
                .failing_modules
                .contains(&ModuleName::from_str("layer_film")),
            "layer_film should be failing"
        );

        assert!(
            result
                .passing_modules
                .contains(&ModuleName::from_str("gen_layers")),
            "gen_layers should be passing"
        );
        assert!(
            has_lazy_eligible_dep(&result, "gen_layers", "layer_film"),
            "gen_layers should have layer_film in its lazy_eligible list"
        );

        assert!(
            result
                .passing_modules
                .contains(&ModuleName::from_str("test_module")),
            "test_module should be passing"
        );
        assert!(
            has_lazy_eligible_dep(&result, "test_module", "gen_layers"),
            "test_module should have gen_layers in its lazy_eligible list (side-effect import)"
        );
    }

    #[test]
    fn test_cycle_deps_do_not_propagate_to_failing_children() {
        // When a cycle module has a child that is failing (e.g. due to SubmoduleReExport),
        // the child must NOT be added to lazy_eligible during cycle propagation.
        // This mirrors the cryptography.hazmat.backends.openssl.backend production bug.

        // cycle_a is a package (__init__.py) in a cycle with cycle_b.
        // cycle_a re-exports "val" from its submodule cycle_a.sub via __all__.
        let cycle_a = r#"
            import cycle_b
            from cycle_a.sub import val
            __all__ = ["val"]
        "#;

        let cycle_b = r#"
            import cycle_a
            def func_b():
                pass
        "#;

        let cycle_a_sub = r#"
            val = 42
        "#;

        let modules = vec![
            ("cycle_a", cycle_a),
            ("cycle_b", cycle_b),
            ("cycle_a.sub", cycle_a_sub),
        ];

        let result = run_lifeguard_analysis(&modules);

        // cycle_a.sub should be FAILING (SubmoduleReExport) and NOT in lazy_eligible
        assert!(
            result
                .failing_modules
                .contains(&ModuleName::from_str("cycle_a.sub")),
            "cycle_a.sub should be failing due to SubmoduleReExport"
        );
        assert!(
            !result
                .output
                .lazy_eligible
                .contains_key(&ModuleName::from_str("cycle_a.sub")),
            "cycle_a.sub should NOT be in lazy_eligible (failing module, even with cycle propagation)"
        );
    }
}
