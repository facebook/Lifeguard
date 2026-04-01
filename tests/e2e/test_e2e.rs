/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
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
}
