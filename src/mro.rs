/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! C3 linearization of class hierarchies, matching CPython's method resolution
//! order. Reduce-time inherited-method resolution needs the same order the
//! runtime would use: a naive depth-first walk can, in diamond hierarchies,
//! attribute a method to a shared ancestor that C3 would have shadowed by a
//! right-hand branch, silently picking the wrong verdict.

use std::collections::HashMap;

use pyrefly_python::module_name::ModuleName;

/// The C3 linearization (method resolution order) of `class_fqn`, most-derived
/// first (starting with `class_fqn` itself). `class_bases` maps a class FQN to
/// its direct base FQNs; a class absent from the map (or with no bases) is a
/// leaf. Bases outside the analyzed set simply behave as leaves.
///
/// Robust to malformed input: import cycles are broken by a per-path guard, and
/// an inconsistent hierarchy (one with no valid C3 order — which CPython would
/// reject at class creation) still terminates by forcing progress rather than
/// looping.
pub fn c3_linearize(
    class_bases: &HashMap<ModuleName, Vec<ModuleName>>,
    class_fqn: &ModuleName,
) -> Vec<ModuleName> {
    let mut memo: HashMap<ModuleName, Vec<ModuleName>> = HashMap::new();
    linearize(class_bases, *class_fqn, &mut memo, &mut Vec::new())
}

fn linearize(
    class_bases: &HashMap<ModuleName, Vec<ModuleName>>,
    class: ModuleName,
    memo: &mut HashMap<ModuleName, Vec<ModuleName>>,
    on_path: &mut Vec<ModuleName>,
) -> Vec<ModuleName> {
    if let Some(cached) = memo.get(&class) {
        return cached.clone();
    }
    // A class already on the current recursion path is an inheritance cycle;
    // stop descending so linearization terminates.
    if on_path.contains(&class) {
        return vec![class];
    }
    let bases = match class_bases.get(&class) {
        Some(bases) if !bases.is_empty() => bases,
        _ => return vec![class],
    };

    on_path.push(class);
    let mut sequences: Vec<Vec<ModuleName>> = bases
        .iter()
        .map(|base| linearize(class_bases, *base, memo, on_path))
        .collect();
    on_path.pop();
    sequences.push(bases.clone());

    let mut result = vec![class];
    for candidate in c3_merge(sequences) {
        if !result.contains(&candidate) {
            result.push(candidate);
        }
    }
    memo.insert(class, result.clone());
    result
}

/// Merge base linearizations per the C3 rule: repeatedly take the head of some
/// sequence that appears in no other sequence's tail, removing it from every
/// front. On an inconsistent hierarchy no such head exists; rather than fail we
/// force the first available head so the merge still terminates.
fn c3_merge(mut sequences: Vec<Vec<ModuleName>>) -> Vec<ModuleName> {
    let mut result = Vec::new();
    loop {
        sequences.retain(|seq| !seq.is_empty());
        if sequences.is_empty() {
            return result;
        }

        let head = sequences
            .iter()
            .map(|seq| seq[0])
            .find(|head| !sequences.iter().any(|seq| seq[1..].contains(head)))
            .unwrap_or(sequences[0][0]);

        result.push(head);
        for seq in &mut sequences {
            if seq.first() == Some(&head) {
                seq.remove(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mn(s: &str) -> ModuleName {
        ModuleName::from_str(s)
    }

    fn bases(pairs: &[(&str, &[&str])]) -> HashMap<ModuleName, Vec<ModuleName>> {
        pairs
            .iter()
            .map(|(class, bases)| (mn(class), bases.iter().map(|b| mn(b)).collect()))
            .collect()
    }

    #[test]
    fn leaf_class_linearizes_to_itself() {
        let class_bases = bases(&[]);
        assert_eq!(c3_linearize(&class_bases, &mn("A")), vec![mn("A")]);
    }

    #[test]
    fn linear_chain_is_most_derived_first() {
        // C -> B -> A
        let class_bases = bases(&[("C", &["B"]), ("B", &["A"])]);
        assert_eq!(
            c3_linearize(&class_bases, &mn("C")),
            vec![mn("C"), mn("B"), mn("A")],
        );
    }

    #[test]
    fn simple_diamond_orders_branches_before_shared_ancestor() {
        // D(B, C); B -> A; C -> A. C3 = [D, B, C, A].
        // A naive depth-first walk would instead reach A (via B) before C,
        // which is exactly the divergence this module fixes.
        let class_bases = bases(&[("D", &["B", "C"]), ("B", &["A"]), ("C", &["A"])]);
        assert_eq!(
            c3_linearize(&class_bases, &mn("D")),
            vec![mn("D"), mn("B"), mn("C"), mn("A")],
        );
    }

    #[test]
    fn canonical_c3_example() {
        // The example from the Python C3 documentation.
        let class_bases = bases(&[
            ("A", &["O"]),
            ("B", &["O"]),
            ("C", &["O"]),
            ("D", &["O"]),
            ("E", &["O"]),
            ("K1", &["A", "B", "C"]),
            ("K2", &["D", "B", "E"]),
            ("K3", &["D", "A"]),
            ("Z", &["K1", "K2", "K3"]),
        ]);
        assert_eq!(
            c3_linearize(&class_bases, &mn("Z")),
            vec![
                mn("Z"),
                mn("K1"),
                mn("K2"),
                mn("K3"),
                mn("D"),
                mn("A"),
                mn("B"),
                mn("C"),
                mn("E"),
                mn("O"),
            ],
        );
    }

    #[test]
    fn bases_outside_the_map_are_treated_as_leaves() {
        // `external.Base` has no entry; it linearizes to itself and stops.
        let class_bases = bases(&[("sub.Sub", &["external.Base"])]);
        assert_eq!(
            c3_linearize(&class_bases, &mn("sub.Sub")),
            vec![mn("sub.Sub"), mn("external.Base")],
        );
    }

    #[test]
    fn inheritance_cycle_terminates() {
        // A -> B -> A. Must not loop; each class still appears once.
        let class_bases = bases(&[("A", &["B"]), ("B", &["A"])]);
        let mro = c3_linearize(&class_bases, &mn("A"));
        assert!(mro.contains(&mn("A")));
        assert!(mro.contains(&mn("B")));
        assert_eq!(mro.iter().filter(|&&c| c == mn("A")).count(), 1);
    }
}
