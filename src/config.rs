/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::CmpOp;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtIf;

use crate::pyrefly::sys_info::PythonVersion;
use crate::pyrefly::sys_info::SysInfo;
use crate::traits::SysInfoExt;

fn is_name_eq_main(name_side: &Expr, main_side: &Expr) -> bool {
    matches!(name_side, Expr::Name(n) if n.id.as_str() == "__name__")
        && matches!(main_side, Expr::StringLiteral(s) if s.value.to_str() == "__main__")
}

fn is_name_main_guard(expr: &Expr) -> bool {
    let Expr::Compare(cmp) = expr else {
        return false;
    };
    if cmp.ops.len() != 1 || cmp.ops[0] != CmpOp::Eq {
        return false;
    }
    let Some(right) = cmp.comparators.first() else {
        return false;
    };
    is_name_eq_main(cmp.left.as_ref(), right) || is_name_eq_main(right, cmp.left.as_ref())
}

#[derive(Debug, Clone)]
pub struct AnalysisConfig {
    pub sys_info: SysInfo,
    /// Which module is run as `__main__`, if known.
    /// - `None`: caller did not specify; `__main__` guards are treated as live.
    /// - `Some(M)` where M matches the current module: the body of
    ///   `if __name__ == "__main__"` is analyzed; in all other modules, those
    ///   bodies are pruned.
    /// - `Some("")`: sentinel for "no module runs as `__main__`" — every
    ///   `__main__` guard body is pruned everywhere. Used for python_binary
    ///   targets built with `main_function = ...`, where the entry file is
    ///   imported as a regular module and its `__main__` block is dead code.
    pub main_module: Option<ModuleName>,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            sys_info: SysInfo::lg_default(),
            main_module: None,
        }
    }
}

impl AnalysisConfig {
    pub fn new(sys_info: SysInfo, main_module: Option<ModuleName>) -> Self {
        Self {
            sys_info,
            main_module,
        }
    }

    pub fn with_python_version(
        python_version: PythonVersion,
        main_module: Option<ModuleName>,
    ) -> Self {
        Self {
            sys_info: SysInfo::lg_with_version(python_version),
            main_module,
        }
    }

    pub fn lg_pruned_if_branches<'a>(
        &'a self,
        x: &'a StmtIf,
        current_module: ModuleName,
    ) -> impl Iterator<Item = (Option<&'a Expr>, &'a [Stmt])> + 'a {
        let main_module = self.main_module;
        let mut done = false;
        self.sys_info
            .pruned_if_branches(x)
            .filter_map(move |(test, body)| {
                if done {
                    return None;
                }
                // A `__main__` guard: keep its body (unguarded) only in the module
                // that runs as __main__; prune it everywhere else.
                if let Some(t) = test
                    && let Some(main) = main_module
                    && is_name_main_guard(t)
                {
                    return if main == current_module {
                        done = true;
                        Some((None, body))
                    } else {
                        None
                    };
                }
                Some((test, body))
            })
    }
}
