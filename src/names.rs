/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use pyrefly_python::module_name::ModuleName;

use crate::traits::ModuleNameExt;

/// The module a dotted name is defined in, and the name within it: the longest
/// prefix `is_module` accepts, paired with the remainder.
///
/// Yields `None` for a name with no dot, or with no prefix satisfying is_module.
pub(crate) fn enclosing_module<'a>(
    name: &'a str,
    is_module: impl Fn(&ModuleName) -> bool,
) -> Option<(ModuleName, &'a str)> {
    ModuleName::from_str(name)
        .iter_parents()
        .find(|(parent, _)| is_module(parent))
        .map(|(module, dot_pos)| (module, &name[dot_pos + 1..]))
}

/// As [`enclosing_module`], but testing candidate ancestors as string slices.
///
/// `ModuleName::from_str` interns, so probing with it pays the global interner for
/// every ancestor tried, and leaves a permanent entry behind for each one that is
/// not a module. A caller splitting many names against one module set should build
/// a `&str` view of that set once and probe it instead; only the module it settles
/// on is interned.
pub(crate) fn enclosing_module_str<'a>(
    name: &'a str,
    is_module: impl Fn(&str) -> bool,
) -> Option<(ModuleName, &'a str)> {
    let mut end = name.len();
    while let Some(pos) = name[..end].rfind('.') {
        end = pos;
        if is_module(&name[..pos]) {
            return Some((ModuleName::from_str(&name[..pos]), &name[pos + 1..]));
        }
    }
    None
}
