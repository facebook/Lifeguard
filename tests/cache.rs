/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use lifeguard::cache::CachedError;
    use lifeguard::cache::CachedExports;
    use lifeguard::cache::CachedModule;
    use lifeguard::cache::CachedModuleSafety;
    use lifeguard::cache::CachedReExport;
    use lifeguard::cache::CachedSafety;
    use lifeguard::cache::ConstructorCallees;
    use lifeguard::cache::LibraryCache;
    use lifeguard::cache::MergedClassFacts;
    use lifeguard::cache::dedupe_implicit_imports;
    use lifeguard::cache::is_call_verified_safe;
    use lifeguard::config::AnalysisConfig;
    use lifeguard::effects::ImportedArgs;
    use lifeguard::errors::ErrorKind;
    use lifeguard::errors::SafetyError;
    use lifeguard::exports::Exports;
    use lifeguard::hasher::AHashMap;
    use lifeguard::hasher::HashMapExt;
    use lifeguard::imports::ImportGraph;
    use lifeguard::imports::resolve_to_known_module;
    use lifeguard::module_safety::FunctionSafety;
    use lifeguard::module_safety::FunctionSafetyInfo;
    use lifeguard::module_safety::ModuleSafety;
    use lifeguard::module_safety::MutatedParam;
    use lifeguard::module_safety::MutationCandidate;
    use lifeguard::module_safety::MutationCandidateSite;
    use lifeguard::module_safety::ParamPosition;
    use lifeguard::module_safety::SafetyResult;
    use lifeguard::output::LifeGuardAnalysis;
    use lifeguard::project;
    use lifeguard::project::SafetyMap;
    use lifeguard::project::SideEffectMap;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::runner::Options;
    use lifeguard::runner::default_python_version;
    use lifeguard::test_lib::TestSources;
    use lifeguard::test_lib::reduce_workspace_from_merged;

    /// A record whose only callee is the class's own `__init__`.
    fn own_init() -> ConstructorCallees {
        ConstructorCallees {
            mask: lifeguard::cache::own_constructor_bit("__init__"),
            ..Default::default()
        }
    }

    /// A record whose only callee is a constructor the mask cannot name, i.e. one
    /// owned by neither the class nor its metaclass.
    fn inherited(callee: ModuleName) -> ConstructorCallees {
        ConstructorCallees {
            extra: vec![callee],
            ..Default::default()
        }
    }

    fn mn(s: &str) -> ModuleName {
        ModuleName::from_str(s)
    }

    /// A `(name, Safe)` `function_safety` entry.
    fn safe(name: &str) -> (String, FunctionSafetyInfo) {
        (
            name.to_owned(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        )
    }

    /// A `(name, UnsafeIfImported)` `function_safety` entry.
    fn unsafe_if_imported(name: &str) -> (String, FunctionSafetyInfo) {
        (
            name.to_owned(),
            FunctionSafetyInfo::new(FunctionSafety::UnsafeIfImported),
        )
    }

    /// A `(name, Unsafe)` `function_safety` entry.
    fn unsafe_(name: &str) -> (String, FunctionSafetyInfo) {
        (
            name.to_owned(),
            FunctionSafetyInfo::new(FunctionSafety::Unsafe),
        )
    }

    /// A `(name, UnsafeMissingDep)` `function_safety` entry blocked on `callee`.
    fn unsafe_missing_dep(name: &str, callee: &str) -> (String, FunctionSafetyInfo) {
        (
            name.to_owned(),
            FunctionSafetyInfo::unsafe_missing_dep(mn(callee)),
        )
    }

    /// Build a `function_safety` map from entries (`AHashMap` has no `From<[_; N]>`).
    fn fsmap<const N: usize>(
        entries: [(String, FunctionSafetyInfo); N],
    ) -> AHashMap<String, FunctionSafetyInfo> {
        entries.into_iter().collect()
    }

    fn empty_exports() -> CachedExports {
        CachedExports {
            re_exports: Vec::new(),
        }
    }

    fn cached_error(kind: ErrorKind, metadata: &str) -> CachedError {
        CachedError {
            kind,
            metadata: metadata.to_owned(),
            parameterized_decorator: false,
        }
    }

    fn parameterized_decorator_error(metadata: &str) -> CachedError {
        CachedError {
            parameterized_decorator: true,
            ..cached_error(ErrorKind::UnsafeDecoratorCall, metadata)
        }
    }

    struct CachedModuleBuilder(CachedModule);

    impl CachedModuleBuilder {
        fn errors(mut self, errors: Vec<CachedError>) -> Self {
            let CachedSafety::Ok(safety) = &mut self.0.safety else {
                unreachable!("test builder always creates cached safety")
            };
            safety.errors = errors;
            self
        }

        fn imports(mut self, imports: &[&str]) -> Self {
            self.0.imports = imports.iter().map(|name| mn(name)).collect();
            self
        }

        fn function_safety<const N: usize>(
            mut self,
            entries: [(String, FunctionSafetyInfo); N],
        ) -> Self {
            self.0.function_safety = fsmap(entries);
            self
        }

        fn build(self) -> CachedModule {
            self.0
        }
    }

    fn cached_module(name: &str) -> CachedModuleBuilder {
        CachedModuleBuilder(CachedModule {
            name: mn(name),
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: Default::default(),
            missing_imports: Default::default(),
            ambiguous_imports: Default::default(),
            side_effect_imports: Default::default(),
            function_safety: AHashMap::new(),
            mutation_candidates: Vec::new(),
        })
    }

    fn build_cache(sources: &TestSources) -> LibraryCache {
        let config = AnalysisConfig::default();
        let (import_graph, exports, in_scope) = ImportGraph::make_with_exports(sources, &config);
        let output = project::run_analysis(
            sources,
            &exports,
            &import_graph,
            &config,
            project::ExecutionMode::Incremental,
            &in_scope,
        );
        let mut cache = LibraryCache::build(
            &output.safety_map,
            &import_graph,
            &exports,
            &output.side_effect_imports,
        );
        // Mirror what `analyze-library` attaches, so these tests exercise the
        // recorded class facts rather than an empty cache.
        cache.set_class_bases(output.class_bases);
        cache.set_constructor_callees(output.constructor_callees);
        cache
    }

    fn resolved_cache(own: &[(&str, &str)], dependencies: &[(&str, &str)]) -> LibraryCache {
        let dep_cache = build_cache(&TestSources::new(dependencies));
        merge_and_resolve(build_cache(&TestSources::new(own)), dep_cache)
    }

    fn merge_and_resolve(mut cache: LibraryCache, dep_cache: LibraryCache) -> LibraryCache {
        let merged_facts = cache.merge_dep_caches(vec![dep_cache]);
        cache.resolve_cross_library_errors(merged_facts);
        cache
    }

    fn module<'a>(cache: &'a LibraryCache, name: &str) -> &'a CachedModule {
        cache
            .modules
            .iter()
            .find(|module| module.name == mn(name))
            .unwrap_or_else(|| panic!("cache should contain module {name}"))
    }

    fn function_verdict(
        cache: &LibraryCache,
        module_name: &str,
        function_name: &str,
    ) -> FunctionSafety {
        module(cache, module_name)
            .function_safety
            .get(function_name)
            .unwrap_or_else(|| {
                panic!("{module_name} should contain function safety for {function_name}")
            })
            .verdict
    }

    fn safe_cached_module(name: &str, imports: &[&str], implicit: &[&str]) -> CachedModule {
        CachedModule {
            name: mn(name),
            safety: CachedSafety::Ok(CachedModuleSafety {
                implicit_imports: implicit.iter().map(|s| mn(s)).collect(),
                ..Default::default()
            }),
            imports: imports.iter().map(|s| mn(s)).collect(),
            missing_imports: Default::default(),
            ambiguous_imports: Default::default(),
            side_effect_imports: Default::default(),
            function_safety: AHashMap::new(),
            mutation_candidates: Vec::new(),
        }
    }

    fn temp_cache_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lifeguard_{prefix}_{}_{}.postcard",
            std::process::id(),
            nanos
        ))
    }

    fn round_trip(cache: &LibraryCache) -> LibraryCache {
        let path = temp_cache_path("cache");
        cache
            .write_to_file(&path)
            .expect("cache write_to_file should succeed");
        let loaded =
            LibraryCache::read_from_file(&path).expect("cache read_from_file should succeed");
        std::fs::remove_file(&path).expect("temporary cache file should be removable");
        loaded
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_cached_struct_sizes() {
        // Wire fields only: the reduce-side accumulators live on `ReduceWorkspace`.
        assert_eq!(std::mem::size_of::<LibraryCache>(), 96);
        assert_eq!(
            std::mem::size_of::<lifeguard::cache::ConstructorCallees>(),
            40,
        );
        assert_eq!(std::mem::size_of::<CachedModule>(), 264);
        assert_eq!(std::mem::size_of::<CachedSafety>(), 72);
        assert_eq!(std::mem::size_of::<CachedModuleSafety>(), 72);
        assert_eq!(std::mem::size_of::<lifeguard::cache::CachedError>(), 32);
        assert_eq!(std::mem::size_of::<CachedExports>(), 24);
        assert_eq!(std::mem::size_of::<CachedReExport>(), 64);
    }

    #[test]
    fn test_cache_round_trip() {
        let safety_map = SafetyMap::new();

        safety_map.insert(mn("foo"), SafetyResult::Ok(ModuleSafety::new()));

        let mut unsafe_safety = ModuleSafety::new();
        unsafe_safety.add_error(SafetyError::new(
            ErrorKind::UnsafeFunctionCall,
            "bad_func()".to_string(),
            Default::default(),
        ));
        safety_map.insert(mn("bar"), SafetyResult::Ok(unsafe_safety));

        let mut import_graph = ImportGraph::new();
        import_graph.graph.add_node(&mn("foo"));
        import_graph.graph.add_node(&mn("bar"));
        import_graph.graph.add_edge(&mn("foo"), &mn("bar"));

        let exports = Exports::empty();
        let side_effect_imports = SideEffectMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let loaded = round_trip(&cache);

        assert_eq!(loaded.modules.len(), 2);

        let foo = module(&loaded, "foo");
        assert!(matches!(&foo.safety, CachedSafety::Ok(s) if s.is_safe()));
        assert!(foo.imports.contains(&mn("bar")));

        let bar = module(&loaded, "bar");
        match &bar.safety {
            CachedSafety::Ok(s) => {
                assert_eq!(s.errors.len(), 1);
                assert_eq!(s.errors[0].kind, ErrorKind::UnsafeFunctionCall);
                assert_eq!(s.errors[0].metadata, "bad_func()");
            }
            _ => panic!("Expected Ok safety"),
        }
    }

    #[test]
    fn test_cache_analysis_error() {
        let safety_map = SafetyMap::new();
        safety_map.insert(
            mn("broken"),
            SafetyResult::AnalysisError(std::io::Error::other("parse failed").into()),
        );

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let side_effect_imports = SideEffectMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let loaded = round_trip(&cache);

        let broken = module(&loaded, "broken");
        assert!(
            matches!(&broken.safety, CachedSafety::AnalysisError { message } if message == "parse failed")
        );
    }

    #[test]
    fn test_cache_serialize_deserialize_bytes() {
        let safety_map = SafetyMap::new();
        safety_map.insert(mn("test"), SafetyResult::Ok(ModuleSafety::new()));

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let side_effect_imports = SideEffectMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let loaded = round_trip(&cache);

        assert_eq!(loaded.modules.len(), 1);
        assert_eq!(loaded.modules[0].name, mn("test"));
    }

    #[test]
    fn test_cache_round_trip_preserves_re_exports() {
        let cache = LibraryCache {
            modules: vec![safe_cached_module("package", &[], &[])],
            exports: CachedExports {
                re_exports: vec![CachedReExport {
                    exported_module: mn("package"),
                    exported_attr: "public_name".to_owned(),
                    imported_module: mn("implementation"),
                    imported_attr: "private_name".to_owned(),
                }],
            },
            class_bases: vec![(mn("package.Derived"), vec![mn("package.Base")])],
            constructor_callees: vec![(
                mn("package.Derived"),
                inherited(mn("package.Base.__init__")),
            )],
            ..Default::default()
        };

        let loaded = round_trip(&cache);
        assert_eq!(loaded.exports.re_exports.len(), 1);
        let re_export = &loaded.exports.re_exports[0];
        assert_eq!(re_export.exported_module, mn("package"));
        assert_eq!(re_export.exported_attr, "public_name");
        assert_eq!(re_export.imported_module, mn("implementation"));
        assert_eq!(re_export.imported_attr, "private_name");
        assert_eq!(
            loaded.constructor_callees,
            vec![(
                mn("package.Derived"),
                inherited(mn("package.Base.__init__"))
            )],
            "constructor callees must survive the wire round trip",
        );
        assert_eq!(
            loaded.class_bases,
            vec![(mn("package.Derived"), vec![mn("package.Base")])],
        );
    }

    #[test]
    fn test_cache_round_trip_function_safety_and_mutations() {
        let mut info = FunctionSafetyInfo::new(FunctionSafety::UnsafeMissingDep);
        info.missing_dep_callees = [mn("dep.callee")].into_iter().collect();
        info.mutated_params = vec![MutatedParam {
            name: mn("pkg.param"),
            position: ParamPosition::Positional(2),
        }];
        let mut function_safety = AHashMap::new();
        function_safety.insert("helper".to_owned(), info.clone());

        let candidate = MutationCandidate {
            callee: mn("dep.configure"),
            site: MutationCandidateSite::ModuleScope {
                call: mn("pkg.call"),
            },
            arg_offset: 3,
            imported_args: ImportedArgs {
                unsafe_arg_indices: 0b101,
                unsafe_keyword_names: vec![mn("pkg.kw")],
                has_unsafe_kwargs_expansion: true,
                unsafe_args_expansion_min: Some(4),
            },
        };

        let cache = LibraryCache {
            modules: vec![CachedModule {
                name: mn("m"),
                safety: CachedSafety::Ok(CachedModuleSafety::default()),
                imports: Default::default(),
                missing_imports: Default::default(),
                ambiguous_imports: Default::default(),
                side_effect_imports: Default::default(),
                function_safety,
                mutation_candidates: vec![candidate.clone()],
            }],
            exports: CachedExports {
                re_exports: Vec::new(),
            },
            ..Default::default()
        };

        let loaded = round_trip(&cache);
        let module = &loaded.modules[0];
        assert_eq!(
            module.function_safety.get("helper"),
            Some(&info),
            "function safety (verdict, missing_dep_callees, mutated_params) should round-trip",
        );
        assert_eq!(
            module.mutation_candidates,
            vec![candidate],
            "mutation candidates (incl. imported_args details) should round-trip",
        );
    }

    #[test]
    fn test_cache_read_rejects_corrupt_bytes() {
        let cache = LibraryCache {
            modules: vec![safe_cached_module("m", &["dep"], &[])],
            exports: CachedExports {
                re_exports: Vec::new(),
            },
            ..Default::default()
        };
        let path = temp_cache_path("corrupt");
        cache
            .write_to_file(&path)
            .expect("cache write should succeed");
        let bytes = std::fs::read(&path).expect("reading cache bytes should succeed");

        // A wrong-format file (bad magic header) is rejected with a clear error.
        let mut wrong_magic = bytes.clone();
        wrong_magic[0] ^= 0xFF;
        std::fs::write(&path, &wrong_magic).expect("writing should succeed");
        assert!(
            LibraryCache::read_from_file(&path).is_err(),
            "a file with the wrong magic header should fail to read"
        );

        // A truncated cache is rejected rather than silently decoded or panicking.
        std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncating should succeed");
        assert!(
            LibraryCache::read_from_file(&path).is_err(),
            "a truncated cache should fail to read"
        );

        // Trailing bytes past the last module are rejected.
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(b"extra");
        std::fs::write(&path, &trailing).expect("appending should succeed");
        assert!(
            LibraryCache::read_from_file(&path).is_err(),
            "a cache with trailing data should fail to read"
        );

        std::fs::remove_file(&path).expect("temporary cache file should be removable");
    }

    #[test]
    fn test_cache_from_pipeline() {
        let sources = TestSources::new(&[
            ("foo", "import bar\nx = bar.func()\n"),
            ("bar", "def func(): return 1\n"),
        ]);
        let cache = build_cache(&sources);

        assert_eq!(cache.modules.len(), 2);

        for module in &cache.modules {
            assert!(
                matches!(&module.safety, CachedSafety::Ok(s) if s.is_safe()),
                "Module {} should be safe",
                module.name.as_str()
            );
        }

        let foo = module(&cache, "foo");
        assert!(foo.imports.contains(&mn("bar")));

        let loaded = round_trip(&cache);
        assert_eq!(loaded.modules.len(), 2);
    }

    #[test]
    fn test_constructor_call_caches_class_level_safety() {
        let cache = build_cache(&TestSources::new(&[
            (
                "defs",
                "from dataclasses import dataclass\n\
                 @dataclass\n\
                 class Safe:\n\
                 \x20   value: int = 0\n",
            ),
            ("caller", "from defs import Safe\nobj = Safe()\n"),
        ]));

        let defs_mod = module(&cache, "defs");
        assert!(
            defs_mod.function_safety.contains_key("Safe"),
            "function_safety should contain class-level entry 'Safe', got keys: {:?}",
            defs_mod.function_safety.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            defs_mod
                .function_safety
                .get("Safe")
                .map(|info| info.verdict),
            Some(FunctionSafety::Safe),
        );
    }

    #[test]
    fn test_cache_with_load_imports_eagerly() {
        let safety_map = SafetyMap::new();
        let mut safety = ModuleSafety::new();
        safety.add_force_import_override(SafetyError::new(
            ErrorKind::ExecCall,
            "exec()".to_string(),
            Default::default(),
        ));
        safety_map.insert(mn("exec_mod"), SafetyResult::Ok(safety));

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let side_effect_imports = SideEffectMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let loaded = round_trip(&cache);

        let exec_mod = module(&loaded, "exec_mod");
        match &exec_mod.safety {
            CachedSafety::Ok(s) => {
                assert!(s.is_safe());
                assert!(s.should_load_imports_eagerly());
                assert_eq!(s.force_imports_eager_overrides.len(), 1);
                assert_eq!(s.force_imports_eager_overrides[0].kind, ErrorKind::ExecCall);
            }
            _ => panic!("Expected Ok safety"),
        }
    }

    #[test]
    fn test_cache_side_effect_imports() {
        let safety_map = SafetyMap::new();
        safety_map.insert(mn("a"), SafetyResult::Ok(ModuleSafety::new()));

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let mut side_effect_imports = SideEffectMap::new();
        side_effect_imports.insert(mn("a"), [mn("unused_dep")].into_iter().collect());

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);
        let loaded = round_trip(&cache);

        let a = module(&loaded, "a");
        assert!(a.side_effect_imports.contains(&mn("unused_dep")));
    }

    #[test]
    fn test_cache_sorted_output() {
        let safety_map = SafetyMap::new();
        safety_map.insert(mn("z_mod"), SafetyResult::Ok(ModuleSafety::new()));
        safety_map.insert(mn("a_mod"), SafetyResult::Ok(ModuleSafety::new()));
        safety_map.insert(mn("m_mod"), SafetyResult::Ok(ModuleSafety::new()));

        let import_graph = ImportGraph::new();
        let exports = Exports::empty();
        let side_effect_imports = SideEffectMap::new();

        let cache = LibraryCache::build(&safety_map, &import_graph, &exports, &side_effect_imports);

        let names: Vec<&str> = cache.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a_mod", "m_mod", "z_mod"]);
    }

    #[test]
    fn test_merge_dep_caches() {
        let safety_map = SafetyMap::new();
        safety_map.insert(mn("own"), SafetyResult::Ok(ModuleSafety::new()));

        let mut cache = LibraryCache::build(
            &safety_map,
            &ImportGraph::new(),
            &Exports::empty(),
            &SideEffectMap::new(),
        );
        assert_eq!(cache.modules.len(), 1);

        let dep_safety_map = SafetyMap::new();
        dep_safety_map.insert(mn("dep_a"), SafetyResult::Ok(ModuleSafety::new()));
        let mut unsafe_safety = ModuleSafety::new();
        unsafe_safety.add_error(SafetyError::new(
            ErrorKind::UnsafeFunctionCall,
            "bad()".to_string(),
            Default::default(),
        ));
        dep_safety_map.insert(mn("dep_b"), SafetyResult::Ok(unsafe_safety));

        let dep_cache = LibraryCache::build(
            &dep_safety_map,
            &ImportGraph::new(),
            &Exports::empty(),
            &SideEffectMap::new(),
        );

        cache.merge_dep_caches(vec![dep_cache]);

        assert_eq!(cache.modules.len(), 3);
        let names: Vec<&str> = cache.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["dep_a", "dep_b", "own"]);

        let dep_b = module(&cache, "dep_b");
        match &dep_b.safety {
            CachedSafety::Ok(s) => {
                assert_eq!(s.errors.len(), 1);
                assert_eq!(s.errors[0].kind, ErrorKind::UnsafeFunctionCall);
            }
            _ => panic!("Expected Ok safety"),
        }
    }

    #[test]
    fn test_merge_preserves_mutation_candidates() {
        // When the same .py appears in more than one python_library, merging the
        // duplicate copies must keep a mutation candidate recorded in only one of them,
        // or the reduce step would never resolve that cross-library call.
        let candidate = MutationCandidate {
            callee: mn("dep.configure"),
            site: MutationCandidateSite::Function { name: mn("f") },
            arg_offset: 0,
            imported_args: ImportedArgs {
                unsafe_arg_indices: 1,
                ..Default::default()
            },
        };

        // Copy A of `dup` carries no mutation candidate.
        let map_a = SafetyMap::new();
        map_a.insert(mn("dup"), SafetyResult::Ok(ModuleSafety::new()));
        let mut cache = LibraryCache::build(
            &map_a,
            &ImportGraph::new(),
            &Exports::empty(),
            &SideEffectMap::new(),
        );

        // Copy B of `dup` carries the mutation candidate.
        let map_b = SafetyMap::new();
        let mut safety_b = ModuleSafety::new();
        safety_b.mutation_candidates.push(candidate.clone());
        map_b.insert(mn("dup"), SafetyResult::Ok(safety_b));
        let dep_cache = LibraryCache::build(
            &map_b,
            &ImportGraph::new(),
            &Exports::empty(),
            &SideEffectMap::new(),
        );

        cache.merge_dep_caches(vec![dep_cache]);

        let dup = module(&cache, "dup");
        assert_eq!(
            dup.mutation_candidates,
            vec![candidate],
            "merge must preserve mutation candidates from a duplicate module copy",
        );
    }

    #[test]
    fn test_merge_deduplicates_repeated_mutation_candidates() {
        let candidate = MutationCandidate {
            callee: mn("dep.configure"),
            site: MutationCandidateSite::Function { name: mn("f") },
            arg_offset: 0,
            imported_args: ImportedArgs::default(),
        };
        let distinct_candidate = MutationCandidate {
            callee: mn("dep.validate"),
            ..candidate.clone()
        };
        let make_cache = |candidates| {
            let safety_map = SafetyMap::new();
            let mut safety = ModuleSafety::new();
            safety.mutation_candidates = candidates;
            safety_map.insert(mn("dup"), SafetyResult::Ok(safety));
            LibraryCache::build(
                &safety_map,
                &ImportGraph::new(),
                &Exports::empty(),
                &SideEffectMap::new(),
            )
        };

        let mut cache = make_cache(vec![candidate.clone()]);
        cache.merge_dep_caches(vec![
            make_cache(vec![candidate.clone(), distinct_candidate.clone()]),
            make_cache(vec![candidate.clone()]),
        ]);

        assert_eq!(cache.modules.len(), 1);
        let candidates = &cache.modules[0].mutation_candidates;
        assert_eq!(candidates.len(), 2);
        assert!(candidates.contains(&candidate));
        assert!(candidates.contains(&distinct_candidate));
    }

    #[test]
    fn test_own_build_plus_merge_matches_full_build() {
        let dep_modules: Vec<(&str, &str)> = vec![
            ("safe_module", "def greet(name): return f'Hello, {name}'\n"),
            (
                "unsafe_module",
                "import os\nresult = os.path.join('a', 'b')\ndef helper(): return result\n",
            ),
            (
                "importer",
                "from safe_module import greet\nfrom unsafe_module import helper\n",
            ),
            (
                "has_finalizer",
                "class Leaker:\n    def __del__(self):\n        pass\n",
            ),
            ("uses_exec", "exec('x = 1')\n"),
        ];
        let own_module = (
            "main",
            "from importer import greet\ndef main():\n    print(greet('world'))\n",
        );

        let dep_cache = build_cache(&TestSources::new(&dep_modules));
        assert_eq!(dep_cache.modules.len(), 5);

        let mut own_cache = build_cache(&TestSources::new(&[own_module]));
        assert_eq!(own_cache.modules.len(), 1);

        own_cache.merge_dep_caches(vec![dep_cache]);
        assert_eq!(own_cache.modules.len(), 6);

        let mut all_modules = dep_modules.clone();
        all_modules.push(own_module);
        let full_cache = build_cache(&TestSources::new(&all_modules));
        assert_eq!(full_cache.modules.len(), 6);

        let full_names: Vec<&str> = full_cache.modules.iter().map(|m| m.name.as_str()).collect();
        let merged_names: Vec<&str> = own_cache.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(full_names, merged_names);

        for (full_mod, merged_mod) in full_cache.modules.iter().zip(own_cache.modules.iter()) {
            let full_safe = matches!(&full_mod.safety, CachedSafety::Ok(s) if s.is_safe());
            let merged_safe = matches!(&merged_mod.safety, CachedSafety::Ok(s) if s.is_safe());
            assert_eq!(
                full_safe,
                merged_safe,
                "Module {} safety mismatch: full={}, merged={}",
                full_mod.name.as_str(),
                full_safe,
                merged_safe,
            );
        }
    }

    #[test]
    fn test_resolve_cross_library_constructor_call() {
        let dep_cache = build_cache(&TestSources::new(&[(
            "dep",
            "from dataclasses import dataclass\n\
             @dataclass\n\
             class MyClass:\n\
             \x20   value: int = 0\n",
        )]));

        let own_sources = TestSources::new(&[(
            "caller",
            "from dep import MyClass\n\
             instance = MyClass()\n",
        )]);
        let own_cache = build_cache(&own_sources);

        let caller_before = module(&own_cache, "caller");
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (dep is missing)",
        );

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let caller_after = module(&own_cache, "caller");
        assert!(
            caller_after.is_safe(),
            "caller should be safe after resolving cross-library constructor call",
        );
    }

    #[test]
    fn test_resolve_cross_library_unsafe_constructor() {
        let cache = resolved_cache(
            &[(
                "caller",
                "from dep import MyClass\n\
                 instance = MyClass()\n",
            )],
            &[
                (
                    "dep",
                    "import dep_state\n\
             class MyClass:\n\
             \x20   def __init__(self):\n\
             \x20       dep_state.counter = dep_state.counter + 1\n",
                ),
                ("dep_state", "counter = 0\n"),
            ],
        );

        let caller = module(&cache, "caller");
        assert!(
            !caller.is_safe(),
            "caller should remain unsafe when constructor has side effects",
        );
    }

    #[test]
    fn test_cross_library_constructor_mutates_imported_arg_is_unsafe() {
        // A cross-library class whose constructor mutates a passed-in parameter is
        // safe in isolation, but `caller` passes imported state into it at import,
        // so `caller` must stay unsafe. The class FQN is unresolved in the consuming
        // library, so the mutation candidate records the class, not `__init__`.
        let cache = resolved_cache(
            &[
                ("config", "settings = 1\n"),
                (
                    "caller",
                    "from dep import MyClass\n\
                     from config import settings\n\
                     instance = MyClass(settings)\n",
                ),
            ],
            &[(
                "dep",
                "class MyClass:\n\
                 \x20   def __init__(self, x):\n\
                 \x20       x.attr = 1\n",
            )],
        );

        let caller = module(&cache, "caller");
        assert!(
            !caller.is_safe(),
            "constructor mutates the imported arg, so caller must stay unsafe",
        );
    }

    #[test]
    fn test_resolve_cross_library_unsafe_if_imported_constructor() {
        let dep_cache = build_cache(&TestSources::new(&[(
            "defs",
            "counter = 0\n\
             class Foo:\n\
             \x20   def __init__(self):\n\
             \x20       global counter\n\
             \x20       counter += 1\n\
             obj = Foo()\n",
        )]));

        let defs_mod = module(&dep_cache, "defs");
        assert_ne!(
            defs_mod.function_safety.get("Foo").map(|info| info.verdict),
            Some(FunctionSafety::Safe),
            "class Foo must not be cached as Safe when __init__ mutates module globals",
        );

        let own_sources = TestSources::new(&[(
            "caller",
            "from defs import Foo\n\
             instance = Foo()\n",
        )]);
        let own_cache = build_cache(&own_sources);

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let caller = module(&own_cache, "caller");
        assert!(
            !caller.is_safe(),
            "caller should remain unsafe: Foo.__init__ mutates module globals",
        );
    }

    #[test]
    fn test_unsafe_if_imported_propagates_through_same_module_caller() {
        // `bump` mutates a module global -> UnsafeIfImported. `helper` calls `bump`
        // within the same module, so it *inherits* UnsafeIfImported: running
        // `helper` transitively mutates `lib`'s global, which is safe only when
        // `helper` is called from `lib` itself. `trigger`, in another module, calls
        // `helper` cross-module, so importing `trigger`'s module would trigger the
        // mutation -> `trigger` is hard Unsafe.
        //
        // This guards two things: (1) the "...if imported" qualifier propagates
        // through a same-module intermediary rather than being resolved to Safe
        // (otherwise a cross-module caller further up is wrongly treated as safe),
        // and (2) each verdict depends only on module membership, not on which
        // entry point the analysis reached the function from first.
        let cache = build_cache(&TestSources::new(&[
            (
                "lib",
                "counter = 0\n\
                 def bump():\n\
                 \x20   global counter\n\
                 \x20   counter += 1\n\
                 def helper():\n\
                 \x20   bump()\n",
            ),
            (
                "other",
                "from lib import helper\n\
                 def trigger():\n\
                 \x20   helper()\n",
            ),
        ]));

        assert_eq!(
            function_verdict(&cache, "lib", "bump"),
            FunctionSafety::UnsafeIfImported,
            "bump mutates a module global, so it is UnsafeIfImported",
        );
        assert_eq!(
            function_verdict(&cache, "lib", "helper"),
            FunctionSafety::UnsafeIfImported,
            "helper calls bump within its own module, so it inherits UnsafeIfImported",
        );

        assert_eq!(
            function_verdict(&cache, "other", "trigger"),
            FunctionSafety::Unsafe,
            "trigger calls helper cross-module, so it is hard Unsafe",
        );
    }

    #[test]
    fn test_param_mutation_through_function_is_unsafe() {
        // `sink` mutates its parameter (fine in isolation -> Safe). `f` calls
        // `sink(other)`, passing the imported module `other`, so running `f`
        // mutates imported state at import time -> `f` is Unsafe. `app` calls `f`
        // at module scope, so importing `app` runs that mutation -> `app` fails.
        //
        // This is detected as a property of `f`'s verdict (not only via the
        // module-scope call-tree traversal, which short-circuits on cached
        // verdicts), so the transitive case is flagged deterministically rather
        // than being a false-safe.
        let cache = build_cache(&TestSources::new(&[
            ("other", "value = 1\n"),
            (
                "m",
                "import other\n\
                 def sink(x):\n\
                 \x20   x.attr = 1\n\
                 def f():\n\
                 \x20   sink(other)\n",
            ),
            (
                "app",
                "from m import f\n\
                 f()\n",
            ),
        ]));

        assert_eq!(
            function_verdict(&cache, "m", "sink"),
            FunctionSafety::Safe,
            "sink mutates its own parameter, which is safe in isolation",
        );
        assert_eq!(
            function_verdict(&cache, "m", "f"),
            FunctionSafety::Unsafe,
            "f passes an imported var to a mutated parameter, mutating imported state",
        );

        let app = module(&cache, "app");
        assert!(
            !app.is_safe(),
            "app calls f at import time, so importing app runs the mutation",
        );
    }

    #[test]
    fn test_cache_records_mutated_params() {
        // The per-function mutated-parameter summary is carried in (and survives
        // serialization of) the cache, so cross-library callers can resolve it at
        // reduce time. `sink` mutates its first parameter `x`, so its cached entry
        // must record `x` at positional index 0.
        let cache = round_trip(&build_cache(&TestSources::new(&[(
            "m",
            "def sink(x):\n\
             \x20   x.attr = 1\n",
        )])));

        let m = module(&cache, "m");
        let sink = m
            .function_safety
            .get("sink")
            .expect("sink should have a function_safety entry");
        let param = sink
            .mutated_params
            .iter()
            .find(|param| param.name == mn("x"))
            .unwrap_or_else(|| panic!("sink should record mutated parameter x: {sink:?}"));
        assert_eq!(
            param.position,
            ParamPosition::Positional(0),
            "x should be positional index 0"
        );
    }

    #[test]
    fn test_cache_records_cross_library_mutation_candidate() {
        // A call passing an imported object to a callee unresolved in this
        // library is cached (and survives serialization) as a mutation candidate for the
        // reduce step. `f` passes the imported module `other` to `sinklib.sink`,
        // which is not in this library, so the map step cannot evaluate it.
        let cache = round_trip(&build_cache(&TestSources::new(&[
            ("other", "value = 1\n"),
            (
                "m",
                "import other\n\
                 from sinklib import sink\n\
                 def f():\n\
                 \x20   sink(other)\n",
            ),
        ])));

        let m = module(&cache, "m");
        let candidate = m
            .mutation_candidates
            .iter()
            .find(|o| o.callee == mn("sinklib.sink"))
            .unwrap_or_else(|| {
                panic!(
                    "expected a cross-library mutation candidate for sinklib.sink; got {:?}",
                    m.mutation_candidates,
                )
            });
        assert_eq!(
            candidate.site,
            MutationCandidateSite::Function { name: mn("f") },
            "the call is in f's body"
        );
        assert_eq!(
            candidate.arg_offset, 0,
            "plain function call has no receiver offset"
        );
        assert!(
            candidate.imported_args.unsafe_arg_indices & 1 != 0,
            "the imported `other` is passed at positional index 0; got {:#b}",
            candidate.imported_args.unsafe_arg_indices,
        );
    }

    #[test]
    fn test_cache_no_mutation_candidate_for_in_library_callee() {
        // Negative case: when the callee is resolvable in this library, the map
        // step handles it directly, so no mutation candidate is cached (avoids the reduce
        // step double-counting).
        let cache = build_cache(&TestSources::new(&[
            ("other", "value = 1\n"),
            (
                "m",
                "import other\n\
                 def sink(x):\n\
                 \x20   x.attr = 1\n\
                 def f():\n\
                 \x20   sink(other)\n",
            ),
        ]));

        let m = module(&cache, "m");
        assert!(
            m.mutation_candidates.is_empty(),
            "in-library sink is handled by the map step; got {:?}",
            m.mutation_candidates,
        );
    }

    #[test]
    fn test_cross_library_param_mutation_is_unsafe() {
        // Cross-library counterpart of test_param_mutation_through_function_is_unsafe.
        // `sink` lives in a dependency library; `other`/`m`/`app` live in the
        // consuming library. `m.f` passes the imported module `other` to the
        // cross-library `sink`, which mutates it, so importing `app` runs that
        // mutation at module scope -> `app` must be unsafe.
        let dep_cache = build_cache(&TestSources::new(&[(
            "sinklib",
            "def sink(x):\n\
             \x20   x.attr = 1\n",
        )]));

        let own_cache = build_cache(&TestSources::new(&[
            ("other", "value = 1\n"),
            (
                "m",
                "import other\n\
                 from sinklib import sink\n\
                 def f():\n\
                 \x20   sink(other.value)\n",
            ),
            (
                "app",
                "from m import f\n\
                 f()\n",
            ),
        ]));

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let m = module(&own_cache, "m");
        assert_eq!(
            m.function_safety.get("f").map(|i| i.verdict),
            Some(FunctionSafety::Unsafe),
            "f passes an imported var to a cross-library mutating parameter",
        );

        let app = module(&own_cache, "app");
        assert!(
            !app.is_safe(),
            "app calls f at import time, so importing app runs the cross-library mutation",
        );
    }

    #[test]
    fn test_cross_library_module_scope_mutation_is_unsafe() {
        // Module-scope counterpart: `main` calls the cross-library `configure`
        // directly at import time, passing the imported `settings`. The reduce
        // step must add an ImportedVarArgument error to `main`.
        let dep_cache = build_cache(&TestSources::new(&[(
            "setup",
            "def configure(x):\n\
             \x20   x.enabled = True\n",
        )]));

        let own_cache = build_cache(&TestSources::new(&[
            ("config", "settings = 1\n"),
            (
                "main",
                "from setup import configure\n\
                 from config import settings\n\
                 configure(settings)\n",
            ),
        ]));

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let main = module(&own_cache, "main");
        assert!(
            !main.is_safe(),
            "main mutates the imported `settings` via cross-library `configure` at import time",
        );
    }

    #[test]
    fn test_class_promotion_follows_its_constructor_methods() {
        // `Model` is blocked on `Model.__init__`, not on what it calls, so the
        // blocking callee resolving safe must not promote the unsafe constructor.
        let mut models = safe_cached_module("models", &[], &[]);
        models.function_safety = fsmap([
            unsafe_missing_dep("Model", "models.Model.__init__"),
            unsafe_("Model.__init__"),
        ]);
        let mut helpers = safe_cached_module("helpers", &[], &[]);
        helpers.function_safety = fsmap([safe("initialize")]);
        let mut cache = LibraryCache::empty();
        cache.modules = vec![models, helpers];

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let models = cache
            .modules
            .iter()
            .find(|module| module.name == mn("models"))
            .unwrap();
        let verdict = models
            .function_safety
            .get("Model")
            .map(|info| info.verdict)
            .expect("the class entry should survive the reduce");

        assert!(
            !verdict.is_safe(),
            "the class must not promote past an unsafe constructor method",
        );
    }

    #[test]
    fn test_constructor_keeps_recoverable_verdict_for_missing_dep() {
        let cache = build_cache(&TestSources::new(&[(
            "models",
            "from helpers import initialize\n\nclass Model:\n    def __init__(self):\n        initialize()\n",
        )]));
        let models = cache
            .modules
            .iter()
            .find(|module| module.name == mn("models"))
            .unwrap();
        let verdict = models
            .function_safety
            .get("Model")
            .map(|info| info.verdict)
            .expect("the class should have a cached constructor summary");

        assert!(
            verdict.has(FunctionSafety::UnsafeMissingDep),
            "an unresolved cross-library callee keeps the constructor recoverable",
        );
        assert!(
            !verdict.has(FunctionSafety::Unsafe),
            "a hard-unsafe constructor could never be promoted once `initialize` resolves",
        );
    }

    #[test]
    fn test_cross_library_non_mutating_callee_stays_safe() {
        // Unconfirmed direction (parity preservation): the cross-library callee does
        // NOT mutate its parameter, so the deferred pessimism must be resolved
        // and `main` must end up safe — matching single-pass analysis.
        let dep_cache = build_cache(&TestSources::new(&[(
            "setup",
            "def configure(x):\n\
             \x20   return x\n",
        )]));

        let own_cache = build_cache(&TestSources::new(&[
            ("config", "settings = 1\n"),
            (
                "main",
                "from setup import configure\n\
                 from config import settings\n\
                 configure(settings)\n",
            ),
        ]));

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let main = module(&own_cache, "main");
        assert!(
            main.is_safe(),
            "configure does not mutate its parameter, so main is safe to lazily import",
        );
    }

    #[test]
    fn test_cross_library_wrapper_non_mutating_stays_safe() {
        // One-level wrapper: `g` (a function) makes the cross-library call and is
        // resolved to Safe; `main` calls `g` at module scope. The resolution must
        // also clear `main`'s deferred error even though the promotion fixpoint
        // promotes nothing.
        let dep_cache = build_cache(&TestSources::new(&[(
            "setup",
            "def configure(x):\n\
             \x20   return x\n",
        )]));

        let own_cache = build_cache(&TestSources::new(&[
            ("config", "settings = 1\n"),
            (
                "lib",
                "from setup import configure\n\
                 from config import settings\n\
                 def g():\n\
                 \x20   configure(settings)\n",
            ),
            ("main", "from lib import g\ng()\n"),
        ]));

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let main = module(&own_cache, "main");
        assert!(
            main.is_safe(),
            "g's cross-library callee does not mutate, so main must be safe",
        );
    }

    #[test]
    fn test_cross_library_deep_wrapper_non_mutating_stays_safe() {
        // Multi-level wrapper: main -> f() -> g() -> configure(imported),
        // configure is non-mutating cross-library.
        // All of f/g must end Safe and main must be cleared.
        let dep_cache = build_cache(&TestSources::new(&[(
            "setup",
            "def configure(x):\n\
             \x20   return x\n",
        )]));

        let own_cache = build_cache(&TestSources::new(&[
            ("config", "settings = 1\n"),
            (
                "lib",
                "from setup import configure\n\
                 from config import settings\n\
                 def g():\n\
                 \x20   configure(settings)\n\
                 def f():\n\
                 \x20   g()\n",
            ),
            ("main", "from lib import f\nf()\n"),
        ]));

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let main = module(&own_cache, "main");
        assert!(
            main.is_safe(),
            "the whole chain is non-mutating, so main must be safe",
        );
    }

    #[test]
    fn test_cross_library_wrapper_unsafe_callee_stays_unsafe() {
        // `g` passes an imported object to a cross-library callee that resolves as
        // Unsafe (recursive) but does not mutate the imported arg. The unconfirmed
        // mutation candidate must NOT resolve `g`'s missing dep on that unsafe
        // callee, or `g` — and `main`, which runs it at import — would be wrongly
        // promoted to Safe.
        let dep_cache = build_cache(&TestSources::new(&[(
            "setup",
            "def configure(x):\n\
             \x20   configure(x)\n",
        )]));

        let own_cache = build_cache(&TestSources::new(&[
            ("config", "settings = 1\n"),
            (
                "lib",
                "from setup import configure\n\
                 from config import settings\n\
                 def g():\n\
                 \x20   configure(settings)\n",
            ),
            ("main", "from lib import g\ng()\n"),
        ]));

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let main = module(&own_cache, "main");
        assert!(
            !main.is_safe(),
            "g's cross-library callee resolves unsafe, so main must stay unsafe",
        );
    }

    #[test]
    fn test_resolve_to_known_module_exact_and_parent() {
        let known = [mn("foo"), mn("bar.baz")].into_iter().collect();

        assert_eq!(resolve_to_known_module(&mn("foo"), &known), Some(mn("foo")));
        assert_eq!(
            resolve_to_known_module(&mn("bar.baz"), &known),
            Some(mn("bar.baz")),
        );
        assert_eq!(
            resolve_to_known_module(&mn("bar.baz.Qux"), &known),
            Some(mn("bar.baz")),
        );
        assert_eq!(resolve_to_known_module(&mn("unknown"), &known), None);
    }

    #[test]
    fn test_dedupe_implicit_imports_preserves_dotted_paths() {
        let mut implicits = vec![mn("dep.ClassName"), mn("other"), mn("missing.Foo")];
        dedupe_implicit_imports(&mut implicits);

        assert_eq!(
            implicits,
            vec![mn("dep.ClassName"), mn("other"), mn("missing.Foo")]
        );
    }

    #[test]
    fn test_dedupe_implicit_imports_deduplicates_exact_paths() {
        let mut implicits = vec![mn("dep.ClassA"), mn("dep.ClassB"), mn("dep.ClassA")];
        dedupe_implicit_imports(&mut implicits);

        assert_eq!(implicits, vec![mn("dep.ClassA"), mn("dep.ClassB")]);
    }

    #[test]
    fn test_precompute_function_safety_populates_all_functions() {
        let cache = build_cache(&TestSources::new(&[(
            "mod_a",
            "def helper(): return 1\ndef unused(): return 2\n",
        )]));

        let mod_a = module(&cache, "mod_a");
        assert!(
            mod_a.function_safety.contains_key("helper"),
            "helper should have a function_safety entry, got keys: {:?}",
            mod_a.function_safety.keys().collect::<Vec<_>>()
        );
        assert!(
            mod_a.function_safety.contains_key("unused"),
            "unused should have a function_safety entry, got keys: {:?}",
            mod_a.function_safety.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_propagate_re_export_unions_source_concerns() {
        // Safety concerns are orthogonal bits, so a re-exported symbol inherits the
        // union of its own and its source's concerns; neither masks the other. The
        // UnsafeMissingDep bit is discharged separately by the promotion fixpoint
        // once its callees resolve, not by re-export propagation choosing the "safer"
        // verdict.
        let mut cache = LibraryCache::empty();

        cache.modules.push(CachedModule {
            name: mn("c"),
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: Default::default(),
            missing_imports: Default::default(),
            ambiguous_imports: Default::default(),
            side_effect_imports: Default::default(),
            function_safety: fsmap([unsafe_if_imported("foo")]),
            mutation_candidates: Vec::new(),
        });

        cache.modules.push(CachedModule {
            name: mn("b"),
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: Default::default(),
            missing_imports: Default::default(),
            ambiguous_imports: Default::default(),
            side_effect_imports: Default::default(),
            function_safety: fsmap([(
                "foo".to_string(),
                FunctionSafetyInfo::new(FunctionSafety::UnsafeMissingDep),
            )]),
            mutation_candidates: Vec::new(),
        });

        cache.exports.re_exports.push(CachedReExport {
            exported_module: mn("b"),
            exported_attr: "foo".to_string(),
            imported_module: mn("c"),
            imported_attr: "foo".to_string(),
        });

        cache.propagate_re_export_safety();

        let b = module(&cache, "b");
        let verdict = b
            .function_safety
            .get("foo")
            .map(|info| info.verdict)
            .expect("re-export propagation should populate b.foo");
        assert!(
            verdict.has(FunctionSafety::UnsafeMissingDep),
            "b.foo keeps its own UnsafeMissingDep concern",
        );
        assert!(
            verdict.has(FunctionSafety::UnsafeIfImported),
            "b.foo also inherits the source's UnsafeIfImported concern",
        );
    }

    #[test]
    fn test_resolve_cross_library_function_call() {
        let dep_cache = build_cache(&TestSources::new(&[("dep", "def safe_func(): return 1\n")]));

        let own_cache = build_cache(&TestSources::new(&[(
            "caller",
            "from dep import safe_func\nx = safe_func()\n",
        )]));

        let caller_before = module(&own_cache, "caller");
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (dep is missing)",
        );

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let caller_after = module(&own_cache, "caller");
        assert!(
            caller_after.is_safe(),
            "caller should be safe after resolving cross-library function call",
        );
    }

    #[test]
    fn test_errors_not_cleared_without_missing_imports() {
        let safety_map = SafetyMap::new();
        let mut unsafe_safety = ModuleSafety::new();
        unsafe_safety.add_error(SafetyError::new(
            ErrorKind::UnknownFunctionCall,
            "dep.helper()".to_string(),
            Default::default(),
        ));
        safety_map.insert(mn("caller"), SafetyResult::Ok(unsafe_safety));

        let mut dep_safety = ModuleSafety::new();
        dep_safety.function_safety.insert(
            "helper".to_string(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        safety_map.insert(mn("dep"), SafetyResult::Ok(dep_safety));

        let mut import_graph = ImportGraph::new();
        import_graph.graph.add_node(&mn("caller"));
        import_graph.graph.add_node(&mn("dep"));
        import_graph.graph.add_edge(&mn("caller"), &mn("dep"));

        let exports = Exports::empty();
        let mut cache =
            LibraryCache::build(&safety_map, &import_graph, &exports, &SideEffectMap::new());

        assert!(
            module(&cache, "caller").missing_imports.is_empty(),
            "no missing imports",
        );

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let caller = module(&cache, "caller");
        assert!(
            !caller.is_safe(),
            "errors from already-imported modules should not be cleared (conservative)",
        );
    }

    /// Reduce must use the constructor callees the map phase recorded, not the
    /// set it would derive itself.
    #[test]
    fn test_recorded_constructor_callees_outrank_derived_ones() {
        let mut cache = LibraryCache {
            modules: vec![
                cached_module("app")
                    .errors(vec![cached_error(
                        ErrorKind::UnknownFunctionCall,
                        "sub.Sentinel",
                    )])
                    .imports(&["sub"])
                    .build(),
                cached_module("sub")
                    .function_safety([safe("Sentinel")])
                    .build(),
                cached_module("base")
                    .function_safety([unsafe_("Base.__init__")])
                    .build(),
            ],
            exports: empty_exports(),
            class_bases: Vec::new(),
            constructor_callees: vec![(mn("sub.Sentinel"), inherited(mn("base.Base.__init__")))],
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        assert!(
            !app.is_safe(),
            "an inherited Unsafe constructor recorded by the map phase must keep the call unsafe",
        );
    }

    /// A recorded callee that is `Safe` clears the call, so the test is
    /// pinning the verdict rather than the mere presence of a recorded entry.
    #[test]
    fn test_recorded_safe_constructor_callee_clears() {
        let mut cache = LibraryCache {
            modules: vec![
                cached_module("app")
                    .errors(vec![cached_error(
                        ErrorKind::UnknownFunctionCall,
                        "sub.Sentinel",
                    )])
                    .imports(&["sub"])
                    .build(),
                cached_module("sub")
                    .function_safety([safe("Sentinel")])
                    .build(),
                cached_module("base")
                    .function_safety([safe("Base.__init__")])
                    .build(),
            ],
            exports: empty_exports(),
            class_bases: Vec::new(),
            constructor_callees: vec![(mn("sub.Sentinel"), inherited(mn("base.Base.__init__")))],
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        assert!(
            app.is_safe(),
            "a recorded constructor callee that is Safe should clear the call",
        );
    }

    /// A class's own `__new__` runs on instantiation just as its `__init__` does,
    /// so a side effect there has to keep the call unsafe. Before the method sets
    /// were unified the own half of the mask covered only `__init__` and
    /// `__post_init__`, and this cleared.
    #[test]
    fn test_own_new_side_effect_keeps_call_unsafe() {
        let cache = resolved_cache(
            &[(
                "caller",
                "from dep import Widget\n\
                 instance = Widget()\n",
            )],
            &[
                (
                    "dep",
                    "import dep_state\n\
                     class Widget:\n\
                     \x20   def __new__(cls):\n\
                     \x20       dep_state.counter = dep_state.counter + 1\n\
                     \x20       return super().__new__(cls)\n\
                     \x20   def __init__(self):\n\
                     \x20       pass\n",
                ),
                ("dep_state", "counter = 0\n"),
            ],
        );

        let caller = module(&cache, "caller");
        assert!(
            !caller.is_safe(),
            "a side effect in the class's own __new__ must keep the call unsafe",
        );
    }

    /// End-to-end cover for the inherited case `extra` exists to carry: `Sentinel`
    /// defines no constructor, so the map phase walks its MRO and records
    /// `base.Base.__init__` as the callee. That callee has an import-time side
    /// effect, so instantiating `Sentinel` from another library must stay unsafe
    /// -- the aggregate `Sentinel` verdict alone would clear it.
    #[test]
    fn test_cross_library_inherited_constructor_stays_unsafe() {
        let cache = resolved_cache(
            &[(
                "caller",
                "from sub import Sentinel\n\
                 instance = Sentinel()\n",
            )],
            &[
                (
                    "base",
                    "import base_state\n\
                     class Base:\n\
                     \x20   def __init__(self):\n\
                     \x20       base_state.counter = base_state.counter + 1\n",
                ),
                ("base_state", "counter = 0\n"),
                (
                    "sub",
                    "from base import Base\n\
                     class Sentinel(Base):\n\
                     \x20   pass\n",
                ),
            ],
        );

        let caller = module(&cache, "caller");
        assert!(
            !caller.is_safe(),
            "an inherited constructor with a side effect must keep the call unsafe",
        );
    }

    /// The safe counterpart, so the inherited path is pinned to the callee's
    /// verdict rather than to the mere presence of an inherited constructor.
    #[test]
    fn test_cross_library_inherited_safe_constructor_clears() {
        let cache = resolved_cache(
            &[(
                "caller",
                "from sub import Sentinel\n\
                 instance = Sentinel()\n",
            )],
            &[
                (
                    "base",
                    "class Base:\n\
                     \x20   def __init__(self):\n\
                     \x20       self.value = 0\n",
                ),
                (
                    "sub",
                    "from base import Base\n\
                     class Sentinel(Base):\n\
                     \x20   pass\n",
                ),
            ],
        );

        let caller = module(&cache, "caller");
        assert!(
            caller.is_safe(),
            "an inherited constructor with no side effect should clear the call",
        );
    }

    /// The state left behind when a constructor is promoted at reduce time:
    /// `Widget.__init__` was cached `UnsafeMissingDep`, which failed the
    /// constructor check and gave `app` its error, and `precompute_constructor_safety`
    /// recorded the class-level `Widget` entry as hard `Unsafe`. Resolving the
    /// missing dep promotes `Widget.__init__` to `Safe`, but the class-level entry
    /// stays `Unsafe` because `can_promote` rejects hard `Unsafe`. Clearing the
    /// error therefore has to read the constructor methods, not that entry.
    #[test]
    fn test_safe_constructor_error_clears_without_promotion() {
        let mut cache = LibraryCache {
            modules: vec![
                cached_module("app")
                    .errors(vec![cached_error(
                        ErrorKind::UnsafeFunctionCall,
                        "dep.Widget",
                    )])
                    .imports(&["dep"])
                    .build(),
                cached_module("dep")
                    .function_safety([unsafe_("Widget"), safe("Widget.__init__")])
                    .build(),
            ],
            exports: empty_exports(),
            class_bases: Vec::new(),
            constructor_callees: vec![(mn("dep.Widget"), own_init())],
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        assert!(app.is_safe(), "safe constructor call should be cleared");
    }

    #[test]
    fn test_safe_constructor_error_uses_nested_module_parent() {
        let mut cache = LibraryCache {
            modules: vec![
                cached_module("pkg").build(),
                cached_module("pkg.debug")
                    .errors(vec![cached_error(
                        ErrorKind::UnsafeFunctionCall,
                        "pkg.debug.Printer",
                    )])
                    .function_safety([
                        unsafe_("Printer"),
                        safe("Printer.__init__"),
                        (
                            "Printer.__call__".to_owned(),
                            FunctionSafetyInfo::new(FunctionSafety::UnsafeMissingDep),
                        ),
                    ])
                    .build(),
            ],
            exports: empty_exports(),
            class_bases: Vec::new(),
            constructor_callees: vec![(mn("pkg.debug.Printer"), own_init())],
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let debug = module(&cache, "pkg.debug");
        assert!(
            debug.is_safe(),
            "same-module constructor calls should use the concrete nested module and the constructor method verdict",
        );
    }

    #[test]
    fn test_safe_function_error_does_not_clear_without_promotion() {
        let mut cache = LibraryCache {
            modules: vec![
                cached_module("app")
                    .errors(vec![cached_error(
                        ErrorKind::UnsafeFunctionCall,
                        "dep.helper",
                    )])
                    .imports(&["dep"])
                    .build(),
                cached_module("dep")
                    .function_safety([safe("helper")])
                    .build(),
            ],
            exports: empty_exports(),
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        assert!(
            !app.is_safe(),
            "ordinary safe function calls should not clear in the no-promotion pass",
        );
    }

    #[test]
    fn test_unsafe_if_imported_constructor_clears_for_same_module() {
        let mut cache = LibraryCache {
            modules: vec![
                cached_module("dep")
                    .errors(vec![cached_error(
                        ErrorKind::UnsafeFunctionCall,
                        "dep.Widget",
                    )])
                    .function_safety([
                        unsafe_if_imported("Widget"),
                        unsafe_if_imported("Widget.__init__"),
                    ])
                    .build(),
            ],
            exports: empty_exports(),
            class_bases: Vec::new(),
            constructor_callees: vec![(mn("dep.Widget"), own_init())],
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let dep = module(&cache, "dep");
        assert!(
            dep.is_safe(),
            "UnsafeIfImported constructors are safe when called from their own module",
        );
    }

    #[test]
    fn test_unsafe_if_imported_constructor_stays_unsafe_cross_module() {
        let mut cache = LibraryCache {
            modules: vec![
                cached_module("app")
                    .errors(vec![cached_error(
                        ErrorKind::UnsafeFunctionCall,
                        "dep.Widget",
                    )])
                    .imports(&["dep"])
                    .build(),
                cached_module("dep")
                    .function_safety([
                        unsafe_if_imported("Widget"),
                        unsafe_if_imported("Widget.__init__"),
                    ])
                    .build(),
            ],
            exports: empty_exports(),
            class_bases: Vec::new(),
            constructor_callees: vec![(mn("dep.Widget"), own_init())],
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        assert!(
            !app.is_safe(),
            "UnsafeIfImported constructors stay unsafe when called cross-module",
        );
    }

    #[test]
    fn test_class_decorator_error_uses_constructor_safety() {
        let mut cache = LibraryCache {
            modules: vec![
                cached_module("app")
                    .errors(vec![parameterized_decorator_error("dep.Decorator")])
                    .imports(&["dep"])
                    .build(),
                cached_module("dep")
                    .function_safety([
                        safe("Decorator"),
                        safe("Decorator.__init__"),
                        unsafe_("Decorator.helper"),
                    ])
                    .build(),
            ],
            exports: empty_exports(),
            class_bases: Vec::new(),
            constructor_callees: vec![(mn("dep.Decorator"), own_init())],
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        assert!(
            app.is_safe(),
            "class decorators should be verified from constructor safety, not class method safety",
        );
    }

    #[test]
    fn test_error_cleared_from_ambiguous_import() {
        let dep_cache = build_cache(&TestSources::new(&[
            ("pkg", ""),
            ("pkg.sub", "def helper(): return 1\n"),
        ]));

        let own_cache = build_cache(&TestSources::new(&[(
            "caller",
            "from pkg import sub\nx = sub.helper()\n",
        )]));

        let caller_before = module(&own_cache, "caller");
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (pkg.sub is unresolved)",
        );

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let caller = module(&own_cache, "caller");
        assert!(
            caller.imports.contains(&mn("pkg.sub")),
            "ambiguous import pkg.sub should be resolved as a real import",
        );
        assert!(
            caller.is_safe(),
            "caller error should be cleared once the ambiguous import feeds into error clearing",
        );
    }

    #[test]
    fn test_multiple_ambiguous_imports_resolve_independently() {
        let dep_cache = build_cache(&TestSources::new(&[
            ("pkg", ""),
            ("pkg.one", "def helper(): return 1\n"),
            ("pkg.two", "def helper(): return 2\n"),
        ]));
        let mut own_cache = build_cache(&TestSources::new(&[
            ("caller_one", "from pkg import one\nx = one.helper()\n"),
            ("caller_two", "from pkg import two\nx = two.helper()\n"),
        ]));

        let merged_facts = own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors(merged_facts);

        for (caller_name, imported_name) in [("caller_one", "pkg.one"), ("caller_two", "pkg.two")] {
            let caller = own_cache
                .modules
                .iter()
                .find(|module| module.name == mn(caller_name))
                .unwrap();
            assert!(caller.imports.contains(&mn(imported_name)));
            assert!(caller.is_safe());
        }
    }

    #[test]
    fn test_missing_dep_promotion_blocked_by_unsafe_callee() {
        let dep_cache = build_cache(&TestSources::new(&[("dep", "def g():\n    g()\n")]));

        let own_cache = build_cache(&TestSources::new(&[
            ("mid", "from dep import g\ndef f():\n    g()\n"),
            ("top", "from mid import f\nf()\n"),
        ]));

        assert!(
            !module(&own_cache, "top").is_safe(),
            "top unsafe before merge (mid is missing)",
        );

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let top = module(&own_cache, "top");
        assert!(
            !top.is_safe(),
            "top must stay unsafe: importing it runs f() -> unsafe g()",
        );
    }

    #[test]
    fn test_missing_dep_promotion_through_safe_callee() {
        let dep_cache = build_cache(&TestSources::new(&[("dep", "def g():\n    return 1\n")]));

        let own_cache = build_cache(&TestSources::new(&[
            ("mid", "from dep import g\ndef f():\n    g()\n"),
            ("top", "from mid import f\nf()\n"),
        ]));

        assert!(
            !module(&own_cache, "top").is_safe(),
            "top unsafe before merge (mid is missing)",
        );

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let top = module(&own_cache, "top");
        assert!(
            top.is_safe(),
            "top should be safe: f() only reaches the now-resolved safe g()",
        );
    }

    #[test]
    fn test_missing_dep_promotion_preserves_unsafe_if_imported_floor() {
        // `f` both mutates a module global (an `UnsafeIfImported` floor) and calls
        // the cross-library `g`, so its verdict is `UnsafeMissingDep`, masking the
        // floor. When `g` resolves `Safe` at reduce, `f` must promote to its
        // `min_safety_level` (`UnsafeIfImported`), not `Safe` — so `top`, a
        // cross-module caller, stays unsafe.
        let dep_cache = build_cache(&TestSources::new(&[("dep", "def g():\n    return 1\n")]));

        let own_cache = build_cache(&TestSources::new(&[
            (
                "mid",
                "from dep import g\n\
                 counter = 0\n\
                 def f():\n\
                 \x20   global counter\n\
                 \x20   counter += 1\n\
                 \x20   g()\n",
            ),
            ("top", "from mid import f\nf()\n"),
        ]));

        let own_cache = merge_and_resolve(own_cache, dep_cache);

        let top = module(&own_cache, "top");
        assert!(
            !top.is_safe(),
            "g resolves safe but f's UnsafeIfImported floor survives, so top must stay unsafe",
        );
    }

    #[test]
    fn test_dotted_local_name_exact_method_only() {
        // Regression test to make sure we have gotten rid of the "class fallback" heuristic.
        //
        // A `Class.method` callee is verified via its own exact entry, never the
        // bare `Class` (constructor) verdict. `is_call_verified_safe` carries no
        // MRO data, so a method with no exact entry is not verified.
        let mut fs = AHashMap::new();
        fs.insert(
            "MyClass".to_string(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        fs.insert(
            "MyClass.safe_method".to_string(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        let mut func_safety_by_module = AHashMap::new();
        func_safety_by_module.insert(mn("dep"), fs);

        let resolved = [mn("dep")].into_iter().collect();

        assert!(
            is_call_verified_safe("dep.MyClass.safe_method", &resolved, &func_safety_by_module),
            "an exact safe method verdict resolves",
        );

        assert!(
            is_call_verified_safe("dep.MyClass", &resolved, &func_safety_by_module),
            "the class constructor entry resolves directly",
        );

        assert!(
            !is_call_verified_safe(
                "dep.MyClass.other_method",
                &resolved,
                &func_safety_by_module
            ),
            "a method with no exact verdict no longer falls back to the class verdict",
        );
    }

    #[test]
    fn test_reduce_keeps_unsafe_method_error_with_safe_class_prefix() {
        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: mn("app"),
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnsafeMethodCall,
                            metadata: "dep.Widget.configure".to_owned(),
                            parameterized_decorator: false,
                        }],
                        ..Default::default()
                    }),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: [unsafe_missing_dep("wrapper", "dep.safe")]
                        .into_iter()
                        .collect(),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: mn("dep"),
                    safety: CachedSafety::Ok(CachedModuleSafety::default()),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: [safe("Widget"), unsafe_("Widget.configure"), safe("safe")]
                        .into_iter()
                        .collect(),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: empty_exports(),
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        let CachedSafety::Ok(safety) = &app.safety else {
            panic!("app should have cached module safety");
        };
        assert!(
            safety.errors.iter().any(|e| {
                e.kind == ErrorKind::UnsafeMethodCall && e.metadata == "dep.Widget.configure"
            }),
            "an exact unsafe method verdict must not be cleared by the safe class-level verdict",
        );
        assert_eq!(
            app.function_safety.get("wrapper").map(|i| i.verdict),
            Some(FunctionSafety::Safe),
            "the unrelated promotion should still run and trigger global error clearing",
        );
    }

    #[test]
    fn test_reduce_keeps_unknown_method_error_without_exact_method_verdict() {
        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: mn("app"),
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "dep.Widget.configure".to_owned(),
                            parameterized_decorator: false,
                        }],
                        ..Default::default()
                    }),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: [unsafe_missing_dep("wrapper", "dep.safe")]
                        .into_iter()
                        .collect(),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: mn("dep"),
                    safety: CachedSafety::Ok(CachedModuleSafety::default()),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: [safe("Widget"), safe("safe")].into_iter().collect(),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: empty_exports(),
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        let CachedSafety::Ok(safety) = &app.safety else {
            panic!("app should have cached module safety");
        };
        assert!(
            safety.errors.iter().any(|e| {
                e.kind == ErrorKind::UnknownFunctionCall && e.metadata == "dep.Widget.configure"
            }),
            "an unknown method call needs an exact method verdict; class-level safety is insufficient",
        );
        assert_eq!(
            app.function_safety.get("wrapper").map(|i| i.verdict),
            Some(FunctionSafety::Safe),
            "the unrelated promotion should still run and trigger global error clearing",
        );
    }

    #[test]
    fn test_reduce_keeps_unqualified_unknown_call_from_resolved_module() {
        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: mn("app"),
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "b()".to_owned(),
                            parameterized_decorator: false,
                        }],
                        ..Default::default()
                    }),
                    imports: Default::default(),
                    missing_imports: [mn("dep")].into_iter().collect(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: AHashMap::new(),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: mn("dep"),
                    safety: CachedSafety::Ok(CachedModuleSafety::default()),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: fsmap([safe("b")]),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: empty_exports(),
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        let CachedSafety::Ok(safety) = &app.safety else {
            panic!("app should have cached module safety");
        };
        assert!(
            safety
                .errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnknownFunctionCall && e.metadata == "b()"),
            "an unqualified unknown call must not clear just because a resolved module has that function name",
        );
    }

    #[test]
    fn test_reduce_keeps_unqualified_unknown_call_despite_global_safe_name() {
        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: mn("app"),
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnknownFunctionCall,
                            metadata: "b()".to_owned(),
                            parameterized_decorator: false,
                        }],
                        ..Default::default()
                    }),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: fsmap([unsafe_missing_dep("wrapper", "dep.safe")]),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: mn("dep"),
                    safety: CachedSafety::Ok(CachedModuleSafety::default()),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: fsmap([safe("b"), safe("safe")]),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: empty_exports(),
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        let CachedSafety::Ok(safety) = &app.safety else {
            panic!("app should have cached module safety");
        };
        assert!(
            safety
                .errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnknownFunctionCall && e.metadata == "b()"),
            "an unqualified unknown call must not clear just because another module has a safe function with the same short name",
        );
        assert_eq!(
            app.function_safety.get("wrapper").map(|i| i.verdict),
            Some(FunctionSafety::Safe),
            "the unrelated promotion should still run and trigger global error clearing",
        );
    }

    #[test]
    fn test_reduce_indexes_unqualified_candidate_name_on_demand() {
        let mut app = safe_cached_module("app", &[], &[]);
        app.function_safety = fsmap([unsafe_missing_dep("wrapper", "needed")]);
        let mut dep = safe_cached_module("dep", &[], &[]);
        dep.function_safety = fsmap([safe("needed"), safe("unused")]);
        let mut cache = LibraryCache::empty();
        cache.modules = vec![app, dep];

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        assert_eq!(
            cache.modules[0]
                .function_safety
                .get("wrapper")
                .map(|info| info.verdict),
            Some(FunctionSafety::Safe),
        );
    }

    #[test]
    fn test_reduce_indexes_unqualified_error_name_on_demand() {
        let mut app = safe_cached_module("app", &[], &[]);
        app.safety = CachedSafety::Ok(CachedModuleSafety {
            errors: vec![CachedError {
                kind: ErrorKind::UnsafeFunctionCall,
                metadata: "needed()".to_owned(),
                parameterized_decorator: false,
            }],
            ..Default::default()
        });
        app.function_safety = fsmap([unsafe_missing_dep("wrapper", "dep.safe")]);
        let mut dep = safe_cached_module("dep", &[], &[]);
        dep.function_safety = fsmap([safe("needed"), safe("safe"), safe("unused")]);
        let mut cache = LibraryCache::empty();
        cache.modules = vec![app, dep];

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let CachedSafety::Ok(safety) = &cache.modules[0].safety else {
            panic!("app should have cached module safety");
        };
        assert!(safety.errors.is_empty());
        assert_eq!(
            cache.modules[0]
                .function_safety
                .get("wrapper")
                .map(|info| info.verdict),
            Some(FunctionSafety::Safe),
        );
    }

    #[test]
    fn test_reduce_strips_repeated_call_suffixes_from_error_metadata() {
        let mut app = safe_cached_module("app", &[], &[]);
        app.safety = CachedSafety::Ok(CachedModuleSafety {
            errors: vec![CachedError {
                kind: ErrorKind::UnsafeFunctionCall,
                metadata: "needed()()".to_owned(),
                parameterized_decorator: false,
            }],
            ..Default::default()
        });
        app.function_safety = fsmap([unsafe_missing_dep("wrapper", "dep.safe")]);
        let mut dep = safe_cached_module("dep", &[], &[]);
        dep.function_safety = fsmap([safe("needed"), safe("safe")]);
        let mut cache = LibraryCache::empty();
        cache.modules = vec![app, dep];

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let CachedSafety::Ok(safety) = &cache.modules[0].safety else {
            panic!("app should have cached module safety");
        };
        assert!(safety.errors.is_empty());
    }

    /// Injecting the bundled stubs rebuilds the `typing` <-> `typing_extensions`
    /// cycle, so an implicit `typing` import propagates onto `typing_extensions`.
    /// Without injection that propagation is lost.
    #[test]
    fn inject_bundled_stub_graph_restores_stub_cycle_propagation() {
        let options = Options {
            verbose_output_path: None,
            sorted_output: true,
            main_module: None,
            python_version: default_python_version(),
        };

        let make_cache = || {
            let mut cache = LibraryCache::empty();
            cache.modules.push(safe_cached_module(
                "typing_extensions",
                &["typing", "types"],
                &[],
            ));
            cache.modules.push(safe_cached_module(
                "consumer",
                &["typing_extensions"],
                &["typing"],
            ));
            cache
        };

        let te_inherits_typing = |analysis: &LifeGuardAnalysis| {
            analysis
                .output
                .lazy_eligible
                .get(&mn("typing_extensions"))
                .map(|e| e.value().contains(&mn("typing")))
                .unwrap_or(false)
        };

        // With injection: the cycle is rebuilt and `typing` propagates.
        let mut with = make_cache();
        let graph_only_stubs = with.inject_bundled_stub_graph(default_python_version());
        assert!(
            graph_only_stubs.contains(&mn("typing")) && graph_only_stubs.contains(&mn("types")),
            "bundled stubs typing/types should be injected as graph-only modules",
        );
        assert!(
            !graph_only_stubs.contains(&mn("typing_extensions")),
            "an already-present real module must not be overwritten by the stub graph",
        );
        let resolved = reduce_workspace_from_merged(with, graph_only_stubs.clone()).resolve();
        let analysis = LifeGuardAnalysis::from_resolved_cache(&resolved, &options);
        assert!(
            te_inherits_typing(&analysis),
            "with stub injection, typing_extensions should inherit `typing` via the rebuilt stub cycle",
        );
        assert!(
            !analysis.output.lazy_eligible.contains_key(&mn("typing")),
            "graph-only stub `typing` must not be emitted as an output key",
        );

        // Without injection: `typing` is not a node, so no propagation.
        let mut empty = graph_only_stubs;
        empty.clear();
        let without = make_cache();
        let resolved = reduce_workspace_from_merged(without, empty).resolve();
        let analysis = LifeGuardAnalysis::from_resolved_cache(&resolved, &options);
        assert!(
            !te_inherits_typing(&analysis),
            "without stub injection, typing_extensions should not inherit `typing`",
        );
    }

    #[test]
    fn test_reduce_keeps_unsafe_decorator_error_after_unrelated_promotion() {
        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: mn("app"),
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnsafeDecoratorCall,
                            metadata: "app.deco".to_owned(),
                            parameterized_decorator: true,
                        }],
                        ..Default::default()
                    }),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: fsmap([
                        safe("deco"),
                        unsafe_if_imported("deco.builder"),
                        unsafe_missing_dep("wrapper", "dep.safe"),
                    ]),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: mn("dep"),
                    safety: CachedSafety::Ok(CachedModuleSafety::default()),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: fsmap([safe("safe")]),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: empty_exports(),
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        let CachedSafety::Ok(safety) = &app.safety else {
            panic!("app should have cached module safety");
        };
        assert!(
            safety
                .errors
                .iter()
                .any(|e| e.kind == ErrorKind::UnsafeDecoratorCall && e.metadata == "app.deco"),
            "decorator errors need the call-site nested-function check, so a safe function verdict must not clear them",
        );
        assert_eq!(
            app.function_safety.get("wrapper").map(|i| i.verdict),
            Some(FunctionSafety::Safe),
            "the unrelated promotion should still run and trigger global error clearing",
        );
    }

    #[test]
    fn test_reduce_clears_bare_decorator_error_without_nested_function_check() {
        let mut cache = LibraryCache {
            modules: vec![CachedModule {
                name: mn("app"),
                safety: CachedSafety::Ok(CachedModuleSafety {
                    errors: vec![CachedError {
                        kind: ErrorKind::UnsafeDecoratorCall,
                        metadata: "app.deco".to_owned(),
                        parameterized_decorator: false,
                    }],
                    ..Default::default()
                }),
                imports: Default::default(),
                missing_imports: Default::default(),
                ambiguous_imports: Default::default(),
                side_effect_imports: Default::default(),
                function_safety: fsmap([safe("deco"), unsafe_if_imported("deco.unused_helper")]),
                mutation_candidates: Vec::new(),
            }],
            exports: empty_exports(),
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        let CachedSafety::Ok(safety) = &app.safety else {
            panic!("app should have cached module safety");
        };
        assert!(
            safety.errors.is_empty(),
            "bare decorators execute the decorator function itself, not every nested helper",
        );
    }

    #[test]
    fn test_reduce_clears_decorator_error_when_nested_functions_are_safe() {
        let mut cache = LibraryCache {
            modules: vec![
                CachedModule {
                    name: mn("app"),
                    safety: CachedSafety::Ok(CachedModuleSafety {
                        errors: vec![CachedError {
                            kind: ErrorKind::UnsafeDecoratorCall,
                            metadata: "app.deco".to_owned(),
                            parameterized_decorator: true,
                        }],
                        ..Default::default()
                    }),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: fsmap([
                        safe("deco"),
                        safe("deco.builder"),
                        unsafe_missing_dep("wrapper", "dep.safe"),
                    ]),
                    mutation_candidates: Vec::new(),
                },
                CachedModule {
                    name: mn("dep"),
                    safety: CachedSafety::Ok(CachedModuleSafety::default()),
                    imports: Default::default(),
                    missing_imports: Default::default(),
                    ambiguous_imports: Default::default(),
                    side_effect_imports: Default::default(),
                    function_safety: fsmap([safe("safe")]),
                    mutation_candidates: Vec::new(),
                },
            ],
            exports: empty_exports(),
            ..Default::default()
        };

        cache.resolve_cross_library_errors(MergedClassFacts::default());

        let app = module(&cache, "app");
        let CachedSafety::Ok(safety) = &app.safety else {
            panic!("app should have cached module safety");
        };
        assert!(
            safety.errors.is_empty(),
            "a decorator error verified safe together with its immediate nested functions should clear",
        );
    }

    #[test]
    fn graph_only_stub_ancestors_do_not_rewrite_implicit_imports() {
        let options = Options {
            verbose_output_path: None,
            sorted_output: true,
            main_module: None,
            python_version: default_python_version(),
        };

        let mut cache = LibraryCache::empty();
        cache.modules.push(safe_cached_module(
            "consumer",
            &["provider"],
            &["collections.abc"],
        ));
        cache
            .modules
            .push(safe_cached_module("provider", &["collections"], &[]));
        cache
            .modules
            .push(safe_cached_module("collections", &[], &[]));

        let graph_only_stubs = [mn("collections")].into_iter().collect();
        let resolved = reduce_workspace_from_merged(cache, graph_only_stubs).resolve();
        let analysis = LifeGuardAnalysis::from_resolved_cache(&resolved, &options);

        let consumer_deps = analysis
            .output
            .lazy_eligible
            .get(&mn("consumer"))
            .expect("consumer should be lazy-eligible");
        assert!(
            consumer_deps.value().contains(&mn("collections.abc")),
            "the direct implicit-import guard should keep the unresolved submodule",
        );

        let provider_deps = analysis
            .output
            .lazy_eligible
            .get(&mn("provider"))
            .expect("provider should be lazy-eligible");
        assert!(
            !provider_deps.value().contains(&mn("collections")),
            "graph-only stub ancestors should not receive propagated implicit-import guards",
        );
    }

    #[test]
    fn real_source_ancestors_do_not_rewrite_implicit_imports() {
        let options = Options {
            verbose_output_path: None,
            sorted_output: true,
            main_module: None,
            python_version: default_python_version(),
        };

        let mut cache = LibraryCache::empty();
        cache.modules.push(safe_cached_module(
            "consumer",
            &["provider"],
            &["torch.nn.functional"],
        ));
        cache
            .modules
            .push(safe_cached_module("provider", &["torch.nn"], &[]));
        cache.modules.push(safe_cached_module("torch", &[], &[]));
        cache.modules.push(safe_cached_module("torch.nn", &[], &[]));

        let graph_only_stubs = Default::default();
        let resolved = reduce_workspace_from_merged(cache, graph_only_stubs).resolve();
        let analysis = LifeGuardAnalysis::from_resolved_cache(&resolved, &options);

        let consumer_deps = analysis
            .output
            .lazy_eligible
            .get(&mn("consumer"))
            .expect("consumer should be lazy-eligible");
        assert!(
            consumer_deps.value().contains(&mn("torch.nn.functional")),
            "the direct implicit-import guard should keep the exact accessed submodule",
        );
        assert!(
            !consumer_deps.value().contains(&mn("torch.nn")),
            "a real source ancestor should not replace the exact implicit-import guard",
        );

        let provider_deps = analysis
            .output
            .lazy_eligible
            .get(&mn("provider"))
            .expect("provider should be lazy-eligible");
        assert!(
            !provider_deps.value().contains(&mn("torch.nn")),
            "real source ancestors should not receive propagated implicit-import guards",
        );
    }
}
