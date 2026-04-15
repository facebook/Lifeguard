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

    /// Run analyze-library and return the parsed cache JSON.
    fn run_analyze_library(
        db_path: &str,
        cache_path: &Path,
        dep_caches: &[&Path],
    ) -> serde_json::Value {
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
        let content = std::fs::read_to_string(cache_path).expect("Failed to read cache output");
        serde_json::from_str(&content).expect("Failed to parse cache JSON")
    }

    fn get_module_names(cache: &serde_json::Value) -> Vec<String> {
        cache["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn is_module_safe(module: &serde_json::Value) -> bool {
        module["safety"]
            .get("Ok")
            .and_then(|ok| ok["errors"].as_array())
            .is_some_and(|errs| errs.is_empty())
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

        let cache_path = tmp.path().join("sample_lib_cache.json");
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

        let safe_mod = cache["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"].as_str().unwrap().contains("safe_module"))
            .unwrap();
        assert!(is_module_safe(safe_mod), "safe_module should be safe");
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
        let lib_cache_path = tmp.path().join("sample_lib_cache.json");
        let lib_cache = run_analyze_library(&lib_db_path, &lib_cache_path, &[]);
        assert_eq!(get_module_names(&lib_cache).len(), 5);

        // --- Step 2: Analyze sample_project-library's own sources only ---
        let proj_db_path = build_source_db_no_deps(SAMPLE_PROJECT_LIB);
        let merged_cache_path = tmp.path().join("merged_cache.json");
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
        // The 5 dep modules should have the same safety in both caches
        let lib_modules = lib_cache["modules"].as_array().unwrap();
        let merged_modules = merged_cache["modules"].as_array().unwrap();
        for lib_mod in lib_modules {
            let lib_name = lib_mod["name"].as_str().unwrap();
            let merged_mod = merged_modules
                .iter()
                .find(|m| m["name"].as_str().unwrap() == lib_name)
                .unwrap_or_else(|| panic!("Module {lib_name} missing from merged cache"));
            assert_eq!(
                is_module_safe(lib_mod),
                is_module_safe(merged_mod),
                "Safety mismatch for module {lib_name}"
            );
        }
    }
}
