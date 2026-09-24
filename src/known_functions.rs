/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Everything the analyzer knows about code it never analyzes: stub effects,
//! builtins, and the manual safelist behind one query surface.
//!
//! `stub_analyzer` feeds this knowledge by parsing `.pyi` files; it runs
//! through `Stubs` and is never queried directly.

use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::name::Name;

use crate::builtins::Builtins;
use crate::manual_override;
use crate::stubs::Stubs;

/// Answers what the analyzer knows about a callee without analyzing source.
#[derive(Clone, Copy)]
pub(crate) struct KnownFunctions<'a> {
    stubs: &'a Stubs,
}

impl<'a> KnownFunctions<'a> {
    pub(crate) fn new(stubs: &'a Stubs) -> Self {
        Self { stubs }
    }

    /// The builtins view, for callers asking it more than one question.
    pub(crate) fn builtins(&self) -> Builtins<'a> {
        self.stubs.builtins()
    }

    /// Whether `method_name` is non-mutating on every builtin type defining it.
    pub(crate) fn is_method_safe_in_builtins(&self, method_name: &Name) -> bool {
        self.stubs.is_method_safe_in_builtins(method_name)
    }

    /// Whether `func` is on the manual safelist. Static data, not stub state.
    pub(crate) fn declared_safe(&self, func: &ModuleName) -> bool {
        manual_override::declared_safe(func)
    }
}
