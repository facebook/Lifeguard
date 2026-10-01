/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Putting the bundled stdlib stubs back into a merged graph.
//!
//! A per-library cache drops modules only the stubs provide, so merging caches
//! loses the typeshed import cycle present in whole-program analysis. The
//! reduce rebuilds it from the bundled sources because the stubs depend only
//! on the Python version shared by the caches.

use pyrefly_python::module_name::ModuleName;

use crate::cache::artifact::CachedModule;
use crate::cache::artifact::GraphEdgeSets;
use crate::cache::artifact::LibraryCache;
use crate::cache::artifact::graph_edge_sets;
use crate::cache::main_guard::MainGuardFacts;
use crate::config::AnalysisConfig;
use crate::hasher::AHashSet;
use crate::hasher::HashSetExt;
use crate::imports::ImportGraph;
use crate::pyrefly::sys_info::PythonVersion;
use crate::source_map::bundled_stub_sources;

impl LibraryCache {
    /// Inject the bundled stdlib stubs as graph-only nodes so the merged graph
    /// matches the e2e graph: per-library caches drop stub-only modules, losing
    /// the typeshed import cycle. Skips names a real library already provides.
    /// Returns the injected names so the caller can keep them out of the safety map.
    pub(super) fn inject_bundled_stub_graph(
        &mut self,
        python_version: PythonVersion,
    ) -> AHashSet<ModuleName> {
        let sources = bundled_stub_sources(python_version);
        let config = AnalysisConfig::with_python_version(python_version, None);
        let graph = ImportGraph::make(&sources, &config);

        let existing: AHashSet<ModuleName> = self.modules.iter().map(|m| m.name).collect();
        let mut added = AHashSet::new();
        for name in graph.graph.node_names() {
            let name = *name;
            if existing.contains(&name) {
                continue;
            }
            let GraphEdgeSets {
                imports,
                missing_imports,
                ambiguous_imports,
                main_guard_imports,
            } = graph_edge_sets(&graph, &name);
            self.modules.push(CachedModule {
                imports,
                missing_imports,
                ambiguous_imports,
                main_guard: MainGuardFacts::new(main_guard_imports),
                ..CachedModule::empty(name)
            });
            added.insert(name);
        }
        added
    }
}
