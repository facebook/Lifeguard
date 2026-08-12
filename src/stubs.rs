/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::sync::OnceLock;

use pyrefly_python::module_name::ModuleName;
use ruff_python_ast::name::Name;
use starlark_map::small_map::SmallMap;

use crate::analyzer::AnalyzedModule;
use crate::builtins::Builtins;
use crate::effects::EffectKind;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::stub_analyzer;

/// A lazily initialized map of parsed stubs. Stores the text of the stub files in the `raw` map on
/// creation, and then parses the stub into an AnalyzedModule the first time it's accessed.
#[derive(Debug)]
pub struct Stubs {
    raw: SmallMap<ModuleName, String>,
    parsed: SmallMap<ModuleName, OnceLock<AnalyzedModule>>,
    /// Modules whose stub came from an `__init__.pyi` file, i.e. packages. Needed
    /// so relative imports (`from .sub import ...`) in package stubs resolve
    /// against the package itself rather than its parent.
    init_modules: AHashSet<ModuleName>,
    /// Memoizes `is_method_safe_in_builtins`, whose scan covers every builtin
    /// function scope and returns the same answer at every call site.
    safe_builtin_methods: OnceLock<AHashMap<Name, bool>>,
}

impl Stubs {
    pub fn new() -> Self {
        let bundle = lifeguard_stubs::bundled_stubs().unwrap();
        let mut raw = SmallMap::new();
        let mut parsed = SmallMap::new();
        let mut init_modules = AHashSet::new();
        for (path, val) in bundle {
            let key = ModuleName::from_relative_path(&path).unwrap();
            if path.file_name() == Some(OsStr::new("__init__.pyi")) {
                init_modules.insert(key);
            }
            raw.insert(key, val);
            parsed.insert(key, OnceLock::new());
        }
        Self {
            raw,
            parsed,
            init_modules,
            safe_builtin_methods: OnceLock::new(),
        }
    }

    /// Whether the stub for `key` is a package (came from an `__init__.pyi` file).
    pub fn is_init(&self, key: &ModuleName) -> bool {
        self.init_modules.contains(key)
    }

    /// Get the analysis output for a stub module, running the analysis if it hasn't happened yet.
    pub fn get(&self, key: &ModuleName) -> Option<&AnalyzedModule> {
        let raw = self.raw.get(key)?;
        let parsed = self.parsed.get(key)?;
        let is_init = self.is_init(key);
        let ret = parsed.get_or_init(|| stub_analyzer::analyze_str(*key, raw, is_init, self));
        Some(ret)
    }

    /// Get an iterator to the name and contents of the raw stub sources.
    pub fn raw_sources_iter(&self) -> impl Iterator<Item = (&ModuleName, &String)> {
        self.raw.iter()
    }

    /// Get the raw source text for a stub module by name.
    pub fn get_raw_source(&self, key: &ModuleName) -> Option<&str> {
        self.raw.get(key).map(|s| s.as_str())
    }

    /// Get the analysis output for the builtins module, running it if it hasn't happened yet.
    pub fn builtins(&self) -> Builtins<'_> {
        // We should panic if builtins is not in the stubs, so unwrap is fine here.
        Builtins::new(self.get(&ModuleName::builtins()).unwrap())
    }

    /// Check whether a method name is safe (non-mutating) across all builtin types
    /// that define it. Returns true if the method is defined in at least one
    /// builtin type and none of those definitions have a Mutation effect.
    ///
    /// Methods annotated with `no_effects()` in stubs are removed from the
    /// effects table during stub analysis, so we check the definitions table
    /// to find all builtin methods and then verify none of them are mutating.
    pub fn is_method_safe_in_builtins(&self, method_name: &Name) -> bool {
        let safe = match self.safe_builtin_methods.get() {
            Some(safe) => safe,
            None => {
                // Resolved before entering `get_or_init`, not inside it. `get`
                // runs the stub analysis, which is handed `self`; a stub that
                // asked this same question mid-analysis can re-enter the
                // `OnceLock`, and re-entrant initialization deadlocks.
                let builtins = self.get(&ModuleName::builtins());
                self.safe_builtin_methods
                    .get_or_init(|| Self::build_safe_builtin_methods(builtins))
            }
        };
        safe.get(method_name).copied().unwrap_or(false)
    }

    /// Method name -> whether every builtin definition of it is non-mutating.
    /// Absent means no builtin type defines the name.
    fn build_safe_builtin_methods(builtins: Option<&AnalyzedModule>) -> AHashMap<Name, bool> {
        let mut safe = AHashMap::default();
        let Some(builtins) = builtins else {
            return safe;
        };
        for func in builtins.definitions.function_scopes() {
            // Last component of a qualified scope. Usually a method
            // (`builtins.list.append`), but module-level and nested functions
            // (`builtins.print`) reach here too and are treated the same way.
            let Some((_, method)) = func.as_str().rsplit_once('.') else {
                continue;
            };
            let mutates = builtins
                .module_effects
                .effects
                .get(func)
                .is_some_and(|effects| effects.iter().any(|e| e.kind == EffectKind::Mutation));
            *safe.entry(Name::new(method)).or_insert(true) &= !mutates;
        }
        safe
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use lifeguard_stubs;
    use ruff_python_ast::name::Name;

    use super::*;

    #[test]
    fn test_bundled_stubs() {
        let bundle = lifeguard_stubs::bundled_stubs().unwrap();
        let key = PathBuf::from("builtins.pyi");
        assert!(bundle.contains_key(&key));
        let builtins = bundle.get(&key).unwrap();
        assert!(builtins.contains("class filter"));
    }

    #[test]
    fn test_lazy_parsing() {
        let stubs = Stubs::new();
        let key = ModuleName::from_str("builtins");
        assert!(stubs.raw.contains_key(&key));
        assert!(stubs.parsed.contains_key(&key));
        // parsed value is uninitialized
        assert!(stubs.parsed.get(&key).unwrap().get().is_none());
        // gets and initializes the map entry
        let stub = stubs.get(&key);
        assert!(stub.is_some());
        assert!(stubs.parsed.get(&key).unwrap().get().is_some());
    }

    #[test]
    fn test_builtins_lookup() {
        let stubs = Stubs::new();
        let builtins = stubs.builtins();
        let list = Name::new("list");
        assert!(builtins.get(&list).is_some());
        assert!(builtins.is_class(&list));
    }

    #[test]
    fn test_method_safe_in_builtins() {
        let stubs = Stubs::new();
        assert!(stubs.is_method_safe_in_builtins(&Name::new("copy")));
        assert!(stubs.is_method_safe_in_builtins(&Name::new("get")));
        assert!(stubs.is_method_safe_in_builtins(&Name::new("index")));
        assert!(stubs.is_method_safe_in_builtins(&Name::new("count")));
        assert!(!stubs.is_method_safe_in_builtins(&Name::new("append")));
        assert!(!stubs.is_method_safe_in_builtins(&Name::new("extend")));
        assert!(!stubs.is_method_safe_in_builtins(&Name::new("pop")));
        assert!(!stubs.is_method_safe_in_builtins(&Name::new("remove")));
        assert!(!stubs.is_method_safe_in_builtins(&Name::new("nonexistent_method")));
    }

    #[test]
    fn test_shared_lookup() {
        let stubs = Stubs::new();
        let key = ModuleName::from_str("lifeguard_test");
        let test = stubs.get(&key);
        assert!(test.is_some());
        assert!(
            test.unwrap()
                .classes
                .contains(&ModuleName::from_str("lifeguard_test.A"))
        );
    }
}
