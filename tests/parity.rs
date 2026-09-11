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

    /// Assert a known gap precisely, so that any new unknown failures still show up.
    fn assert_known_gap(
        modules: &Vec<(&str, &str)>,
        whole_program_passing: &str,
        incremental_passing: &str,
        single_shard_reason: &str,
    ) {
        let differences = path_differences(modules, &[1, 2, 3]);

        let diverging: Vec<usize> = differences.iter().map(|(count, _)| *count).collect();
        assert_eq!(diverging, vec![2, 3], "{}", single_shard_reason);
        for (count, difference) in &differences {
            assert!(
                difference.starts_with("passing modules:"),
                "{count} shards: expected a passing-module difference, got: {difference}",
            );
            // Located by content and relative position rather than by matching
            // the rendered label, so reformatting the message cannot turn this
            // into an assertion that quietly checks nothing.
            let whole_program = difference.find(whole_program_passing);
            let incremental = difference.find(incremental_passing);
            assert!(
                matches!((whole_program, incremental), (Some(w), Some(i)) if w < i),
                "{count} shards: expected whole-program to pass `app` and incremental to fail it \
                 (conservative, not a false-safe), got: {difference}",
            );
        }
    }

    /// The two star fixtures diverge identically: the symbol's own safety does
    /// not reach the outcome while the import is unresolved.
    fn assert_star_import_gap(modules: &Vec<(&str, &str)>) {
        assert_known_gap(
            modules,
            r#"["app", "starbase", "starmid"]"#,
            r#"["starbase", "starmid"]"#,
            "one shard can expand the star locally, so the gap needs a split to appear",
        );
    }

    /// The chained-call fixtures below diverge the same way: A shard holding `app`
    /// alone falls back to `UnknownFunctionCall <chained method>`, which the
    /// reduce cannot resolve because it names no callee.
    fn assert_chained_call_gap(modules: &Vec<(&str, &str)>) {
        assert_known_gap(
            modules,
            r#"["app", "base", "sub"]"#,
            r#"["base", "sub"]"#,
            "one shard keeps every module in one library, so the gap needs a split to appear",
        );
    }

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

    /// KNOWN GAP (T288043641) -- cross-library parameter forwarding.
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
    fn parent_fallback_shadowing_agrees() {
        // `pkg.sub` is a real module, so `import pkg.sub` must resolve exactly.
        // A shard holding `pkg` but not `pkg.sub` can only resolve the import to
        // the parent, and the reduce has to refine that once the exact module
        // shows up. Committing the parent early would attach the dependency to
        // the wrong module.
        let pkg = r#"
            PARENT = 1
        "#;
        let pkg_sub = r#"
            import os

            CHILD = os.environ['HOME']
        "#;
        let app = r#"
            import pkg.sub

            value = pkg.sub.CHILD
        "#;
        assert_paths_agree_sharded(&[("pkg", pkg), ("pkg.sub", pkg_sub), ("app", app)]);
    }

    #[test]
    fn ambiguous_from_import_agrees() {
        // `from pkg import thing` is ambiguous in a shard that has `pkg` but not
        // `pkg.thing`: it could be a submodule or an attribute of `pkg`. Here it
        // is a submodule, so the reduce must resolve it to one once both are
        // present. `attr` is the other reading, kept alongside so a fix that
        // simply treats every ambiguous name as a submodule fails.
        let pkg = r#"
            attr = 1
        "#;
        let pkg_thing = r#"
            import os

            VALUE = os.environ['HOME']
        "#;
        let app = r#"
            from pkg import thing
            from pkg import attr
        "#;
        assert_paths_agree_sharded(&[("pkg", pkg), ("pkg.thing", pkg_thing), ("app", app)]);
    }

    /// KNOWN GAP (T288043164) -- star-import expansion.
    #[test]
    fn star_import_is_a_known_gap() {
        let starbase = r#"
            def helper():
                return 1

            VALUE = 2
        "#;
        let starmid = r#"
            from starbase import *
        "#;
        let app = r#"
            from starmid import helper

            value = helper()
        "#;
        assert_star_import_gap(&vec![
            ("starbase", starbase),
            ("starmid", starmid),
            ("app", app),
        ]);
    }

    /// The same gap as [`star_import_is_a_known_gap`], with the star-imported
    /// symbol unsafe rather than safe. Kept separate because the two exercise
    /// different verdicts once the reduce can discharge the obligation.
    #[test]
    fn star_import_of_unsafe_symbol_is_a_known_gap() {
        let starbase = r#"
            import os

            def helper():
                return os.environ['HOME']
        "#;
        let starmid = r#"
            from starbase import *
        "#;
        let app = r#"
            from starmid import helper

            value = helper()
        "#;
        assert_star_import_gap(&vec![
            ("starbase", starbase),
            ("starmid", starmid),
            ("app", app),
        ]);
    }

    #[test]
    fn inherited_method_is_a_known_gap() {
        // Calling `Sub.method` resolves through the MRO to a base class in
        // another module, so the reduce has to complete the linearization from
        // cached class bases rather than from a local class table.
        let base = r#"
            class Base:
                def method(self):
                    return 1
        "#;
        let sub = r#"
            from base import Base

            class Sub(Base):
                pass
        "#;
        let app = r#"
            from sub import Sub

            value = Sub().method()
        "#;
        assert_chained_call_gap(&vec![("base", base), ("sub", sub), ("app", app)]);
    }

    #[test]
    fn inherited_unsafe_method_is_a_known_gap() {
        let base = r#"
            import os

            class Base:
                def method(self):
                    return os.environ['HOME']
        "#;
        let sub = r#"
            from base import Base

            class Sub(Base):
                pass
        "#;
        let app = r#"
            from sub import Sub

            value = Sub().method()
        "#;
        assert_chained_call_gap(&vec![("base", base), ("sub", sub), ("app", app)]);
    }

    #[test]
    fn overriding_method_shadows_base_is_a_known_gap() {
        // The override must win over the inherited method in both paths; the
        // reduce walks the MRO only when the class has no entry of its own.
        let base = r#"
            class Base:
                def method(self):
                    return 1
        "#;
        let sub = r#"
            import os
            from base import Base

            class Sub(Base):
                def method(self):
                    return os.environ['HOME']
        "#;
        let app = r#"
            from sub import Sub

            value = Sub().method()
        "#;
        assert_chained_call_gap(&vec![("base", base), ("sub", sub), ("app", app)]);
    }

    #[test]
    fn re_export_chain_agrees() {
        let origin = r#"
            import os

            def thing():
                return os.environ['HOME']
        "#;
        let hop_one = r#"
            from origin import thing
        "#;
        let hop_two = r#"
            from hop_one import thing
        "#;
        let app = r#"
            from hop_two import thing

            value = thing()
        "#;
        assert_paths_agree_sharded(&[
            ("origin", origin),
            ("hop_one", hop_one),
            ("hop_two", hop_two),
            ("app", app),
        ]);
    }

    #[test]
    fn re_export_cycle_agrees() {
        // A cycle in the re-export graph: both paths resolve chains, and both
        // have to terminate rather than loop.
        let cyc_a = r#"
            from cyc_b import thing
        "#;
        let cyc_b = r#"
            from cyc_a import thing
        "#;
        let app = r#"
            from cyc_a import thing
        "#;
        assert_paths_agree_sharded(&[("cyc_a", cyc_a), ("cyc_b", cyc_b), ("app", app)]);
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
