/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod common;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use clap::Parser;
    use lifeguard::commands::run_tree::RunTreeArgs;
    use lifeguard::commands::run_tree::run;

    use crate::common::populate_temp_dir;

    #[test]
    fn test_run_tree_resolves_cli_site_packages() {
        let tmp = populate_temp_dir(&[
            ("proj/main.py", "import foo\n"),
            ("sp/foo/__init__.py", ""),
            ("sp/bar/__init__.py", ""),
        ]);
        let proj = tmp.path().join("proj");
        let sp = tmp.path().join("sp");
        let output = tmp.path().join("out.json");

        let args = RunTreeArgs::try_parse_from([
            "run-tree",
            proj.to_str().unwrap(),
            output.to_str().unwrap(),
            "--site-packages",
            sp.to_str().unwrap(),
            "--sorted-output",
        ])
        .unwrap();
        run(args).unwrap();

        let content = fs::read_to_string(&output).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        let modules: BTreeSet<&str> = value["LAZY_ELIGIBLE"]
            .as_object()
            .expect("LAZY_ELIGIBLE object in output JSON")
            .keys()
            .map(|s| s.as_str())
            .collect();
        // `foo` appears only because --site-packages caused its resolution.
        // `bar` doesn't.
        assert_eq!(modules, BTreeSet::from(["main", "foo"]));
    }
}
