/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use ahash::AHashMap;
use petgraph::Direction;
use petgraph::algo::tarjan_scc;
use petgraph::graph::DiGraph;
use petgraph::graph::NodeIndex;
use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;

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

        let ix = self.graph.add_node(node.clone());
        self.nodes.insert(node.clone(), ix);
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

    /// Get a parallel iterator over all nodes in the graph.
    pub fn nodes_par_iter(&self) -> impl ParallelIterator<Item = (&ModuleName, &NodeIndex)> {
        self.nodes.par_iter()
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

    /// Check if a node exists in the graph.
    pub fn contains(&self, node: &ModuleName) -> bool {
        self.nodes.contains_key(node)
    }

    /// Check if an edge exists in the graph.
    pub(crate) fn has_edge(&self, from: &ModuleName, to: &ModuleName) -> bool {
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

    /// Find cycles in the graph (for circular import detection)
    ///
    /// Finds non-trivial strongly-connected components in the graph; an SCC with more than one
    /// node contains at least one cycle.
    /// Note that this doesn't find every possible cycle, but it does find every node that is part
    /// of at least one cycle.
    pub fn find_cycles(&self) -> Vec<Cycle> {
        let mut sccs = tarjan_scc(&self.graph);
        sccs.retain(|scc| scc.len() > 1);
        sccs
    }

    /// Get an iterator over all module names in a cycle.
    pub fn cycle_names(&self, cycle: &Cycle) -> impl Iterator<Item = ModuleName> {
        cycle.iter().map(|ix| self.graph[*ix])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_lib::*;

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
        assert!(g.find_cycles().is_empty());
    }

    #[test]
    fn test_find_cycles_simple_cycle() {
        let mut g = Graph::new();
        let a = ModuleName::from_str("a");
        let b = ModuleName::from_str("b");
        g.add_node(&a);
        g.add_node(&b);
        g.add_edge(&a, &b);
        g.add_edge(&b, &a);
        let cycles = g.find_cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), 2);
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
        let cycles = g.find_cycles();
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
        let cycles = g.find_cycles();
        assert_eq!(cycles.len(), 1);
        let mut names = g.cycle_names(&cycles[0]).collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec![a, b, c]);
    }
}
