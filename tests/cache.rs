/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use lifeguard::cache::CachedExports;
    use lifeguard::cache::CachedModule;
    use lifeguard::cache::CachedModuleSafety;
    use lifeguard::cache::CachedReExport;
    use lifeguard::cache::CachedSafety;
    use lifeguard::cache::LibraryCache;
    use lifeguard::cache::is_call_verified_safe;
    use lifeguard::cache::resolve_implicit_imports;
    use lifeguard::config::AnalysisConfig;
    use lifeguard::effects::ImportedArgs;
    use lifeguard::errors::ErrorKind;
    use lifeguard::errors::SafetyError;
    use lifeguard::exports::Exports;
    use lifeguard::hasher::HashMapExt;
    use lifeguard::imports::ImportGraph;
    use lifeguard::imports::resolve_to_known_module;
    use lifeguard::module_safety::FunctionSafety;
    use lifeguard::module_safety::FunctionSafetyInfo;
    use lifeguard::module_safety::ModuleSafety;
    use lifeguard::module_safety::MutationCandidate;
    use lifeguard::module_safety::MutationCandidateSite;
    use lifeguard::module_safety::SafetyResult;
    use lifeguard::output::LifeGuardAnalysis;
    use lifeguard::project;
    use lifeguard::project::SafetyMap;
    use lifeguard::project::SideEffectMap;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::runner::Options;
    use lifeguard::runner::default_python_version;
    use lifeguard::test_lib::TestSources;

    fn mn(s: &str) -> ModuleName {
        ModuleName::from_str(s)
    }

    fn build_cache(sources: &TestSources) -> LibraryCache {
        let config = AnalysisConfig::default();
        let (import_graph, exports) = ImportGraph::make_with_exports(sources, &config);
        let output = project::run_analysis(
            sources,
            &exports,
            &import_graph,
            &config,
            project::ExecutionMode::Incremental,
        );
        LibraryCache::build(
            &output.safety_map,
            &import_graph,
            &exports,
            &output.side_effect_imports,
        )
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
            function_safety: HashMap::new(),
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
        assert_eq!(std::mem::size_of::<LibraryCache>(), 120);
        assert_eq!(std::mem::size_of::<CachedModule>(), 280);
        assert_eq!(std::mem::size_of::<CachedSafety>(), 72);
        assert_eq!(std::mem::size_of::<CachedModuleSafety>(), 72);
        assert_eq!(std::mem::size_of::<lifeguard::cache::CachedError>(), 32);
        assert_eq!(std::mem::size_of::<CachedExports>(), 96);
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

        let foo = loaded.modules.iter().find(|m| m.name == mn("foo")).unwrap();
        assert!(matches!(&foo.safety, CachedSafety::Ok(s) if s.is_safe()));
        assert!(foo.imports.contains(&mn("bar")));

        let bar = loaded.modules.iter().find(|m| m.name == mn("bar")).unwrap();
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

        let broken = loaded
            .modules
            .iter()
            .find(|m| m.name == mn("broken"))
            .unwrap();
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

        let foo = cache.modules.iter().find(|m| m.name == mn("foo")).unwrap();
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

        let defs_mod = cache.modules.iter().find(|m| m.name == mn("defs")).unwrap();
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

        let exec_mod = loaded
            .modules
            .iter()
            .find(|m| m.name == mn("exec_mod"))
            .unwrap();
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

        let a = loaded.modules.iter().find(|m| m.name == mn("a")).unwrap();
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

        let dep_b = cache
            .modules
            .iter()
            .find(|m| m.name == mn("dep_b"))
            .unwrap();
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

        let dup = cache.modules.iter().find(|m| m.name == mn("dup")).unwrap();
        assert_eq!(
            dup.mutation_candidates,
            vec![candidate],
            "merge must preserve mutation candidates from a duplicate module copy",
        );
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
        let mut own_cache = build_cache(&own_sources);

        let caller_before = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (dep is missing)",
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller_after = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            caller_after.is_safe(),
            "caller should be safe after resolving cross-library constructor call",
        );
    }

    #[test]
    fn test_resolve_cross_library_unsafe_constructor() {
        let dep_cache = build_cache(&TestSources::new(&[
            (
                "dep",
                "import dep_state\n\
             class MyClass:\n\
             \x20   def __init__(self):\n\
             \x20       dep_state.counter = dep_state.counter + 1\n",
            ),
            ("dep_state", "counter = 0\n"),
        ]));

        let own_sources = TestSources::new(&[(
            "caller",
            "from dep import MyClass\n\
             instance = MyClass()\n",
        )]);
        let mut own_cache = build_cache(&own_sources);

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
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
        let dep_cache = build_cache(&TestSources::new(&[(
            "dep",
            "class MyClass:\n\
             \x20   def __init__(self, x):\n\
             \x20       x.attr = 1\n",
        )]));

        let mut own_cache = build_cache(&TestSources::new(&[
            ("config", "settings = 1\n"),
            (
                "caller",
                "from dep import MyClass\n\
                 from config import settings\n\
                 instance = MyClass(settings)\n",
            ),
        ]));

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
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

        let defs_mod = dep_cache
            .modules
            .iter()
            .find(|m| m.name == mn("defs"))
            .unwrap();
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
        let mut own_cache = build_cache(&own_sources);

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
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

        let lib = cache.modules.iter().find(|m| m.name == mn("lib")).unwrap();
        assert_eq!(
            lib.function_safety.get("bump").map(|i| i.verdict),
            Some(FunctionSafety::UnsafeIfImported),
            "bump mutates a module global, so it is UnsafeIfImported",
        );
        assert_eq!(
            lib.function_safety.get("helper").map(|i| i.verdict),
            Some(FunctionSafety::UnsafeIfImported),
            "helper calls bump within its own module, so it inherits UnsafeIfImported",
        );

        let other = cache
            .modules
            .iter()
            .find(|m| m.name == mn("other"))
            .unwrap();
        assert_eq!(
            other.function_safety.get("trigger").map(|i| i.verdict),
            Some(FunctionSafety::Unsafe),
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

        let m = cache.modules.iter().find(|x| x.name == mn("m")).unwrap();
        assert_eq!(
            m.function_safety.get("sink").map(|i| i.verdict),
            Some(FunctionSafety::Safe),
            "sink mutates its own parameter, which is safe in isolation",
        );
        assert_eq!(
            m.function_safety.get("f").map(|i| i.verdict),
            Some(FunctionSafety::Unsafe),
            "f passes an imported var to a mutated parameter, mutating imported state",
        );

        let app = cache.modules.iter().find(|x| x.name == mn("app")).unwrap();
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

        let m = cache.modules.iter().find(|x| x.name == mn("m")).unwrap();
        let sink = m
            .function_safety
            .get("sink")
            .expect("sink should have a function_safety entry");
        let param = sink
            .mutated_params
            .iter()
            .find(|param| param.name == mn("x"))
            .unwrap_or_else(|| panic!("sink should record mutated parameter x: {sink:?}"));
        assert_eq!(param.index, Some(0), "x should be positional index 0");
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

        let m = cache.modules.iter().find(|x| x.name == mn("m")).unwrap();
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

        let m = cache.modules.iter().find(|x| x.name == mn("m")).unwrap();
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

        let mut own_cache = build_cache(&TestSources::new(&[
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

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let m = own_cache
            .modules
            .iter()
            .find(|x| x.name == mn("m"))
            .unwrap();
        assert_eq!(
            m.function_safety.get("f").map(|i| i.verdict),
            Some(FunctionSafety::Unsafe),
            "f passes an imported var to a cross-library mutating parameter",
        );

        let app = own_cache
            .modules
            .iter()
            .find(|x| x.name == mn("app"))
            .unwrap();
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

        let mut own_cache = build_cache(&TestSources::new(&[
            ("config", "settings = 1\n"),
            (
                "main",
                "from setup import configure\n\
                 from config import settings\n\
                 configure(settings)\n",
            ),
        ]));

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let main = own_cache
            .modules
            .iter()
            .find(|x| x.name == mn("main"))
            .unwrap();
        assert!(
            !main.is_safe(),
            "main mutates the imported `settings` via cross-library `configure` at import time",
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

        let mut own_cache = build_cache(&TestSources::new(&[
            ("config", "settings = 1\n"),
            (
                "main",
                "from setup import configure\n\
                 from config import settings\n\
                 configure(settings)\n",
            ),
        ]));

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let main = own_cache
            .modules
            .iter()
            .find(|x| x.name == mn("main"))
            .unwrap();
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

        let mut own_cache = build_cache(&TestSources::new(&[
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

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let main = own_cache
            .modules
            .iter()
            .find(|x| x.name == mn("main"))
            .unwrap();
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

        let mut own_cache = build_cache(&TestSources::new(&[
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

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let main = own_cache
            .modules
            .iter()
            .find(|x| x.name == mn("main"))
            .unwrap();
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

        let mut own_cache = build_cache(&TestSources::new(&[
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

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let main = own_cache
            .modules
            .iter()
            .find(|x| x.name == mn("main"))
            .unwrap();
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
    fn test_resolve_implicit_imports_dotted_paths() {
        let known = [mn("dep"), mn("other")].into_iter().collect();

        let mut implicits = vec![mn("dep.ClassName"), mn("other"), mn("missing.Foo")];
        resolve_implicit_imports(&mut implicits, &known);

        assert_eq!(implicits, vec![mn("dep"), mn("other"), mn("missing.Foo")]);
    }

    #[test]
    fn test_resolve_implicit_imports_deduplicates() {
        let known = [mn("dep")].into_iter().collect();

        let mut implicits = vec![mn("dep.ClassA"), mn("dep.ClassB"), mn("dep")];
        resolve_implicit_imports(&mut implicits, &known);

        assert_eq!(implicits, vec![mn("dep")]);
    }

    #[test]
    fn test_precompute_function_safety_populates_all_functions() {
        let cache = build_cache(&TestSources::new(&[(
            "mod_a",
            "def helper(): return 1\ndef unused(): return 2\n",
        )]));

        let mod_a = cache
            .modules
            .iter()
            .find(|m| m.name == mn("mod_a"))
            .unwrap();
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
    fn test_propagate_re_export_replaces_conservative_verdict() {
        let mut cache = LibraryCache::empty();

        cache.modules.push(CachedModule {
            name: mn("c"),
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: Default::default(),
            missing_imports: Default::default(),
            ambiguous_imports: Default::default(),
            side_effect_imports: Default::default(),
            function_safety: HashMap::from([(
                "foo".to_string(),
                FunctionSafetyInfo::new(FunctionSafety::Safe),
            )]),
            mutation_candidates: Vec::new(),
        });

        cache.modules.push(CachedModule {
            name: mn("b"),
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: Default::default(),
            missing_imports: Default::default(),
            ambiguous_imports: Default::default(),
            side_effect_imports: Default::default(),
            function_safety: HashMap::from([(
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

        let b = cache.modules.iter().find(|m| m.name == mn("b")).unwrap();
        assert_eq!(
            b.function_safety.get("foo").map(|info| info.verdict),
            Some(FunctionSafety::Safe),
            "propagation should replace UnsafeMissingDep with Safe from source module",
        );
    }

    #[test]
    fn test_resolve_cross_library_function_call() {
        let dep_cache = build_cache(&TestSources::new(&[("dep", "def safe_func(): return 1\n")]));

        let mut own_cache = build_cache(&TestSources::new(&[(
            "caller",
            "from dep import safe_func\nx = safe_func()\n",
        )]));

        let caller_before = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (dep is missing)",
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller_after = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
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
            cache
                .modules
                .iter()
                .find(|m| m.name == mn("caller"))
                .unwrap()
                .missing_imports
                .is_empty(),
            "no missing imports",
        );

        cache.resolve_cross_library_errors();

        let caller = cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller.is_safe(),
            "errors from already-imported modules should not be cleared (conservative)",
        );
    }

    #[test]
    fn test_error_cleared_from_ambiguous_import() {
        let dep_cache = build_cache(&TestSources::new(&[
            ("pkg", ""),
            ("pkg.sub", "def helper(): return 1\n"),
        ]));

        let mut own_cache = build_cache(&TestSources::new(&[(
            "caller",
            "from pkg import sub\nx = sub.helper()\n",
        )]));

        let caller_before = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
        assert!(
            !caller_before.is_safe(),
            "caller should be unsafe before merge (pkg.sub is unresolved)",
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let caller = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("caller"))
            .unwrap();
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
    fn test_missing_dep_promotion_blocked_by_unsafe_callee() {
        let dep_cache = build_cache(&TestSources::new(&[("dep", "def g():\n    g()\n")]));

        let mut own_cache = build_cache(&TestSources::new(&[
            ("mid", "from dep import g\ndef f():\n    g()\n"),
            ("top", "from mid import f\nf()\n"),
        ]));

        assert!(
            !own_cache
                .modules
                .iter()
                .find(|m| m.name == mn("top"))
                .unwrap()
                .is_safe(),
            "top unsafe before merge (mid is missing)",
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let top = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("top"))
            .unwrap();
        assert!(
            !top.is_safe(),
            "top must stay unsafe: importing it runs f() -> unsafe g()",
        );
    }

    #[test]
    fn test_missing_dep_promotion_through_safe_callee() {
        let dep_cache = build_cache(&TestSources::new(&[("dep", "def g():\n    return 1\n")]));

        let mut own_cache = build_cache(&TestSources::new(&[
            ("mid", "from dep import g\ndef f():\n    g()\n"),
            ("top", "from mid import f\nf()\n"),
        ]));

        assert!(
            !own_cache
                .modules
                .iter()
                .find(|m| m.name == mn("top"))
                .unwrap()
                .is_safe(),
            "top unsafe before merge (mid is missing)",
        );

        own_cache.merge_dep_caches(vec![dep_cache]);
        own_cache.resolve_cross_library_errors();

        let top = own_cache
            .modules
            .iter()
            .find(|m| m.name == mn("top"))
            .unwrap();
        assert!(
            top.is_safe(),
            "top should be safe: f() only reaches the now-resolved safe g()",
        );
    }

    #[test]
    fn test_dotted_local_name_class_safety() {
        let mut fs = HashMap::new();
        fs.insert(
            "MyClass".to_string(),
            FunctionSafetyInfo::new(FunctionSafety::Safe),
        );
        let mut func_safety_by_module = HashMap::new();
        func_safety_by_module.insert(mn("dep"), fs);

        let resolved = [mn("dep")].into_iter().collect();

        assert!(
            is_call_verified_safe("dep.MyClass.method", &resolved, &func_safety_by_module),
            "dep.MyClass.method should resolve via class-level safety",
        );

        assert!(
            is_call_verified_safe("dep.MyClass", &resolved, &func_safety_by_module),
            "dep.MyClass should match directly",
        );

        assert!(
            !is_call_verified_safe("dep.OtherClass.method", &resolved, &func_safety_by_module),
            "dep.OtherClass.method should not match when OtherClass is not safe",
        );
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
        let analysis = LifeGuardAnalysis::from_cache(&mut with, &graph_only_stubs, &options);
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
        let mut without = make_cache();
        let analysis = LifeGuardAnalysis::from_cache(&mut without, &empty, &options);
        assert!(
            !te_inherits_typing(&analysis),
            "without stub injection, typing_extensions should not inherit `typing`",
        );
    }
}
