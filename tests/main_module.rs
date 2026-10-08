/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use clap::Parser;
    use lifeguard::commands::analyze;
    use lifeguard::commands::analyze::AnalyzeArgs;
    use lifeguard::commands::analyze_binary;
    use lifeguard::commands::analyze_binary::AnalyzeBinaryArgs;
    use lifeguard::commands::analyze_library;
    use lifeguard::commands::analyze_library::AnalyzeLibraryArgs;
    use lifeguard::runner::check_main_module;
    use lifeguard::source_map::ModuleName;
    use lifeguard::test_lib::populate_temp_dir;
    use serde_json::Value;

    const APP: &str =
        "def _run():\n    raise RuntimeError('boom')\n\nif __name__ == '__main__':\n    _run()\n";

    /// A test runner's entry module has no cache in the reduce, and a thrift `-remote` script's
    /// source file has no `.py` extension: neither may fail the build.
    #[test]
    fn an_unknown_main_module_prunes_every_guard_like_the_empty_sentinel() {
        let tmp = populate_temp_dir(&[("app.py", APP)]);
        let dir = tmp.path();
        let db = dir.join("db.json");
        fs::write(
            &db,
            serde_json::json!({ "app.py": dir.join("app.py") }).to_string(),
        )
        .unwrap();
        let cache = dir.join("app.bin");
        analyze_library::run(
            AnalyzeLibraryArgs::try_parse_from(["analyze-library", path(&db), path(&cache)])
                .unwrap(),
        )
        .unwrap();
        fs::write(dir.join("manifest.txt"), path(&cache)).unwrap();

        for run in [whole_program, incremental] {
            assert!(
                !lazy_eligible(&run(dir, "app"), "app"),
                "the entry module runs its guard",
            );
            let unknown = run(dir, "test.runner");
            assert!(
                lazy_eligible(&unknown, "app"),
                "no analyzed module runs its guard"
            );
            assert_eq!(unknown, run(dir, ""));
        }
    }

    #[test]
    fn check_main_module_rejects_only_a_nonempty_unknown_name() {
        let known = |name: ModuleName| name == ModuleName::from_str("app");
        assert!(check_main_module(Some(ModuleName::from_str("app")), known).is_ok());
        assert!(check_main_module(Some(ModuleName::from_str("")), known).is_ok());
        assert!(check_main_module(None, known).is_ok());
        assert!(check_main_module(Some(ModuleName::from_str("test.runner")), known).is_err());
    }

    fn whole_program(dir: &Path, main_module: &str) -> Value {
        let output = dir.join("whole_program.json");
        let db = dir.join("db.json");
        analyze::run(
            AnalyzeArgs::try_parse_from([
                "analyze",
                path(&db),
                path(&output),
                "--sorted-output",
                "--main-module",
                main_module,
            ])
            .unwrap(),
        )
        .unwrap();
        read_json(&output)
    }

    fn incremental(dir: &Path, main_module: &str) -> Value {
        let output = dir.join("incremental.json");
        let manifest = dir.join("manifest.txt");
        analyze_binary::run(
            AnalyzeBinaryArgs::try_parse_from([
                "analyze-binary",
                path(&output),
                "--cache-manifest",
                path(&manifest),
                "--sorted-output",
                "--main-module",
                main_module,
            ])
            .unwrap(),
        )
        .unwrap();
        read_json(&output)
    }

    fn lazy_eligible(output: &Value, module: &str) -> bool {
        output["LAZY_ELIGIBLE"]
            .as_object()
            .expect("LAZY_ELIGIBLE object in output JSON")
            .contains_key(module)
    }

    fn path(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }
}
