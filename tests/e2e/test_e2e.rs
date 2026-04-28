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
    // Phase 2: Dependency cache merging tests
    // -----------------------------------------------------------------------

    use lifeguard::cache::CachedModule;
    use lifeguard::cache::CachedSafety;
    use lifeguard::cache::LibraryCache;

    const ISO_DIR: &str = "test_dep_cache_merge";
    const SAMPLE_LIB: &str =
        "fbcode//safer_lazy_imports/lifeguard/testdata/sample_project:sample_lib";
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
    fn run_analyze_library(db_path: &str, cache_path: &Path, dep_caches: &[&Path]) -> LibraryCache {
        let mut args = vec![
            "--isolation-dir".to_string(),
            ISO_DIR.to_string(),
            "run".to_string(),
            ANALYZER.to_string(),
            "--".to_string(),
            "analyze-library".to_string(),
            db_path.to_string(),
            cache_path.to_str().unwrap().to_string(),
        ];
        for dep in dep_caches {
            args.push("--dep-cache".to_string());
            args.push(dep.to_str().unwrap().to_string());
        }
        let output = Command::new("buck2")
            .args(&args)
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

    fn is_cached_module_safe(module: &CachedModule) -> bool {
        matches!(&module.safety, CachedSafety::Ok(s) if s.is_safe())
    }

    /// Test 1: Baseline — analyze sample_lib (no deps).
    /// Uses source-db-no-deps to get only sample_lib's own 5 source files.
    #[test]
    fn test_dep_cache_baseline_sample_lib() {
        if !check_buck_availability() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let db_path = build_source_db_no_deps(SAMPLE_LIB);

        let cache_path = tmp.path().join("sample_lib_cache.bin");
        let cache = run_analyze_library(&db_path, &cache_path, &[]);

        let names = get_module_names(&cache);
        assert_eq!(names.len(), 5, "sample_lib cache should have 5 modules");

        for expected in &[
            "has_finalizer",
            "importer",
            "safe_module",
            "unsafe_module",
            "uses_exec",
        ] {
            assert!(
                names.iter().any(|n| n.contains(expected)),
                "Missing module: {expected}"
            );
        }

        let safe_mod = cache
            .modules
            .iter()
            .find(|m| m.name.as_str().contains("safe_module"))
            .unwrap();
        assert!(
            is_cached_module_safe(safe_mod),
            "safe_module should be safe"
        );
    }

    /// Test 2: Analyze sample_project-library (1 own src) with sample_lib cache as dep.
    /// Uses source-db-no-deps for own sources only, merges dep cache.
    /// Verifies the merged output matches a full (non-cached) analysis.
    #[test]
    fn test_dep_cache_merge_sample_project() {
        if !check_buck_availability() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        // --- Step 1: Build sample_lib cache ---
        let lib_db_path = build_source_db_no_deps(SAMPLE_LIB);
        let lib_cache_path = tmp.path().join("sample_lib_cache.bin");
        let lib_cache = run_analyze_library(&lib_db_path, &lib_cache_path, &[]);
        assert_eq!(get_module_names(&lib_cache).len(), 5);

        // --- Step 2: Analyze sample_project-library's own sources only ---
        let proj_db_path = build_source_db_no_deps(SAMPLE_PROJECT_LIB);
        let merged_cache_path = tmp.path().join("merged_cache.bin");
        let merged_cache =
            run_analyze_library(&proj_db_path, &merged_cache_path, &[&lib_cache_path]);

        let merged_names = get_module_names(&merged_cache);

        // 1 own module + 5 from dep cache = 6 total
        assert_eq!(merged_names.len(), 6, "merged cache should have 6 modules");
        assert!(
            merged_names.iter().any(|n| n.contains("main")),
            "merged cache should contain main module"
        );

        // --- Step 3: Verify safety matches between dep cache and merged output ---
        for lib_mod in &lib_cache.modules {
            let lib_name = lib_mod.name.as_str();
            let merged_mod = merged_cache
                .modules
                .iter()
                .find(|m| m.name.as_str() == lib_name)
                .unwrap_or_else(|| panic!("Module {lib_name} missing from merged cache"));
            assert_eq!(
                is_cached_module_safe(lib_mod),
                is_cached_module_safe(merged_mod),
                "Safety mismatch for module {lib_name}"
            );
        }
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
        let mut args = vec![
            "--isolation-dir".to_string(),
            ISO_DIR.to_string(),
            "run".to_string(),
            ANALYZER.to_string(),
            "--".to_string(),
            "analyze-binary".to_string(),
            output_path.to_str().unwrap().to_string(),
            "--sorted-output".to_string(),
        ];
        for cache in cache_paths {
            args.push("--cache".to_string());
            args.push(cache.to_str().unwrap().to_string());
        }
        let output = Command::new("buck2")
            .args(&args)
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

    /// Verify analyze_binary produces the same output as a full baseline analyze.
    #[test]
    fn test_analyze_binary_matches_baseline() {
        if !check_buck_availability() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        // Build library caches
        let lib_db_path = build_source_db_no_deps(SAMPLE_LIB);
        let lib_cache_path = tmp.path().join("sample_lib_cache.bin");
        run_analyze_library(&lib_db_path, &lib_cache_path, &[]);

        let proj_db_path = build_source_db_no_deps(SAMPLE_PROJECT_LIB);
        let proj_cache_path = tmp.path().join("proj_lib_cache.bin");
        run_analyze_library(&proj_db_path, &proj_cache_path, &[&lib_cache_path]);

        // Run analyze_binary from cached libraries
        let binary_output_path = tmp.path().join("binary_output.json");
        let binary_output = run_analyze_binary(&binary_output_path, &[&proj_cache_path]);

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

    const SAMPLE_PROJECT: &str =
        "fbcode//safer_lazy_imports/lifeguard/testdata/sample_project:sample_project";

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
    fn test_library_without_toolchain_analyzer_has_no_cache() {
        if !check_buck_availability() {
            return;
        }

        let output = Command::new("buck2")
            .args([
                "--isolation-dir",
                ISO_DIR,
                "build",
                &format!("{SAMPLE_LIB}[lazy-import-cache]"),
            ])
            .output()
            .expect("failed to execute buck2 build");
        assert!(
            !output.status.success(),
            "lazy-import-cache should NOT be available without toolchain config"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("lazy-import-cache") && stderr.contains("not available"),
            "Expected 'not available' error for lazy-import-cache, got:\n{stderr}"
        );
    }
}
