/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Cross-path parity: the whole-program and incremental analyses are supposed to
//! be one analysis differing only in scheduling, so any fixture should produce
//! the same output through either.
//!
//! Each test runs its modules through the whole-program path and through the
//! map/reduce path at one, two and three shards. Sharding is the part that
//! matters: at one shard the map phase still sees every module, so nothing
//! crosses a library boundary. Splitting the same modules forces the facts
//! through serialization, missing-import obligations, and the merge.

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::assert_paths_agree_sharded;
    use lifeguard::test_lib::path_differences;

    #[test]
    fn mixed_safe_and_unsafe_modules_agree() {
        let safe_module = r#"
            def greet(name):
                return name
        "#;
        let unsafe_module = r#"
            import os

            result = os.environ['HOME']

            def helper():
                return result
        "#;
        let importer = r#"
            from safe_module import greet
            from unsafe_module import helper
        "#;
        let has_finalizer = r#"
            class Leaker:
                def __del__(self):
                    pass
        "#;
        let uses_exec = r#"
            exec('x = 1')
        "#;
        let main = r#"
            from importer import greet

            def main():
                return greet('world')
        "#;
        assert_paths_agree_sharded(&[
            ("safe_module", safe_module),
            ("unsafe_module", unsafe_module),
            ("importer", importer),
            ("has_finalizer", has_finalizer),
            ("uses_exec", uses_exec),
            ("main", main),
        ]);
    }

    #[test]
    fn cross_module_call_chain_agrees() {
        // A call chain that has to be resolved across shard boundaries: `app`
        // calls into `mid`, which calls into `base`.
        let base = r#"
            def leaf():
                return 1
        "#;
        let mid = r#"
            from base import leaf

            def middle():
                return leaf()
        "#;
        let app = r#"
            from mid import middle

            value = middle()
        "#;
        assert_paths_agree_sharded(&[("base", base), ("mid", mid), ("app", app)]);
    }

    #[test]
    fn unsafe_call_chain_agrees() {
        // The same shape, but the leaf has an import-time side effect, so the
        // verdict has to propagate back up through the shards.
        let base = r#"
            import os

            def leaf():
                return os.environ['HOME']
        "#;
        let mid = r#"
            from base import leaf

            def middle():
                return leaf()
        "#;
        let app = r#"
            from mid import middle

            value = middle()
        "#;
        assert_paths_agree_sharded(&[("base", base), ("mid", mid), ("app", app)]);
    }

    #[test]
    fn import_cycle_agrees() {
        let cycle_a = r#"
            import cycle_b

            def a():
                return 1
        "#;
        let cycle_b = r#"
            import cycle_a

            def b():
                return 2
        "#;
        let cycle_user = r#"
            import cycle_a
        "#;
        assert_paths_agree_sharded(&[
            ("cycle_a", cycle_a),
            ("cycle_b", cycle_b),
            ("cycle_user", cycle_user),
        ]);
    }

    #[test]
    fn module_scope_side_effects_agree() {
        let plain = r#"
            VALUE = 1
        "#;
        let reads_sys_modules = r#"
            import sys

            mod = sys.modules['plain']
        "#;
        let calls_exec = r#"
            exec('y = 2')
        "#;
        let mutates_import = r#"
            import plain

            plain.VALUE = 2
        "#;
        assert_paths_agree_sharded(&[
            ("plain", plain),
            ("reads_sys_modules", reads_sys_modules),
            ("calls_exec", calls_exec),
            ("mutates_import", mutates_import),
        ]);
    }

    /// KNOWN GAP -- cross-library parameter forwarding.
    ///
    /// `app` passes an imported module into `forward`, which forwards it into
    /// `sink`, which mutates it. Seeing that requires combining facts from three
    /// modules, and the map phase serializes only its own action-local
    /// mutated-parameter fixpoint, not the forwarding edges that would let the
    /// reduce finish the closure. So once `sink`, `midlib` and `app` land in
    /// different shards the mutation becomes invisible.
    ///
    /// The divergence is a false-safe, which is the dangerous direction: the
    /// whole-program path fails `app`, the incremental path passes it. Closing it
    /// needs the map to emit forwarding edges for the reduce to close over.
    ///
    /// Asserted as a precise disagreement so the gap stays visible without a
    /// red build. Anything other than this exact divergence fails.
    #[test]
    fn two_hop_cross_library_forwarding_is_a_known_gap() {
        let other = r#"
            VALUE = 1
        "#;
        let sinklib = r#"
            def sink(x):
                x.attr = 1
        "#;
        let midlib = r#"
            from sinklib import sink

            def forward(y):
                sink(y)
        "#;
        let app = r#"
            import other
            from midlib import forward

            forward(other)
        "#;
        let differences = path_differences(
            &[
                ("other", other),
                ("sinklib", sinklib),
                ("midlib", midlib),
                ("app", app),
            ],
            &[1, 2, 3],
        );

        let diverging: Vec<usize> = differences.iter().map(|(count, _)| *count).collect();
        assert_eq!(
            diverging,
            vec![2, 3],
            "one shard keeps every module in one library, so the gap needs a split to appear",
        );
        for (count, difference) in &differences {
            assert!(
                difference.starts_with("passing modules:"),
                "{count} shards: expected a passing-module difference, got: {difference}",
            );
            // Located by content and relative position rather than by matching
            // the rendered label, so that reformatting the message cannot turn
            // this into an assertion that quietly checks nothing.
            let whole_program = difference.find(r#"["midlib", "other", "sinklib"]"#);
            let incremental = difference.find(r#"["app", "midlib", "other", "sinklib"]"#);
            assert!(
                matches!((whole_program, incremental), (Some(w), Some(i)) if w < i),
                "{count} shards: expected whole-program to fail `app` and incremental to pass it \
                 (a false-safe), got: {difference}",
            );
        }
    }

    #[test]
    fn single_hop_cross_library_mutation_agrees() {
        // The one-hop version of the gap above, kept as a live assertion: `app`
        // passes an imported module straight into the mutating callee, which the
        // reduce does resolve through cached mutation candidates. Together the
        // two tests pin where the boundary sits.
        let other = r#"
            VALUE = 1
        "#;
        let sinklib = r#"
            def sink(x):
                x.attr = 1
        "#;
        let app = r#"
            import other
            from sinklib import sink

            sink(other)
        "#;
        assert_paths_agree_sharded(&[("other", other), ("sinklib", sinklib), ("app", app)]);
    }
}
