/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use ahash::AHashMap;
use ahash::AHashSet;
use pyrefly_python::module_name::ModuleName;
use ruff_text_size::TextRange;

use crate::effects::Effect;
use crate::effects::EffectTable;

// Map of the scope where modules are imported (ie function or class name) to the imported module names
pub type ModuleImportsMap = AHashMap<ModuleName, AHashSet<ModuleName>>;

#[derive(Debug)]
pub struct ModuleEffects {
    // Accumulate analysis output
    pub effects: EffectTable,

    // Errors encountered when analyzing a module or stub file
    pub file_errors: Vec<FileError>,

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
}

impl ModuleEffects {
    pub fn new() -> Self {
        Self {
            effects: EffectTable::empty(),
            file_errors: Vec::new(),
            called_imports: AHashMap::new(),
            pending_imports: AHashMap::new(),
            all_pending_import_names: AHashSet::new(),
            all_called_import_names: AHashSet::new(),
            called_functions: AHashSet::new(),
            indirectly_called_methods: AHashMap::new(),
        }
    }

    pub fn add_effect(&mut self, scope: ModuleName, eff: Effect) {
        self.effects.insert(scope, eff);
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
}

// Struct to report errors encountered by the analyzer. These are unstructured error messages with
// an optional text range, intended to be human- rather than machine-readable.
#[derive(Debug)]
pub struct FileError {
    pub error: String,
    pub range: TextRange,
}
