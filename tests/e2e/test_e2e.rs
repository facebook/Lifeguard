/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use lifeguard::test_lib::check_buck_availability;

    #[test]
    fn test_lifeguard_build_integration() {
        if !check_buck_availability() {
            return;
        }
        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                "test_lifeguard_build_integration",
                "run",
                "fbcode//safer_lazy_imports/automation:compare-strategy",
            ])
            .output()
            .expect("failed to execute buck2");
        if output.status.success() {
            return;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!(stderr.contains("py_lazy_import_analysis") && stderr.contains("Action failed")));
    }

    #[test]
    fn test_lifeguard_build_integration_with_local_changes() {
        if !check_buck_availability() {
            return;
        }
        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                "test_lifeguard_build_integration_with_local_changes",
                "run",
                "-c",
                "python.safer_lazy_imports_mode=build_local",
                "fbcode//safer_lazy_imports/automation:compare-strategy",
            ])
            .output()
            .expect("failed to execute buck2");
        if output.status.success() {
            return;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!(stderr.contains("py_lazy_import_analysis") && stderr.contains("Action failed")));
    }

    #[test]
    fn test_lifeguard_standalone() {
        // The standalone lifeguard script kicks off a buck build to gather the
        // db json, this creates a tangle of buck commands. Need to set an
        // isolation dir
        if !check_buck_availability() {
            return;
        }
        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                "test_lifeguard_standalone",
                "run",
                "fbcode//safer_lazy_imports/lifeguard:lifeguard",
                "--",
                "--target",
                "fbcode//safer_lazy_imports/lifeguard/testdata/sample_project:sample_project",
            ])
            .env("BUCK_ISOLATION_DIR", "gather_db_json")
            .output()
            .expect("failed to execute buck2");

        assert!(
            output.status.success(),
            "lifeguard standalone failed with exit code {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // -----------------------------------------------------------------------
    // Dependency cache merging tests
    // -----------------------------------------------------------------------

    use lifeguard::cache::CachedModule;
    use lifeguard::cache::CachedSafety;
    use lifeguard::cache::LibraryCache;

    const ISO_DIR: &str = "test_dep_cache_merge";
    const SAMPLE_LIB: &str =
        "fbcode//safer_lazy_imports/lifeguard/testdata/sample_project:sample_lib";
    const SAMPLE_PROJECT: &str =
        "fbcode//safer_lazy_imports/lifeguard/testdata/sample_project:sample_project";
    const SAMPLE_PROJECT_LIB: &str =
        "fbcode//safer_lazy_imports/lifeguard/testdata/sample_project:sample_project-library";
    const ANALYZER: &str = "fbcode//safer_lazy_imports/lifeguard/src:analyzer";

    /// Build the [source-db-no-deps] subtarget and return the path to the JSON file.
    fn build_source_db_no_deps(target: &str) -> String {
        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "build",
                &format!("{target}[source-db-no-deps]"),
                "--show-full-simple-output",
            ])
            .output()
            .expect("failed to execute buck2 build");
        assert!(
            output.status.success(),
            "source-db-no-deps build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Run analyze-library and return the parsed cache.
    fn run_analyze_library(db_path: &str, cache_path: &Path) -> LibraryCache {
        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "run",
                ANALYZER,
                "--",
                "analyze-library",
                db_path,
                cache_path.to_str().unwrap(),
            ])
            .output()
            .expect("failed to execute analyzer");
        assert!(
            output.status.success(),
            "analyze-library failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        LibraryCache::read_from_file(cache_path).expect("Failed to read cache output")
    }

    fn get_module_names(cache: &LibraryCache) -> Vec<String> {
        cache
            .modules
            .iter()
            .map(|m| m.name.as_str().to_string())
            .collect()
    }

    fn should_load_imports_eagerly(module: &CachedModule) -> bool {
        matches!(&module.safety, CachedSafety::Ok(s) if s.should_load_imports_eagerly())
    }

    fn find_module<'a>(cache: &'a LibraryCache, name_suffix: &str) -> &'a CachedModule {
        let dotted = format!(".{name_suffix}");
        let matches: Vec<_> = cache
            .modules
            .iter()
            .filter(|m| {
                let name = m.name.as_str();
                name == name_suffix || name.ends_with(&dotted)
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "Expected exactly 1 module matching '{name_suffix}', found: {:?}",
            matches.iter().map(|m| m.name.as_str()).collect::<Vec<_>>()
        );
        matches[0]
    }

    /// Test 1: Baseline — analyze sample_lib (no deps).
    /// Uses source-db-no-deps to get only sample_lib's own source files.
    #[test]
    fn test_dep_cache_baseline_sample_lib() {
        if !check_buck_availability() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let db_path = build_source_db_no_deps(SAMPLE_LIB);

        let cache_path = tmp.path().join("sample_lib_cache.bin");
        let cache = run_analyze_library(&db_path, &cache_path);

        let names = get_module_names(&cache);
        assert_eq!(names.len(), 6, "sample_lib cache should have 6 modules");

        for expected in &[
            "has_finalizer",
            "importer",
            "pkg.sub",
            "safe_module",
            "unsafe_module",
            "uses_exec",
        ] {
            assert!(
                names.iter().any(|n| n.contains(expected)),
                "Missing module: {expected}"
            );
        }

        let expectations: &[(&str, bool, bool)] = &[
            ("safe_module", true, false),
            ("unsafe_module", true, false),
            ("has_finalizer", false, true),
            ("uses_exec", false, true),
            ("importer", true, false),
        ];
        for &(name, expected_safe, expected_eager) in expectations {
            let m = find_module(&cache, name);
            assert_eq!(m.is_safe(), expected_safe, "{name}: safe mismatch");
            assert_eq!(
                should_load_imports_eagerly(m),
                expected_eager,
                "{name}: eager mismatch"
            );
        }
    }

    /// Test 2: analyze-library produces only the library's own modules.
    /// Merging is handled by analyze-binary in the reduce step.
    #[test]
    fn test_analyze_library_produces_own_modules_only() {
        if !check_buck_availability() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        let lib_db_path = build_source_db_no_deps(SAMPLE_LIB);
        let lib_cache_path = tmp.path().join("sample_lib_cache.bin");
        let lib_cache = run_analyze_library(&lib_db_path, &lib_cache_path);
        assert_eq!(get_module_names(&lib_cache).len(), 6);

        let proj_db_path = build_source_db_no_deps(SAMPLE_PROJECT_LIB);
        let proj_cache_path = tmp.path().join("proj_cache.bin");
        let proj_cache = run_analyze_library(&proj_db_path, &proj_cache_path);

        let proj_names = get_module_names(&proj_cache);

        assert_eq!(
            proj_names.len(),
            2,
            "analyze-library should only contain own modules (main + pkg)"
        );
        assert!(
            proj_names.iter().any(|n| n.contains("main")),
            "cache should contain main module"
        );
        assert!(
            proj_names.iter().any(|n| n.contains("pkg")),
            "cache should contain pkg module"
        );
    }

    /// Build the BXL source DB for a target and return the path to the merged_db.json file.
    fn build_bxl_source_db(target: &str) -> String {
        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "bxl",
                "fbcode//buck2/prelude/python/sourcedb/classic.bxl:build",
                "--",
                "--target",
                target,
            ])
            .output()
            .expect("failed to execute buck2 bxl");
        assert!(
            output.status.success(),
            "BXL build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let bxl: serde_json::Value =
            serde_json::from_str(&stdout).expect("Failed to parse BXL output");
        bxl["db"]
            .as_str()
            .expect("BXL output missing 'db' key")
            .to_string()
    }

    /// Run analyze-binary and return the parsed output JSON.
    fn run_analyze_binary(
        output_path: &std::path::Path,
        cache_paths: &[&std::path::Path],
    ) -> serde_json::Value {
        let manifest_dir = tempfile::tempdir().unwrap();
        let manifest_path = manifest_dir.path().join("cache-manifest.txt");
        let manifest_content: String = cache_paths
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&manifest_path, &manifest_content).expect("Failed to write cache manifest");
        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "run",
                ANALYZER,
                "--",
                "analyze-binary",
                output_path.to_str().unwrap(),
                "--sorted-output",
                "--cache-manifest",
                manifest_path.to_str().unwrap(),
            ])
            .output()
            .expect("failed to execute analyzer");
        assert!(
            output.status.success(),
            "analyze-binary failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let content = std::fs::read_to_string(output_path).expect("Failed to read output");
        serde_json::from_str(&content).expect("Failed to parse output JSON")
    }

    /// Run the baseline analyze command and return the parsed output JSON.
    /// The analyze command uses std::mem::forget which triggers LeakSanitizer
    /// in ASAN builds, so we return the exit status alongside the output.
    fn run_analyze_baseline(
        db_path: &str,
        output_path: &std::path::Path,
    ) -> (bool, serde_json::Value) {
        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "run",
                ANALYZER,
                "--",
                "analyze",
                db_path,
                output_path.to_str().unwrap(),
                "--sorted-output",
            ])
            .output()
            .expect("failed to execute analyzer");
        let content = std::fs::read_to_string(output_path)
            .expect("analyze command did not produce output file");
        let json = serde_json::from_str(&content).expect("Failed to parse output JSON");
        (output.status.success(), json)
    }

    /// Verify that the analyzer's JSON output contains correct safety verdicts
    /// for all modules in the sample project.
    #[test]
    fn test_output_correctness() {
        if !check_buck_availability() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        let lib_db_path = build_source_db_no_deps(SAMPLE_LIB);
        let lib_cache_path = tmp.path().join("sample_lib_cache.bin");
        run_analyze_library(&lib_db_path, &lib_cache_path);

        let proj_db_path = build_source_db_no_deps(SAMPLE_PROJECT_LIB);
        let proj_cache_path = tmp.path().join("proj_lib_cache.bin");
        run_analyze_library(&proj_db_path, &proj_cache_path);

        let output_path = tmp.path().join("output.json");
        let output = run_analyze_binary(&output_path, &[&lib_cache_path, &proj_cache_path]);

        let eager = output["LOAD_IMPORTS_EAGERLY"]
            .as_array()
            .expect("LOAD_IMPORTS_EAGERLY should be an array");
        let eager_names: Vec<&str> = eager.iter().filter_map(|v| v.as_str()).collect();

        for name in ["has_finalizer", "uses_exec"] {
            assert!(
                eager_names.iter().any(|n| {
                    let dotted = format!(".{name}");
                    n == &name || n.ends_with(&dotted)
                }),
                "LOAD_IMPORTS_EAGERLY should contain {name}, got: {eager_names:?}"
            );
        }
        for name in ["safe_module", "unsafe_module", "importer"] {
            assert!(
                !eager_names.iter().any(|n| {
                    let dotted = format!(".{name}");
                    n == &name || n.ends_with(&dotted)
                }),
                "LOAD_IMPORTS_EAGERLY should not contain {name}, got: {eager_names:?}"
            );
        }

        let eligible = output["LAZY_ELIGIBLE"]
            .as_object()
            .expect("LAZY_ELIGIBLE should be an object");

        for key in eligible.keys() {
            assert!(
                eligible[key].is_array(),
                "LAZY_ELIGIBLE value for {key} should be an array"
            );
        }
    }

    /// Verify analyze_binary produces the same output as a full baseline analyze.
    /// Each library cache contains only its own modules (non-cumulative).
    /// Analyze-binary receives all caches and merges them.
    #[test]
    fn test_analyze_binary_matches_baseline() {
        if !check_buck_availability() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        // Build per-library caches (non-cumulative)
        let lib_db_path = build_source_db_no_deps(SAMPLE_LIB);
        let lib_cache_path = tmp.path().join("sample_lib_cache.bin");
        run_analyze_library(&lib_db_path, &lib_cache_path);

        let proj_db_path = build_source_db_no_deps(SAMPLE_PROJECT_LIB);
        let proj_cache_path = tmp.path().join("proj_lib_cache.bin");
        run_analyze_library(&proj_db_path, &proj_cache_path);

        // Run analyze_binary with ALL library caches (merge happens here)
        let binary_output_path = tmp.path().join("binary_output.json");
        let binary_output =
            run_analyze_binary(&binary_output_path, &[&lib_cache_path, &proj_cache_path]);

        // Run baseline analyze on full source DB for comparison
        let full_db_path = build_bxl_source_db(SAMPLE_PROJECT_LIB);
        let baseline_output_path = tmp.path().join("baseline_output.json");
        let (baseline_ok, baseline_output) =
            run_analyze_baseline(&full_db_path, &baseline_output_path);

        // Only compare when baseline exited cleanly (LeakSanitizer in ASAN
        // builds causes non-zero exit and can produce incomplete output).
        if baseline_ok {
            assert_eq!(
                binary_output["LOAD_IMPORTS_EAGERLY"], baseline_output["LOAD_IMPORTS_EAGERLY"],
                "LOAD_IMPORTS_EAGERLY mismatch"
            );
            assert_eq!(
                binary_output["LAZY_ELIGIBLE"], baseline_output["LAZY_ELIGIBLE"],
                "LAZY_ELIGIBLE mismatch"
            );
        }
    }

    #[test]
    fn test_binary_without_analyzer_has_no_analysis_output() {
        if !check_buck_availability() {
            return;
        }

        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "build",
                SAMPLE_PROJECT,
                "--show-full-simple-output",
            ])
            .output()
            .expect("failed to execute buck2 build");
        assert!(
            output.status.success(),
            "binary build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "build",
                &format!("{SAMPLE_PROJECT}[dbg-source-db]"),
                "--show-full-simple-output",
            ])
            .output()
            .expect("failed to execute buck2 build");
        assert!(
            output.status.success(),
            "dbg-source-db sub_target failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn test_library_lazy_import_cache_subtarget_builds() {
        if !check_buck_availability() {
            return;
        }

        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "build",
                "-c",
                "python.use_lifeguard_incremental=true",
                "-c",
                "python.safer_lazy_imports_mode=build_local",
                &format!("{SAMPLE_LIB}[lazy-import-cache]"),
                "--show-full-simple-output",
            ])
            .output()
            .expect("failed to execute buck2 build");
        assert!(
            output.status.success(),
            "lazy-import-cache subtarget should build successfully:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let cache_path = String::from_utf8(output.stdout).unwrap();
        let cache_path = cache_path.trim();
        assert!(
            cache_path.ends_with("library-cache.bin"),
            "Expected library-cache.bin artifact, got: {cache_path}"
        );
        let cache = LibraryCache::read_from_file(Path::new(cache_path))
            .expect("Failed to read lazy-import-cache output");
        assert!(
            !cache.modules.is_empty(),
            "lazy-import-cache should contain at least one module"
        );
    }

    fn find_testdata_dir() -> std::path::PathBuf {
        let start = std::env::current_dir().expect("failed to get current dir");
        let search_paths = [
            "testdata/sample_project",
            "fbcode/safer_lazy_imports/lifeguard/testdata/sample_project",
        ];
        let mut dir = start.as_path();
        loop {
            for relative in &search_paths {
                let candidate = dir.join(relative);
                if candidate.exists() {
                    return candidate;
                }
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
        panic!(
            "Could not find testdata/sample_project in any ancestor of {}",
            start.display()
        );
    }

    /// Verify the gen-source-db subcommand produces a valid source DB JSON
    /// from a raw directory of Python files.
    #[test]
    fn test_gen_source_db() {
        if !check_buck_availability() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let output_path = tmp.path().join("source_db.json");

        let testdata_dir = find_testdata_dir();

        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                "test_gen_source_db",
                "run",
                ANALYZER,
                "--",
                "gen-source-db",
                &testdata_dir.to_string_lossy(),
                output_path.to_str().unwrap(),
            ])
            .output()
            .expect("failed to execute gen-source-db");

        assert!(
            output.status.success(),
            "gen-source-db failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        let content =
            std::fs::read_to_string(&output_path).expect("gen-source-db should produce output");
        let db: serde_json::Value =
            serde_json::from_str(&content).expect("output should be valid JSON");
        let build_map = db["build_map"]
            .as_object()
            .expect("output should have a build_map object");

        let expected_files = [
            "safe_module.py",
            "unsafe_module.py",
            "has_finalizer.py",
            "uses_exec.py",
            "importer.py",
            "main.py",
        ];
        assert!(
            build_map.len() >= expected_files.len(),
            "build_map should have at least {} entries, got {}",
            expected_files.len(),
            build_map.len()
        );

        for expected in &expected_files {
            assert!(
                build_map
                    .keys()
                    .any(|k| { k == *expected || k.ends_with(&format!("/{}", expected)) }),
                "build_map should contain {expected}, got keys: {:?}",
                build_map.keys().collect::<Vec<_>>()
            );
        }

        for (key, value) in build_map {
            let path_str = value.as_str().unwrap_or("");
            assert!(
                Path::new(path_str).is_absolute(),
                "build_map value for {key} should be an absolute path, got: {path_str}"
            );
        }
    }
}
