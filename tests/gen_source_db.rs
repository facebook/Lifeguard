/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use clap::Parser;
    use lifeguard::commands::gen_source_db::GenSourceDbArgs;
    use lifeguard::commands::gen_source_db::run;
    use lifeguard::test_lib::populate_temp_dir;
    use serde_json::Value;

    #[test]
    fn test_gen_source_db_writes_build_map_json() {
        let tmp = populate_temp_dir(&[
            // Site-packages location is discovered via pyproject.toml rather
            // than passed on the CLI.
            (
                "proj/pyproject.toml",
                "[lifeguard]\nsite_packages = \"../sp\"\n",
            ),
            ("proj/m.py", "import other\n"),
            ("sp/other/__init__.py", ""),
        ]);
        let proj = tmp.path().join("proj");
        let output = tmp.path().join("db.json");

        let args = GenSourceDbArgs::try_parse_from([
            "gen-source-db",
            proj.to_str().unwrap(),
            output.to_str().unwrap(),
        ])
        .unwrap();
        run(args).unwrap();

        let content = fs::read_to_string(&output).unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        let build_map = value["build_map"]
            .as_object()
            .expect("build_map object in output JSON");
        let keys: BTreeSet<&str> = build_map.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, BTreeSet::from(["m.py", "other/__init__.py"]));
    }
}
