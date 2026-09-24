/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `LifeGuardOutput` JSON shape: which modules load imports eagerly and
//! the lazy-eligibility guard set of each passing module.
//!
//! The pipeline around it is split by responsibility:
//!
//! - [`policy`] classifies modules and computes the guard sets.
//! - [`diagnostics`] renders the human-readable summary and verbose reports.

mod diagnostics;
mod policy;

use dashmap::DashMap;
use pyrefly_python::module_name::ModuleName;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeMap;
use serde::ser::SerializeStruct;
use starlark_map::small_set::SmallSet;

use crate::hasher::AHashMap;
pub use crate::output::diagnostics::AnalysisSummary;
pub use crate::output::diagnostics::write_verbose;
pub use crate::output::policy::LifeGuardAnalysis;

pub struct LifeGuardOutput {
    // Set of modules where we would like to load all of its imports eagerly
    pub load_imports_eagerly: SmallSet<ModuleName>,

    // Dictionary mapping safe modules to Lazy Imports incompatible modules
    // that are preventing them from being loaded lazily.
    // Uses DashMap for concurrent insertion during analysis.
    pub lazy_eligible: DashMap<ModuleName, SmallSet<ModuleName>>,

    // Whether to sort keys and values for deterministic output.
    pub sorted_output: bool,

    // Verbose-mode fields: only populated when --verbose-output is used.
    pub implicit_imports: Option<AHashMap<ModuleName, Vec<ModuleName>>>,
    pub import_cycles: Option<Vec<Vec<ModuleName>>>,
}

impl Serialize for LifeGuardOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let num_fields =
            2 + self.implicit_imports.is_some() as usize + self.import_cycles.is_some() as usize;
        let mut state = serializer.serialize_struct("LifeGuardOutput", num_fields)?;
        if self.sorted_output {
            let mut items: Vec<&ModuleName> = self.load_imports_eagerly.iter().collect();
            items.sort();
            state.serialize_field("LOAD_IMPORTS_EAGERLY", &items)?;
        } else {
            let items: Vec<&ModuleName> = self.load_imports_eagerly.iter().collect();
            state.serialize_field("LOAD_IMPORTS_EAGERLY", &items)?;
        }
        if self.sorted_output {
            let mut keys: Vec<ModuleName> = self.lazy_eligible.iter().map(|e| *e.key()).collect();
            keys.sort();
            let sorted: Vec<(ModuleName, Vec<ModuleName>)> = keys
                .iter()
                .map(|k| {
                    let entry = self.lazy_eligible.get(k).unwrap();
                    let mut vals: Vec<ModuleName> = entry.value().iter().copied().collect();
                    vals.sort();
                    (*k, vals)
                })
                .collect();
            state.serialize_field("LAZY_ELIGIBLE", &SortedModuleMap(&sorted))?;
        } else {
            state.serialize_field("LAZY_ELIGIBLE", &UnsortedDashMap(&self.lazy_eligible))?;
        }

        // Always sort implicit_imports and import_cycles — these are only
        // included in verbose mode where determinism matters more than speed.
        if let Some(implicit_imports) = &self.implicit_imports {
            let mut sorted: Vec<(ModuleName, Vec<ModuleName>)> = implicit_imports
                .iter()
                .map(|(k, v)| {
                    let mut vals = v.clone();
                    vals.sort();
                    (*k, vals)
                })
                .collect();
            sorted.sort_by_key(|(k, _)| *k);
            state.serialize_field("IMPLICIT_IMPORTS", &SortedModuleMap(&sorted))?;
        }

        if let Some(import_cycles) = &self.import_cycles {
            let mut sorted = import_cycles.clone();
            for cycle in &mut sorted {
                cycle.sort();
            }
            sorted.sort();
            state.serialize_field("IMPORT_CYCLES", &sorted)?;
        }

        state.end()
    }
}

/// Helper to serialize a pre-sorted list of (key, values) as a JSON map.
struct SortedModuleMap<'a>(&'a [(ModuleName, Vec<ModuleName>)]);

impl Serialize for SortedModuleMap<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

/// Helper to serialize a DashMap as a JSON map without sorting.
struct UnsortedDashMap<'a>(&'a DashMap<ModuleName, SmallSet<ModuleName>>);

impl Serialize for UnsortedDashMap<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for entry in self.0.iter() {
            map.serialize_entry(entry.key(), entry.value())?;
        }
        map.end()
    }
}

impl LifeGuardOutput {
    pub fn new(sorted_output: bool) -> Self {
        LifeGuardOutput {
            load_imports_eagerly: SmallSet::new(),
            lazy_eligible: DashMap::new(),
            sorted_output,
            implicit_imports: None,
            import_cycles: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mn(s: &str) -> ModuleName {
        ModuleName::from_str(s)
    }

    // ---- LifeGuardOutput serialization tests ----

    #[test]
    fn test_serialize_sorted_output() {
        let mut output = LifeGuardOutput::new(true);
        output.load_imports_eagerly.insert(mn("z_mod"));
        output.load_imports_eagerly.insert(mn("a_mod"));
        output.lazy_eligible.insert(mn("foo"), SmallSet::new());

        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        let eager = parsed["LOAD_IMPORTS_EAGERLY"].as_array().unwrap();
        assert_eq!(eager[0].as_str().unwrap(), "a_mod");
        assert_eq!(eager[1].as_str().unwrap(), "z_mod");
        assert!(parsed["LAZY_ELIGIBLE"]["foo"].is_array());
    }

    #[test]
    fn test_serialize_unsorted_output() {
        let output = LifeGuardOutput::new(false);
        output.lazy_eligible.insert(mn("mod_a"), {
            let mut s = SmallSet::new();
            s.insert(mn("dep_x"));
            s
        });

        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(
            parsed["LOAD_IMPORTS_EAGERLY"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(parsed["LAZY_ELIGIBLE"].is_object());
    }
}
