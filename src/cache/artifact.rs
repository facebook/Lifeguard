/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The cache artifact: the facts one `analyze-library` action produces and
//! writes, and one `analyze-binary` action reads back.
//!
//! Everything here is either a wire-visible type or a way of building one from
//! the analysis. Combining two artifacts lives in [`super::merge`]; resolving
//! the combined facts lives in [`super`].

use std::path::Path;

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_text_size::TextRange;
use serde::Deserialize;
use serde::Serialize;

use crate::errors::ErrorKind;
use crate::errors::SafetyError;
use crate::exports::Exports;
use crate::hasher::AHashMap;
use crate::hasher::AHashSet;
use crate::hasher::HashMapExt;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::module_safety::FunctionSafetyInfo;
use crate::module_safety::MutationCandidate;
use crate::module_safety::SafetyResult;
use crate::project::SafetyMap;
use crate::project::SideEffectMap;
use crate::traits::ModuleNameExt;

/// A module's import edges from the graph, partitioned by resolution status.
pub(super) struct GraphEdgeSets {
    pub(super) imports: AHashSet<ModuleName>,
    pub(super) missing_imports: AHashSet<ModuleName>,
    pub(super) ambiguous_imports: AHashSet<ModuleName>,
}

pub(super) fn graph_edge_sets(graph: &ImportGraph, name: &ModuleName) -> GraphEdgeSets {
    let imports = graph.get_imports(name).copied().collect();
    let missing_imports = graph
        .get_missing_imports(name)
        .map(|m| m.iter().copied().collect())
        .unwrap_or_default();
    let ambiguous_imports = graph
        .get_ambiguous_imports(name)
        .map(|m| m.iter().copied().collect())
        .unwrap_or_default();
    GraphEdgeSets {
        imports,
        missing_imports,
        ambiguous_imports,
    }
}

/// Cached analysis results for a single Python library.
/// Contains all information needed to merge with other libraries
/// in a map-reduce analysis pipeline.
#[derive(Serialize, Deserialize, Default)]
pub struct LibraryCache {
    pub modules: Vec<CachedModule>,
    pub exports: CachedExports,
    /// Class FQN -> base-class FQNs, for reduce-time MRO resolution of inherited
    /// `Class.method` calls.
    #[serde(default)]
    pub class_bases: Vec<(ModuleName, Vec<ModuleName>)>,
    /// Class FQN -> the functions a constructor call to it dispatches to, as
    /// resolved by the map phase.
    #[serde(default)]
    pub constructor_callees: Vec<(ModuleName, ConstructorCallees)>,
}

/// The methods an instantiation can dispatch to. The same set applies to the
/// metaclass and to the class itself, so a mask over two copies of it names
/// every callee the map phase can resolve without interning an FQN.
pub(crate) const CONSTRUCTOR_METHODS: [&str; 3] = ["__new__", "__init__", "__post_init__"];

/// Which of a class's candidate constructor callees the map phase resolved.
///
/// The candidate set is fixed — `CONSTRUCTOR_METHODS` on the metaclass, then
/// the same set on the class itself — so a mask over it plus the metaclass name
/// reconstructs every callee FQN.
///
/// NOTE: Storing the mask instead of the names keeps the callee FQNs out of both
/// the cache's name table and the process-wide `ModuleName` interner, neither of
/// which ever gives the memory back.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConstructorCallees {
    /// The metaclass the `CONSTRUCTOR_METHODS` bits are relative to. `None` when
    /// no metaclass bit is set, so that two libraries disagreeing about a class's
    /// metaclass cannot be merged into a callee set neither of them recorded.
    pub metaclass: Option<ModuleName>,
    /// Bit `i` for `CONSTRUCTOR_METHODS[i]` on `metaclass`, then bit
    /// `CONSTRUCTOR_METHODS.len() + i` for `CONSTRUCTOR_METHODS[i]` on the class.
    pub mask: u8,
    /// Callees the mask cannot name: a constructor inherited from a base class,
    /// whose owner is neither the class nor its metaclass.
    #[serde(default)]
    pub extra: Vec<ModuleName>,
}

impl ConstructorCallees {
    /// Bits covering the class's own methods, which need no metaclass.
    const OWN_BITS: u8 =
        ((1 << CONSTRUCTOR_METHODS.len()) - 1) << (CONSTRUCTOR_METHODS.len() as u8);

    /// A record naming no callee at all, which reduce must not read as "nothing
    /// runs" — it means the map phase resolved nothing, so the call cannot clear.
    pub(crate) fn is_empty(&self) -> bool {
        self.mask == 0 && self.extra.is_empty()
    }

    /// Union two libraries' records for the same class. Conflicting metaclasses
    /// drop to the own-class bits.
    pub(super) fn merged_with(self, other: Self) -> Self {
        let metaclass = match (self.metaclass, other.metaclass) {
            (Some(a), Some(b)) if a != b => None,
            (a, b) => a.or(b),
        };
        let mut mask = self.mask | other.mask;
        if metaclass.is_none() {
            mask &= Self::OWN_BITS;
        }
        let mut extra = self.extra;
        for callee in other.extra {
            if !extra.contains(&callee) {
                extra.push(callee);
            }
        }
        Self {
            metaclass,
            mask,
            extra,
        }
    }

    /// Each recorded callee as the FQN that owns it paired with the method name,
    /// which together rebuild the callee without allocating an interned FQN.
    pub(crate) fn iter(
        &self,
        class_fqn: ModuleName,
    ) -> impl Iterator<Item = (ModuleName, &'static str)> {
        let metaclass_callees = self.metaclass.into_iter().flat_map(move |metaclass| {
            Self::selected(self.mask, 0, &CONSTRUCTOR_METHODS).map(move |m| (metaclass, m))
        });
        let own_callees =
            Self::selected(self.mask, CONSTRUCTOR_METHODS.len(), &CONSTRUCTOR_METHODS)
                .map(move |m| (class_fqn, m));
        metaclass_callees.chain(own_callees)
    }

    fn selected(
        mask: u8,
        base_bit: usize,
        methods: &'static [&'static str],
    ) -> impl Iterator<Item = &'static str> {
        methods
            .iter()
            .enumerate()
            .filter(move |(i, _)| mask & (1 << (base_bit + i)) != 0)
            .map(|(_, method)| *method)
    }
}

/// The mask bit naming `method` on the class itself.
pub fn own_constructor_bit(method: &str) -> u8 {
    let index = CONSTRUCTOR_METHODS
        .iter()
        .position(|m| *m == method)
        .expect("should name one of CONSTRUCTOR_METHODS");
    1 << (CONSTRUCTOR_METHODS.len() + index)
}

/// Set the bit for `methods[i]` when `resolves` accepts `owner`.`methods[i]`.
pub(crate) fn constructor_mask_bits(
    owner: ModuleName,
    base_bit: usize,
    methods: &[&str],
    mut resolves: impl FnMut(&ModuleName) -> bool,
) -> u8 {
    methods
        .iter()
        .enumerate()
        .filter(|(_, method)| resolves(&owner.append_str(method)))
        .map(|(i, _)| 1u8 << (base_bit + i))
        .fold(0u8, |mask, bit| mask | bit)
}

impl LibraryCache {
    /// Reconstruct an ImportGraph from cached module import edges.
    pub fn to_import_graph(&self) -> ImportGraph {
        let mut graph = ImportGraph::new();
        for module in &self.modules {
            graph.graph.add_node(&module.name);
        }
        for module in &self.modules {
            for imported in &module.imports {
                graph.graph.add_edge(&module.name, imported);
            }
            for missing in &module.missing_imports {
                graph.add_missing(&module.name, *missing);
            }
        }
        graph
    }
}

/// Cached analysis for a single module within a library.
#[derive(Serialize, Deserialize)]
pub struct CachedModule {
    pub name: ModuleName,
    pub safety: CachedSafety,
    /// Resolved imports (edges in the import graph).
    pub imports: AHashSet<ModuleName>,
    /// Imports that could not be resolved to modules in the source DB.
    pub missing_imports: AHashSet<ModuleName>,
    /// `from X import Y` where X is in the library but X.Y is not.
    /// May be a submodule in another library or an attribute of X.
    pub ambiguous_imports: AHashSet<ModuleName>,
    /// Module-level imports never accessed in any scope (side-effect imports).
    pub side_effect_imports: AHashSet<ModuleName>,
    /// Per-function safety info from call graph analysis.
    /// Keys are function-local names (e.g., "helper" for `mod.helper`).
    pub function_safety: AHashMap<String, FunctionSafetyInfo>,
    /// Calls passing imported objects to cross-library-unresolved callees,
    /// resolved against the merged cache in the reduce step.
    pub mutation_candidates: Vec<MutationCandidate>,
}

/// Safety analysis result for a cached module.
#[derive(Serialize, Deserialize)]
pub enum CachedSafety {
    Ok(CachedModuleSafety),
    AnalysisError { message: String },
}

/// Detailed safety information for a module.
#[derive(Default, Serialize, Deserialize)]
pub struct CachedModuleSafety {
    pub errors: Vec<CachedError>,
    pub force_imports_eager_overrides: Vec<CachedError>,
    pub implicit_imports: Vec<ModuleName>,
}

/// A serializable safety error (without source location).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct CachedError {
    pub kind: ErrorKind,
    pub metadata: String,
    pub parameterized_decorator: bool,
}

/// Cached re-export information for a library. Only re-exports are consumed by
/// the reduce (`analyze-binary`); the map phase's other export tables
/// (definitions/`__all__`/return types) are not, so they are not cached.
#[derive(Serialize, Deserialize, Default)]
pub struct CachedExports {
    pub re_exports: Vec<CachedReExport>,
}

/// A cached re-export entry (module.attr -> source_module.source_attr).
#[derive(Serialize, Deserialize)]
pub struct CachedReExport {
    pub exported_module: ModuleName,
    pub exported_attr: String,
    pub imported_module: ModuleName,
    pub imported_attr: String,
}

impl LibraryCache {
    pub fn empty() -> Self {
        LibraryCache {
            modules: Vec::new(),
            exports: CachedExports {
                re_exports: Vec::new(),
            },
            ..Default::default()
        }
    }

    /// Build a cache from the analysis pipeline results.
    pub fn build(
        safety_map: &SafetyMap,
        import_graph: &ImportGraph,
        exports: &Exports,
        side_effect_imports: &SideEffectMap,
    ) -> Self {
        let mut modules: Vec<CachedModule> = safety_map
            .par_iter()
            .map(|entry| {
                let name = *entry.key();
                let safety_result = entry.value();

                let GraphEdgeSets {
                    imports,
                    missing_imports,
                    ambiguous_imports,
                } = graph_edge_sets(import_graph, &name);

                let se_imports: AHashSet<ModuleName> = side_effect_imports
                    .get(&name)
                    .map(|s| s.iter().copied().collect())
                    .unwrap_or_default();

                let (function_safety, mutation_candidates) = match safety_result {
                    SafetyResult::Ok(ms) => {
                        (ms.function_safety.clone(), ms.mutation_candidates.clone())
                    }
                    _ => (AHashMap::new(), Vec::new()),
                };

                let safety = CachedSafety::from_safety_result(safety_result);

                CachedModule {
                    name,
                    safety,
                    imports,
                    missing_imports,
                    ambiguous_imports,
                    side_effect_imports: se_imports,
                    function_safety,
                    mutation_candidates,
                }
            })
            .collect();

        modules.sort_by_key(|m| m.name);

        let own_modules: AHashSet<ModuleName> = modules.iter().map(|m| m.name).collect();
        let exports = CachedExports::from_exports(exports, &own_modules);

        LibraryCache {
            modules,
            exports,
            ..Default::default()
        }
    }

    /// Attach class base edges (class FQN -> base FQNs) for MRO resolution during
    /// the reduce step. Populated by the map phase (`analyze-library`).
    pub fn set_class_bases(&mut self, class_bases: Vec<(ModuleName, Vec<ModuleName>)>) {
        self.class_bases = class_bases;
    }

    /// Attach the map phase's resolved constructor callees (class FQN -> the
    /// functions its instantiation runs).
    pub fn set_constructor_callees(
        &mut self,
        constructor_callees: Vec<(ModuleName, ConstructorCallees)>,
    ) {
        self.constructor_callees = constructor_callees;
    }

    /// Write the cache using the indexed wire format.
    pub fn write_to_file(&self, path: &Path) -> anyhow::Result<()> {
        crate::cache_wire::write(self, path)
    }

    /// Read a cache using the indexed wire format.
    pub fn read_from_file(path: &Path) -> anyhow::Result<Self> {
        crate::cache_wire::read(path)
    }
}

impl CachedModule {
    pub(crate) fn empty(name: ModuleName) -> Self {
        CachedModule {
            name,
            safety: CachedSafety::Ok(CachedModuleSafety::default()),
            imports: AHashSet::new(),
            missing_imports: AHashSet::new(),
            ambiguous_imports: AHashSet::new(),
            side_effect_imports: AHashSet::new(),
            function_safety: AHashMap::new(),
            mutation_candidates: Vec::new(),
        }
    }

    pub fn is_safe(&self) -> bool {
        matches!(&self.safety, CachedSafety::Ok(s) if s.is_safe())
    }
}

impl CachedModuleSafety {
    pub fn is_safe(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn should_load_imports_eagerly(&self) -> bool {
        !self.force_imports_eager_overrides.is_empty()
    }
}

impl CachedError {
    pub(crate) fn from_safety_error(error: &SafetyError) -> Self {
        CachedError {
            kind: error.kind,
            metadata: error.metadata.as_str().to_string(),
            parameterized_decorator: error.parameterized_decorator,
        }
    }

    pub(crate) fn to_safety_error(&self) -> SafetyError {
        let mut error = SafetyError::new(self.kind, self.metadata.clone(), TextRange::default());
        error.parameterized_decorator = self.parameterized_decorator;
        error
    }
}

impl CachedExports {
    /// Build the cached re-exports for a library, keeping only those exported by
    /// one of the library's own modules. `get_re_exports()` also yields the bundled
    /// stubs' re-exports, identical across every cache; dropping them is safe because
    /// each re-export is owned by exactly one module's cache and the reduce rebuilds
    /// stub chains from the bundled stub graph.
    pub(crate) fn from_exports(exports: &Exports, own_modules: &AHashSet<ModuleName>) -> Self {
        let re_exports: Vec<CachedReExport> = exports
            .get_re_exports()
            .filter(|(module, _, _)| own_modules.contains(module))
            .map(|(module, attr, (imported, _range))| CachedReExport {
                exported_module: module,
                exported_attr: attr.to_string(),
                imported_module: imported.module,
                imported_attr: imported.attr.to_string(),
            })
            .collect();

        let mut result = CachedExports { re_exports };
        result.sort_and_dedup();
        result
    }

    pub(crate) fn sort_and_dedup(&mut self) {
        self.re_exports.par_sort_by(|a, b| {
            (&a.exported_module, &a.exported_attr).cmp(&(&b.exported_module, &b.exported_attr))
        });
        self.re_exports.dedup_by(|a, b| {
            a.exported_module == b.exported_module && a.exported_attr == b.exported_attr
        });
    }
}
