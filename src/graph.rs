/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! ModuleName-keyed directed graph over modules, backed by petgraph. Sole
//! production use is `imports::ImportGraph`: one node per module, so per-edge
//! bookkeeping is affordable. Cycle detection reports multi-node SCCs only; a
//! module importing itself is not a cycle here. For the function-scale call
//! graph (dense `u32` keys, millions of nodes, self-loops count as cycles),
//! see `csr_graph::CsrGraph`.

use petgraph::Direction;
use petgraph::algo::kosaraju_scc;
use petgraph::graph::DiGraph;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeFiltered;
use petgraph::visit::EdgeRef;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

use crate::hasher::AHashMap;
use crate::hasher::HashMapExt;
use crate::traits::ModuleNameExt;

/// Sequence of nodes that form a cycle in the graph.
pub type Cycle = Vec<NodeIndex>;

/// A directed graph of ModuleName nodes.
#[derive(Debug)]
pub struct Graph {
    /// The underlying directed graph implementation.  Composed of ModuleName nodes that point to
    /// one another.
    graph: DiGraph<ModuleName, ()>,
    /// Secondary map to support keying by ModuleName.  The petgraph implementation is keyed by
    /// NodeIndex, the ModuleName nodes are treated as values.
    nodes: AHashMap<ModuleName, NodeIndex>,
}

impl Graph {
    /// Create a new, empty graph.
    pub fn new() -> Self {
        Self {
            graph: DiGraph::<ModuleName, ()>::new(),
            nodes: AHashMap::new(),
        }
    }

    /// Create a new graph with pre-allocated capacity.
    pub fn with_capacity(nodes: usize, edges: usize) -> Self {
        Self {
            graph: DiGraph::<ModuleName, ()>::with_capacity(nodes, edges),
            nodes: AHashMap::with_capacity(nodes),
        }
    }

    /// Add a new node into the graph.  Does nothing if the node is already in the graph.
    pub fn add_node(&mut self, node: &ModuleName) -> NodeIndex {
        if let Some(ix) = self.nodes.get(node) {
            return *ix;
        }

        let ix = self.graph.add_node(*node);
        self.nodes.insert(*node, ix);
        ix
    }

    /// Add a new edge into the graph.  The `from` node is expected to exist in the graph already.
    /// When the `to` module is not present in the graph (e.g. it belongs to a third-party package
    /// or the stdlib and is not part of the analyzed source database), no edge is created and
    /// `false` is returned.  Missing imports are tracked separately by `ImportGraph.missing`.
    pub fn add_edge(&mut self, from: &ModuleName, to: &ModuleName) -> bool {
        let p = self.nodes[from];
        if let Some(&q) = self.nodes.get(to) {
            self.graph.add_edge(p, q, ());
            true
        } else {
            false
        }
    }

    /// Add an edge if it does not already exist and is not a self-edge.
    /// Returns `true` if the edge was added, `false` otherwise.
    /// The `from` node is expected to exist in the graph already.
    /// When the `to` module is not present, no edge is created and `false` is returned.
    pub(crate) fn try_add_edge(&mut self, from: &ModuleName, to: &ModuleName) -> bool {
        if from == to {
            return false;
        }
        if self.has_edge(from, to) {
            return false;
        }
        self.add_edge(from, to)
    }

    /// Get a parallel iterator over all nodes in the graph.
    pub fn nodes_par_iter(&self) -> impl ParallelIterator<Item = (&ModuleName, &NodeIndex)> {
        self.nodes.par_iter()
    }

    /// Get an iterator over all node names in the graph.
    pub fn node_names(&self) -> impl Iterator<Item = &ModuleName> {
        self.nodes.keys()
    }

    /// Get an iterator over all the neighbors of a node.
    pub fn neighbors(&self, node: &ModuleName) -> impl Iterator<Item = &ModuleName> {
        self.find_edges(node, Direction::Outgoing)
    }

    /// Get an iterator over all the reverse neighbors of a node, i.e. all nodes that point back to
    /// it.
    pub fn reverse_neighbors(&self, node: &ModuleName) -> impl Iterator<Item = &ModuleName> {
        self.find_edges(node, Direction::Incoming)
    }

    /// Remove a node from the graph entirely.
    pub(crate) fn remove_node(&mut self, name: &ModuleName) {
        if let Some(ix) = self.nodes.remove(name) {
            let last_ix = NodeIndex::new(self.graph.node_count() - 1);
            self.graph.remove_node(ix);
            if ix != last_ix {
                let &swapped_name = self
                    .graph
                    .node_weight(ix)
                    .expect("petgraph swap-remove guarantees the swapped node exists");
                self.nodes.insert(swapped_name, ix);
            }
        }
    }

    /// Check if a node exists in the graph.
    pub fn contains(&self, node: &ModuleName) -> bool {
        self.nodes.contains_key(node)
    }

    /// Check if an edge exists in the graph.
    fn has_edge(&self, from: &ModuleName, to: &ModuleName) -> bool {
        let Some(&p) = self.nodes.get(from) else {
            return false;
        };
        let Some(&q) = self.nodes.get(to) else {
            return false;
        };
        self.graph.contains_edge(p, q)
    }

    fn find_edges(
        &self,
        node: &ModuleName,
        direction: Direction,
    ) -> impl Iterator<Item = &ModuleName> {
        let ix = self.nodes.get(node).copied();
        ix.into_iter().flat_map(move |ix| {
            self.graph.neighbors_directed(ix, direction).map(|v| {
                self.graph
                    .node_weight(v)
                    .expect("Neighboring nodes have to have been inserted in the graph already")
            })
        })
    }

    /// Find cycles in the graph (for circular import detection), as if each key of `extra` also
    /// had edges to its values, which are not kept. Edges with an endpoint that is not a node are
    /// skipped, and so are edges to a module's own package: CPython runs that package first.
    ///
    /// Finds non-trivial strongly-connected components in the graph; an SCC with more than one
    /// node contains at least one cycle.
    /// Note that this doesn't find every possible cycle, but it does find every node that is part
    /// of at least one cycle.
    pub fn find_cycles(&self, extra: &AHashMap<ModuleName, Vec<ModuleName>>) -> Vec<Cycle> {
        if extra.is_empty() {
            return multi_node_sccs(&self.graph);
        }
        let extra_edges: usize = extra.values().map(Vec::len).sum();
        let mut graph = DiGraph::with_capacity(
            self.graph.node_count(),
            self.graph.edge_count() + extra_edges,
        );
        graph.clone_from(&self.graph);
        for (from, tos) in extra {
            let Some(&p) = self.nodes.get(from) else {
                continue;
            };
            for to in tos {
                if let Some(&q) = self.nodes.get(to) {
                    graph.add_edge(p, q, ());
                }
            }
        }
        multi_node_sccs(&graph)
    }

    /// Get an iterator over all module names in a cycle.
    pub fn cycle_names(&self, cycle: &Cycle) -> impl Iterator<Item = ModuleName> {
        cycle.iter().map(|ix| self.graph[*ix])
    }
}

fn multi_node_sccs(graph: &DiGraph<ModuleName, ()>) -> Vec<Cycle> {
    let kept = EdgeFiltered::from_fn(graph, |edge| {
        !graph[edge.target()].is_ancestor_of(&graph[edge.source()])
    });
    let mut sccs = kosaraju_scc(&kept);
    sccs.retain(|scc| scc.len() > 1);
    sccs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lib::*;

    fn all_cycles(g: &Graph) -> Vec<Cycle> {
        g.find_cycles(&AHashMap::new())
    }

    fn assert_deps(g: &Graph, module: &str, expected: Vec<&str>) {
        let m = ModuleName::from_str(module);
        let mut exp = module_names(expected);
        let mut actual = g.neighbors(&m).cloned().collect::<Vec<_>>();
        exp.sort();
        actual.sort();
        assert_eq!(actual, exp);
    }

    fn assert_rdeps(g: &Graph, module: &str, expected: Vec<&str>) {
        let m = ModuleName::from_str(module);
        let mut exp = module_names(expected);
        let mut actual = g.reverse_neighbors(&m).cloned().collect::<Vec<_>>();
        exp.sort();
        actual.sort();
        assert_eq!(actual, exp);
    }

    #[test]
    fn test_basic() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        let c = ModuleName::from_str("c");
        g.add_node(&a);
        g.add_node(&b);
        g.add_node(&c);
        g.add_edge(&a, &b);
        g.add_edge(&a, &c);
        assert_deps(&g, "a", vec!["b", "c"]);
        assert_rdeps(&g, "b", vec!["a"]);
    }

    #[test]
    fn test_missing() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        let c = ModuleName::from_str("c");
        g.add_node(&a);
        g.add_node(&b);
        assert!(g.add_edge(&a, &b));
        assert!(!g.add_edge(&a, &c));
        // Missing target produces no edge; only the resolved edge to b exists
        assert_deps(&g, "a", vec!["b"]);
        assert!(!g.contains(&c));
    }

    #[test]
    fn test_find_cycles_no_cycles() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        let c = ModuleName::from_str("c");
        g.add_node(&a);
        g.add_node(&b);
        g.add_node(&c);
        g.add_edge(&a, &b);
        g.add_edge(&b, &c);
        assert!(all_cycles(&g).is_empty());
    }

    #[test]
    fn test_find_cycles_multiple_cycles() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        let c = ModuleName::from_str("c");
        let d = ModuleName::from_str("d");
        let e = ModuleName::from_str("e");
        g.add_node(&a);
        g.add_node(&b);
        g.add_node(&c);
        g.add_node(&d);
        g.add_node(&e);
        // Cycle 1: a -> b -> a
        g.add_edge(&a, &b);
        g.add_edge(&b, &a);
        // One component with two cycles:
        // Cycle 2: c -> d -> e -> c
        // Cycle 3: c -> e -> c
        g.add_edge(&c, &d);
        g.add_edge(&c, &e);
        g.add_edge(&d, &e);
        g.add_edge(&e, &c);
        let cycles = all_cycles(&g);
        assert_eq!(cycles.len(), 2);
    }

    #[test]
    fn test_find_edges_unknown_node() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        g.add_node(&a);
        g.add_node(&b);
        g.add_edge(&a, &b);
        let unknown = ModuleName::from_str("unknown");
        assert_eq!(g.neighbors(&unknown).count(), 0);
        assert_eq!(g.reverse_neighbors(&unknown).count(), 0);
    }

    #[test]
    fn test_cycle_names() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        let c = ModuleName::from_str("c");
        g.add_node(&a);
        g.add_node(&b);
        g.add_node(&c);
        g.add_edge(&a, &b);
        g.add_edge(&b, &c);
        g.add_edge(&c, &a);
        let cycles = all_cycles(&g);
        assert_eq!(cycles.len(), 1);
        let mut names = g.cycle_names(&cycles[0]).collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec![a, b, c]);
    }

    #[test]
    fn test_self_loop_not_detected_as_cycle() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        g.add_node(&a);
        g.add_edge(&a, &a);
        // Self-loops form a trivial SCC (len 1), filtered out by find_cycles
        assert!(
            all_cycles(&g).is_empty(),
            "self-loop should not be reported as a cycle"
        );
    }

    #[test]
    fn test_self_loop_appears_in_neighbors() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        g.add_node(&a);
        g.add_edge(&a, &a);
        let neighbors: Vec<_> = g.neighbors(&a).collect();
        assert_eq!(neighbors.len(), 1, "self-loop should appear in neighbors");
        assert_eq!(neighbors[0], &a);
    }

    #[test]
    fn test_has_edge() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        let c = ModuleName::from_str("c");
        g.add_node(&a);
        g.add_node(&b);
        g.add_node(&c);
        g.add_edge(&a, &b);
        assert!(g.has_edge(&a, &b));
        assert!(!g.has_edge(&b, &a), "edge is directed");
        assert!(!g.has_edge(&a, &c), "no edge added between a and c");
    }

    #[test]
    fn test_has_edge_unknown_nodes() {
        let g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        assert!(!g.has_edge(&a, &b), "unknown nodes should return false");
    }

    #[test]
    fn test_add_node_is_idempotent() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let ix1 = g.add_node(&a);
        let ix2 = g.add_node(&a);
        assert_eq!(ix1, ix2, "adding same node twice should return same index");
    }

    #[test]
    fn test_find_cycles_with_extra_edges() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        g.add_node(&a);
        g.add_node(&b);
        g.add_edge(&a, &b);
        let mut extra = AHashMap::new();
        extra.insert(b, vec![a, ModuleName::from_str("unknown")]);
        extra.insert(ModuleName::from_str("unknown"), vec![a]);
        let cycles = g.find_cycles(&extra);
        assert_eq!(cycles.len(), 1);
        let mut names = g.cycle_names(&cycles[0]).collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec![a, b]);
        assert!(all_cycles(&g).is_empty(), "extra edges should not be kept");
    }

    #[test]
    fn test_edge_to_own_package_closes_no_cycle() {
        let mut g = Graph::new();
        let pkg = ModuleName::from_str("pkg");
        let sub = ModuleName::from_str("pkg.sub");
        let other = ModuleName::from_str("other");
        g.add_node(&pkg);
        g.add_node(&sub);
        g.add_node(&other);
        g.add_edge(&pkg, &sub);
        g.add_edge(&sub, &pkg);
        assert!(all_cycles(&g).is_empty());
        g.add_edge(&sub, &other);
        g.add_edge(&other, &pkg);
        assert_eq!(
            all_cycles(&g).len(),
            1,
            "pkg -> pkg.sub -> other -> pkg is a cycle"
        );
    }

    #[test]
    fn test_find_cycles_deep_chain_on_small_stack() {
        let n = 100_000;
        let names: Vec<ModuleName> = (0..n)
            .map(|i| ModuleName::from_str(&format!("m{i}")))
            .collect();
        let mut g = Graph::with_capacity(n, n);
        for name in &names {
            g.add_node(name);
        }
        for i in 0..n {
            g.add_edge(&names[i], &names[(i + 1) % n]);
        }
        // Rayon workers get the default 2 MiB stack.
        let cycles = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || all_cycles(&g))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), n);
    }
}
