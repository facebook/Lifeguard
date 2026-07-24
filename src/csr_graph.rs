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
}

#[cfg(test)]
mod tests {
    use super::CsrGraph;

    /// Indices of nodes flagged as being in a cycle.
    fn cyclic(n: usize, edges: &[(u32, u32)]) -> Vec<usize> {
        CsrGraph::from_edges(n, edges)
            .nodes_in_cycles()
            .into_iter()
            .enumerate()
            .filter_map(|(i, c)| c.then_some(i))
            .collect()
    }

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
    fn acyclic_chain_has_no_cycles() {
        // 0 -> 1 -> 2
        assert_eq!(cyclic(3, &[(0, 1), (1, 2)]), Vec::<usize>::new());
    }

    #[test]
    fn self_loop_is_a_cycle() {
        // 1 -> 1
        assert_eq!(cyclic(3, &[(0, 1), (1, 1)]), vec![1]);
    }

    #[test]
    fn two_node_cycle_marks_both() {
        // 0 -> 1 -> 0
        assert_eq!(cyclic(2, &[(0, 1), (1, 0)]), vec![0, 1]);
    }

    #[test]
    fn three_node_cycle_marks_all() {
        // 0 -> 1 -> 2 -> 0
        assert_eq!(cyclic(3, &[(0, 1), (1, 2), (2, 0)]), vec![0, 1, 2]);
    }

    #[test]
    fn only_scc_members_are_marked() {
        // cycle 0<->1, plus 2 -> 0 (reaches the cycle but is not in it),
        // and 1 -> 3 (downstream, not in the cycle).
        assert_eq!(cyclic(4, &[(0, 1), (1, 0), (2, 0), (1, 3)]), vec![0, 1]);
    }

    #[test]
    fn two_disjoint_cycles() {
        // 0<->1 and 2->3->4->2; node 5 isolated.
        let edges = [(0, 1), (1, 0), (2, 3), (3, 4), (4, 2)];
        assert_eq!(cyclic(6, &edges), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn nested_scc_with_chord() {
        // SCC over {0,1,2}: 0->1->2->0 with chords 0->2 and 2->1.
        let edges = [(0, 1), (1, 2), (2, 0), (0, 2), (2, 1)];
        assert_eq!(cyclic(3, &edges), vec![0, 1, 2]);
    }

    #[test]
    fn duplicate_edges_are_handled() {
        // Parallel edges must not corrupt the adjacency or the SCC result.
        let edges = [(0, 1), (0, 1), (1, 0), (1, 0)];
        assert_eq!(cyclic(2, &edges), vec![0, 1]);
    }

    #[test]
    fn empty_graph() {
        assert_eq!(cyclic(0, &[]), Vec::<usize>::new());
        assert_eq!(cyclic(3, &[]), Vec::<usize>::new());
    }
}
