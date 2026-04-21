/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use lifeguard::find_sources::build_source_db;
    use lifeguard::test_lib::populate_temp_dir;

    fn keys(build_map: &std::collections::BTreeMap<String, String>) -> BTreeSet<&str> {
        build_map.keys().map(|s| s.as_str()).collect()
    }

    #[test]
    fn test_seeds_py_files_from_input_dir() {
        let tmp = populate_temp_dir(&[("a.py", ""), ("pkg/__init__.py", ""), ("pkg/b.py", "")]);

        let (build_map, seed_count) = build_source_db(tmp.path(), None).unwrap();
        assert_eq!(seed_count, 3);
        assert_eq!(
            keys(&build_map),
            BTreeSet::from(["a.py", "pkg/__init__.py", "pkg/b.py"]),
        );
    }

    #[test]
    fn test_skips_non_identifier_names() {
        let tmp = populate_temp_dir(&[
            ("good.py", ""),
            // Dir whose name is not a valid identifier: skipped wholesale.
            (".venv/bad.py", ""),
            // File whose stem is not a valid identifier: skipped.
            ("2024-migration.py", ""),
        ]);

        let (build_map, _) = build_source_db(tmp.path(), None).unwrap();
        assert_eq!(keys(&build_map), BTreeSet::from(["good.py"]));
    }

    #[test]
    fn test_follows_imports_into_site_packages() {
        let tmp = populate_temp_dir(&[
            ("proj/main.py", "import foo\n"),
            ("sp/foo/__init__.py", ""),
            // Unreachable from main.py's imports — must not be pulled in.
            ("sp/foo/helper.py", ""),
            ("sp/unused/__init__.py", ""),
        ]);
        let proj = tmp.path().join("proj");
        let sp = tmp.path().join("sp");

        let (build_map, seed_count) = build_source_db(&proj, Some(&sp)).unwrap();
        assert_eq!(seed_count, 1, "only main.py is seeded from the project");
        assert_eq!(
            keys(&build_map),
            BTreeSet::from(["main.py", "foo/__init__.py"]),
        );
    }
}
