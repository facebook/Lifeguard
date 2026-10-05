/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;

use crate::effects::Effect;
use crate::effects::EffectKind;
use crate::effects::EffectTable;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;

// Map of the scope where modules are imported (ie function or class name) to the imported module names
pub type ModuleImportsMap = AHashMap<ModuleName, AHashSet<ModuleName>>;

#[derive(Debug)]
pub struct ModuleEffects {
    // Accumulate analysis output
    pub effects: EffectTable,

    // Errors encountered when analyzing a module or stub file
    pub file_errors: Vec<FileError>,

    // Whether the traversal is currently inside an `if __name__ == "__main__"`
    // body. Every effect added while it is set is marked as guard-produced.
    in_main_guard: bool,

    // map of where imported modules are called (ie function or class name) to the imported module names
    pub called_imports: ModuleImportsMap,

    // map of where import state is defined (ie function or class name) to the modules that are imported
    pub pending_imports: ModuleImportsMap,

    // Set of all pending import names across all scopes, for O(1) membership checks.
    pub all_pending_import_names: AHashSet<ModuleName>,

    // Set of all called import names across all scopes, for O(1) membership checks.
    pub all_called_import_names: AHashSet<ModuleName>,

    // list of functions called in the module
    pub called_functions: AHashSet<ModuleName>,

    // map of methods called on indirectly imported objects to its canonical value
    // i.e if we have import A as C and C.foo() is called we should map C.foo() to A.foo()
    // we get the canonical value using the re_exports table
    pub indirectly_called_methods: AHashMap<ModuleName, ModuleName>,

    // Modules imported without use of the `lazy` keyword. Used to distinguish
    // side-effect imports from explicit lazy imports that have no effect when unused.
    pub eager_imports: AHashSet<ModuleName>,

    // The public names the module's own `.pyi` declares when its source binds none of them,
    // and where to report them.
    pub names_only_in_stub: Option<(Vec<Name>, TextRange)>,
}

impl ModuleEffects {
    pub fn new() -> Self {
        Self {
            in_main_guard: false,
            effects: EffectTable::empty(),
            file_errors: Vec::new(),
            called_imports: AHashMap::new(),
            pending_imports: AHashMap::new(),
            all_pending_import_names: AHashSet::new(),
            all_called_import_names: AHashSet::new(),
            called_functions: AHashSet::new(),
            indirectly_called_methods: AHashMap::new(),
            eager_imports: AHashSet::new(),
            names_only_in_stub: None,
        }
    }

    pub fn add_effect(&mut self, scope: ModuleName, mut eff: Effect) {
        eff.from_main_guard = self.in_main_guard;
        self.effects.insert(scope, eff);
    }

    /// Mark what follows as guard-produced, returning the state to restore.
    ///
    /// `|=` rather than `=` so an ordinary `if` nested inside a guard body does
    /// not un-mark it; the caller restores rather than clearing for the same
    /// reason.
    pub fn enter_main_guard(&mut self, guarded: bool) -> bool {
        let outer = self.in_main_guard;
        self.in_main_guard |= guarded;
        outer
    }

    pub fn leave_main_guard(&mut self, outer: bool) {
        self.in_main_guard = outer;
    }

    pub fn add_file_error(&mut self, error: String, range: TextRange) {
        let err = FileError { error, range };
        self.file_errors.push(err);
    }

    pub fn add_pending_import(&mut self, import: ModuleName, scope: &ModuleName) {
        self.pending_imports
            .entry(*scope)
            .or_default()
            .insert(import);
        self.all_pending_import_names.insert(import);
    }

    pub fn add_called_import(&mut self, import: ModuleName, scope: &ModuleName) {
        self.called_imports
            .entry(*scope)
            .or_default()
            .insert(import);
        self.all_called_import_names.insert(import);
    }

    pub fn scope_has_effect(&self, scope: &ModuleName, kind: EffectKind) -> bool {
        self.effects
            .get(scope)
            .is_some_and(|effects| effects.iter().any(|effect| effect.kind == kind))
    }
}

// Struct to report errors encountered by the analyzer. These are unstructured error messages with
// an optional text range, intended to be human- rather than machine-readable.
#[derive(Debug)]
pub struct FileError {
    pub error: String,
    pub range: TextRange,
}
