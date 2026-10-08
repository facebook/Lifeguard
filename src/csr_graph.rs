/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A compact directed graph in CSR (compressed sparse row) form, keyed by dense
//! `u32` node indices. Built for graphs with millions of nodes and edges where a
//! pointer-based adjacency structure's per-edge bookkeeping dominates: CSR stores
//! the whole adjacency in two flat arrays and runs an allocation-light iterative
//! Tarjan SCC.
//!
//! Sole production use is the call graph in `project.rs`, where a self-loop is
//! recursion and therefore a cycle. For the module-scale import graph
//! (ModuleName keys, petgraph-backed, self-imports not cycles), see
//! `graph::Graph`.

/// A directed graph over nodes `0..num_nodes`, stored as CSR adjacency.
pub struct CsrGraph {
    /// `offsets[v]..offsets[v + 1]` is the range of `adj` holding `v`'s out-edges.
    offsets: Vec<u32>,
    /// Concatenated out-neighbor lists, one block per node in node order.
    adj: Vec<u32>,
}

impl CsrGraph {
    /// Build a CSR graph from `num_nodes` and a list of directed `(from, to)`
    /// edges. Both endpoints must be `< num_nodes`. Duplicate and self edges are
    /// preserved.
    pub fn from_edges(num_nodes: usize, edges: &[(u32, u32)]) -> Self {
        // Counting sort of edges by source into the flat adjacency array.
        let mut offsets = vec![0u32; num_nodes + 1];
        for &(u, _) in edges {
            offsets[u as usize + 1] += 1;
        }
        for i in 0..num_nodes {
            offsets[i + 1] += offsets[i];
        }

        let mut adj = vec![0u32; edges.len()];
        let mut cursor = offsets.clone();
        for &(u, v) in edges {
            let slot = cursor[u as usize];
            adj[slot as usize] = v;
            cursor[u as usize] = slot + 1;
        }

        Self { offsets, adj }
    }

    /// Number of nodes in the graph.
    pub fn num_nodes(&self) -> usize {
        self.offsets.len() - 1
    }

    /// Out-neighbors of `node`.
    pub fn neighbors(&self, node: u32) -> &[u32] {
        let start = self.offsets[node as usize] as usize;
        let end = self.offsets[node as usize + 1] as usize;
        &self.adj[start..end]
    }

    /// Return, for each node, whether it belongs to a cycle: either a member of a
    /// strongly-connected component of size > 1, or a self-loop.
    ///
    /// Uses an iterative (explicit-stack) Tarjan SCC so deep graphs cannot blow
    /// the call stack.
    pub fn nodes_in_cycles(&self) -> Vec<bool> {
        let n = self.num_nodes();
        let mut in_cycle = vec![false; n];

        // Self-loops form trivial (size-1) SCCs, so Tarjan won't flag them.
        for v in 0..n as u32 {
            if self.neighbors(v).contains(&v) {
                in_cycle[v as usize] = true;
            }
        }

        const UNVISITED: u32 = u32::MAX;
        let mut idx = vec![UNVISITED; n];
        let mut low = vec![0u32; n];
        let mut on_stack = vec![false; n];
        let mut scc_stack: Vec<u32> = Vec::new();
        // Explicit DFS work stack: (node, cursor into that node's adjacency).
        let mut work: Vec<(u32, u32)> = Vec::new();
        let mut next_index: u32 = 0;

        for start in 0..n as u32 {
            if idx[start as usize] != UNVISITED {
                continue;
            }
            idx[start as usize] = next_index;
            low[start as usize] = next_index;
            next_index += 1;
            scc_stack.push(start);
            on_stack[start as usize] = true;
            work.push((start, self.offsets[start as usize]));

            while let Some(&(v, cursor)) = work.last() {
                if cursor < self.offsets[v as usize + 1] {
                    work.last_mut().unwrap().1 = cursor + 1;
                    let w = self.adj[cursor as usize];
                    if idx[w as usize] == UNVISITED {
                        idx[w as usize] = next_index;
                        low[w as usize] = next_index;
                        next_index += 1;
                        scc_stack.push(w);
                        on_stack[w as usize] = true;
                        work.push((w, self.offsets[w as usize]));
                    } else if on_stack[w as usize] {
                        let lv = low[v as usize].min(idx[w as usize]);
                        low[v as usize] = lv;
                    }
                } else {
                    // Finished v: if it's an SCC root, pop its whole component off
                    // scc_stack (no per-component allocation). Only components with
                    // more than one node are cycles — self-loops are handled up
                    // front — so a singleton root, which is the node already on top
                    // of the stack, is popped without being marked.
                    if low[v as usize] == idx[v as usize] {
                        if scc_stack.last() == Some(&v) {
                            scc_stack.pop();
                            on_stack[v as usize] = false;
                        } else {
                            // Multi-node SCC: every member from the stack top down to
                            // and including v is in a cycle.
                            loop {
                                let w = scc_stack.pop().expect("scc stack underflow");
                                on_stack[w as usize] = false;
                                in_cycle[w as usize] = true;
                                if w == v {
                                    break;
                                }
                            }
                        }
                    }
                    work.pop();
                    if let Some(&(parent, _)) = work.last() {
                        let lp = low[parent as usize].min(low[v as usize]);
                        low[parent as usize] = lp;
                    }
                }
            }
        }

        in_cycle
    }

    /// Group nodes into levels such that every out-neighbor of a node sits in an
    /// earlier level. Walking the levels in order then ensures a caller's analysis
    /// reads finished results for all of its callees.
    ///
    /// `settled` marks nodes whose result is already known; they are omitted from
    /// the levels and ignored when leveling their predecessors.
    ///
    /// NOTE: A node reachable only through a cycle among unsettled nodes would have
    /// no valid level. Rather than loop forever, a back edge contributes nothing to
    /// the level, which places such a node no later than its cycle peers. Callers
    /// that need cycle members handled first should *settle them beforehand* --
    /// `nodes_in_cycles` identifies them.
    pub fn dependency_levels(&self, settled: &[bool]) -> Vec<Vec<u32>> {
        assert_eq!(
            settled.len(),
            self.num_nodes(),
            "settled flags must cover every node",
        );

        const UNLEVELED: u32 = u32::MAX;
        let n = self.num_nodes();
        let mut level = vec![UNLEVELED; n];
        // Distinguishes a back edge (node still being expanded) from a node whose
        // level is genuinely not computed yet.
        let mut expanding = vec![false; n];
        // Explicit DFS work stack: (node, cursor into that node's adjacency).
        let mut work: Vec<(u32, u32)> = Vec::new();

        for start in 0..n as u32 {
            if settled[start as usize] || level[start as usize] != UNLEVELED {
                continue;
            }
            work.push((start, self.offsets[start as usize]));
            expanding[start as usize] = true;

            while let Some(&(v, cursor)) = work.last() {
                if cursor < self.offsets[v as usize + 1] {
                    work.last_mut().expect("work stack is non-empty here").1 = cursor + 1;
                    let w = self.adj[cursor as usize];
                    let skip = settled[w as usize]
                        || level[w as usize] != UNLEVELED
                        || expanding[w as usize];
                    if !skip {
                        expanding[w as usize] = true;
                        work.push((w, self.offsets[w as usize]));
                    }
                    continue;
                }

                // Every out-neighbor is either settled, leveled, or a back edge, so
                // this node's level is one past the deepest leveled neighbor.
                level[v as usize] = self
                    .neighbors(v)
                    .iter()
                    .filter(|&&w| !settled[w as usize] && level[w as usize] != UNLEVELED)
                    .map(|&w| level[w as usize] + 1)
                    .max()
                    .unwrap_or(0);
                expanding[v as usize] = false;
                work.pop();
            }
        }

        let Some(deepest) = level.iter().filter(|&&l| l != UNLEVELED).max().copied() else {
            return Vec::new();
        };
        let mut levels = vec![Vec::new(); deepest as usize + 1];
        for (node, &node_level) in level.iter().enumerate() {
            if node_level != UNLEVELED {
                levels[node_level as usize].push(node as u32);
            }
        }
        levels
    }
}

#[cfg(test)]
mod tests {
    use petgraph::algo::tarjan_scc;
    use petgraph::graph::DiGraph;
    use petgraph::graph::NodeIndex;

    use super::CsrGraph;
    use crate::test_lib::TestRng;

    #[test]
    fn neighbors_reflect_edges() {
        let g = CsrGraph::from_edges(3, &[(0, 1), (0, 2), (1, 2)]);
        assert_eq!(g.num_nodes(), 3);
        let mut n0 = g.neighbors(0).to_vec();
        n0.sort();
        assert_eq!(n0, vec![1, 2]);
        assert_eq!(g.neighbors(1), &[2]);
        assert_eq!(g.neighbors(2), &[] as &[u32]);
    }

    #[test]
    fn self_loop_is_a_cycle() {
        // Unlike a self-import in `graph::Graph`, recursion is a cycle here.
        let in_cycle = CsrGraph::from_edges(3, &[(0, 1), (1, 1)]).nodes_in_cycles();
        assert_eq!(in_cycle, vec![false, true, false]);
    }

    /// Levels of an `n`-node graph with no node settled.
    fn levels(n: usize, edges: &[(u32, u32)]) -> Vec<Vec<u32>> {
        CsrGraph::from_edges(n, edges).dependency_levels(&vec![false; n])
    }

    #[test]
    fn a_node_follows_its_deepest_neighbor() {
        // 0 points at both 1 (level 0) and 2 (level 1 via 3), so 0 is level 2.
        assert_eq!(
            levels(4, &[(0, 1), (0, 2), (2, 3)]),
            vec![vec![1, 3], vec![2], vec![0]],
        );
    }

    #[test]
    fn settled_nodes_are_omitted_and_ignored() {
        // 0 -> 1 -> 2 with 1 settled: 2 still levels on its own, and 0 no longer
        // waits on anything, so both land in the first level.
        let graph = CsrGraph::from_edges(3, &[(0, 1), (1, 2)]);
        assert_eq!(
            graph.dependency_levels(&[false, true, false]),
            vec![vec![0, 2]],
        );
    }

    #[test]
    fn a_cycle_among_unsettled_nodes_terminates() {
        // Callers are expected to settle cycle members first. If they do not, the
        // back edge is ignored rather than looping, and every node still appears
        // exactly once.
        let levels = levels(3, &[(0, 1), (1, 2), (2, 0)]);
        let mut nodes: Vec<u32> = levels.into_iter().flatten().collect();
        nodes.sort();
        assert_eq!(nodes, vec![0, 1, 2]);
    }

    #[test]
    fn deep_graphs_do_not_overflow_the_stack() {
        let n = 100_000;
        let chain: Vec<(u32, u32)> = (1..n as u32).map(|v| (v - 1, v)).collect();
        assert_eq!(levels(n, &chain).len(), n);

        let mut ring = chain;
        ring.push((n as u32 - 1, 0));
        assert!(
            CsrGraph::from_edges(n, &ring)
                .nodes_in_cycles()
                .iter()
                .all(|&c| c)
        );
    }

    const ROUNDS: usize = 2000;

    /// Up to 12 nodes with self-loops and duplicate edges allowed.
    fn random_graph(rng: &mut TestRng) -> (usize, Vec<(u32, u32)>) {
        let n = rng.below(13);
        if n == 0 {
            return (0, Vec::new());
        }
        let edges = (0..rng.below(3 * n + 1))
            .map(|_| (rng.below(n) as u32, rng.below(n) as u32))
            .collect();
        (n, edges)
    }

    /// A random DAG whose edges all point from a higher `rank` to a lower one.
    fn random_dag(rng: &mut TestRng) -> (usize, Vec<(u32, u32)>, Vec<usize>) {
        let (n, edges) = random_graph(rng);
        let mut rank: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            rank.swap(i, rng.below(i + 1));
        }
        let edges = edges
            .into_iter()
            .filter(|(u, v)| u != v)
            .map(|(u, v)| {
                if rank[u as usize] > rank[v as usize] {
                    (u, v)
                } else {
                    (v, u)
                }
            })
            .collect();
        (n, edges, rank)
    }

    fn random_settled(rng: &mut TestRng, n: usize) -> Vec<bool> {
        (0..n).map(|_| rng.below(4) == 0).collect()
    }

    /// Each node's level index, asserting no level is empty and no node repeats.
    fn level_of(n: usize, levels: &[Vec<u32>]) -> Vec<Option<usize>> {
        let mut level_of = vec![None; n];
        for (index, level) in levels.iter().enumerate() {
            assert!(!level.is_empty(), "level {index} is empty");
            for &node in level {
                assert!(
                    level_of[node as usize].replace(index).is_none(),
                    "node {node} appears more than once",
                );
            }
        }
        level_of
    }

    #[test]
    fn nodes_in_cycles_matches_petgraph_scc() {
        let mut rng = TestRng::new(0xC5C);
        for round in 0..ROUNDS {
            let (n, edges) = random_graph(&mut rng);
            let mut reference = DiGraph::<(), ()>::with_capacity(n, edges.len());
            for _ in 0..n {
                reference.add_node(());
            }
            for &(u, v) in &edges {
                reference.add_edge(NodeIndex::new(u as usize), NodeIndex::new(v as usize), ());
            }
            let mut expected = vec![false; n];
            for scc in tarjan_scc(&reference) {
                let cyclic = scc.len() > 1 || reference.contains_edge(scc[0], scc[0]);
                for node in scc {
                    expected[node.index()] = cyclic;
                }
            }

            assert_eq!(
                CsrGraph::from_edges(n, &edges).nodes_in_cycles(),
                expected,
                "round {round}, {n} nodes, edges {edges:?}",
            );
        }
    }

    #[test]
    fn dag_levels_are_longest_paths_to_a_sink() {
        let mut rng = TestRng::new(0xDA6);
        for round in 0..ROUNDS {
            let (n, edges, rank) = random_dag(&mut rng);
            let settled = random_settled(&mut rng, n);

            let mut by_rank: Vec<usize> = (0..n).collect();
            by_rank.sort_by_key(|&v| rank[v]);
            let mut expected: Vec<Option<usize>> = vec![None; n];
            for v in by_rank {
                if settled[v] {
                    continue;
                }
                let level = edges
                    .iter()
                    .filter(|&&(u, _)| u as usize == v)
                    .filter_map(|&(_, w)| expected[w as usize])
                    .map(|l| l + 1)
                    .max()
                    .unwrap_or(0);
                expected[v] = Some(level);
            }

            let levels = CsrGraph::from_edges(n, &edges).dependency_levels(&settled);
            assert_eq!(
                level_of(n, &levels),
                expected,
                "round {round}, {n} nodes, edges {edges:?}, settled {settled:?}",
            );
        }
    }
}
