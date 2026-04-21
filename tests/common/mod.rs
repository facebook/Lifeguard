/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;

use tempfile::TempDir;

/// Create a new temp directory and write each `(rel_path, contents)` pair
/// into it, creating intermediate directories as needed. The returned
/// [`TempDir`] owns the path and deletes it on drop.
pub fn populate_temp_dir(files: &[(&str, &str)]) -> TempDir {
    let tmp = TempDir::new().expect("create temp dir");
    for (rel, contents) in files {
        let path = tmp.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&path, contents).expect("write file");
    }
    tmp
}
