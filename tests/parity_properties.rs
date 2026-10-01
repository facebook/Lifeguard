/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Properties the reduce must satisfy for a hierarchical, cacheable topology to
//! be possible at all: the result may not depend on how modules were grouped
//! into shards, on the order the shards arrive, on a shard arriving twice, or on
//! whether the caches travelled through the wire format.
//!
//! These gate the later migration stages. A change that makes the reduce
//! sensitive to grouping or arrival order will pass the fixed-shape parity
//! fixtures in `parity.rs` and fail here.

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::CacheDelivery;
    use lifeguard::test_lib::Shards;
    use lifeguard::test_lib::TestRng;
    use lifeguard::test_lib::assert_analyses_agree;
    use lifeguard::test_lib::partition_modules;
    use lifeguard::test_lib::random_partition;
    use lifeguard::test_lib::run_incremental_analysis_on_groups;
    use lifeguard::test_lib::run_whole_program_path;
    use lifeguard::test_lib::verbose_test_options;

    /// A program with enough cross-module structure that a partition can cut
    /// through call chains, re-exports, class hierarchies and import cycles.
    /// Deliberately avoids the two constructs known to diverge across shards
    /// (star imports and two-hop parameter forwarding); those are tracked as
    /// known gaps in `parity.rs`, and including them here would just reproduce
    /// them in every property.
    fn corpus() -> Vec<(&'static str, &'static str)> {
        let leaf_safe = r#"
            def helper():
                return 1
        "#;
        let leaf_unsafe = r#"
            import os

            CONFIG = os.environ['HOME']
        "#;
        let mid_calls_leaf = r#"
            from leaf_safe import helper

            def middle():
                return helper()
        "#;
        let mid_reexports = r#"
            from leaf_unsafe import CONFIG
        "#;
        let base_class = r#"
            class Base:
                def method(self):
                    return 1
        "#;
        let derived_class = r#"
            from base_class import Base

            class Derived(Base):
                pass
        "#;
        let uses_class = r#"
            from derived_class import Derived

            value = Derived().method()
        "#;
        let cycle_one = r#"
            import cycle_two

            ONE = 1
        "#;
        let cycle_two = r#"
            import cycle_one

            TWO = 2
        "#;
        let has_finalizer = r#"
            class Leaker:
                def __del__(self):
                    pass
        "#;
        let uses_exec = r#"
            exec('generated = 1')
        "#;
        let pkg_parent = r#"
            PARENT = 1
        "#;
        let pkg_parent_child = r#"
            CHILD = 2
        "#;
        let app = r#"
            import pkg_parent.child
            from mid_calls_leaf import middle
            from mid_reexports import CONFIG

            result = middle()
        "#;

        vec![
            ("leaf_safe", leaf_safe),
            ("leaf_unsafe", leaf_unsafe),
            ("mid_calls_leaf", mid_calls_leaf),
            ("mid_reexports", mid_reexports),
            ("base_class", base_class),
            ("derived_class", derived_class),
            ("uses_class", uses_class),
            ("cycle_one", cycle_one),
            ("cycle_two", cycle_two),
            ("has_finalizer", has_finalizer),
            ("uses_exec", uses_exec),
            ("pkg_parent", pkg_parent),
            ("pkg_parent.child", pkg_parent_child),
            ("app", app),
        ]
    }

    /// Grouping invariance: any partition must reduce to the whole-program
    /// result. This is the property the whole map-reduce design rests on.
    #[test]
    fn random_partitions_agree_with_whole_program() {
        let modules = corpus();
        let options = verbose_test_options();
        let whole_program = run_whole_program_path(&modules, &options);

        // Fixed seed: a failure must be reproducible from the message alone.
        let mut rng = TestRng::new(0x5EED);
        for round in 0..24 {
            let shards = 1 + rng.below(4);
            let groups = random_partition(&modules, shards, &mut rng);
            let incremental =
                run_incremental_analysis_on_groups(&groups, CacheDelivery::InMemory, &options);
            let sizes: Vec<usize> = groups.iter().map(|g| g.len()).collect();
            assert_analyses_agree(
                &format!("round {round}, shard sizes {sizes:?}"),
                &whole_program,
                &incremental,
            );
        }
    }

    /// Merge order invariance: the reduce takes a set of caches, so the order
    /// they are handed over must not matter. `ReduceWorkspace::merge` preserves
    /// input order deliberately, because duplicate-module mutation candidates are
    /// order-sensitive -- this pins that the preserved order does not reach the
    /// result.
    #[test]
    fn shard_delivery_order_does_not_matter() {
        let modules = corpus();
        let options = verbose_test_options();
        let groups = partition_modules(&modules, Shards::new(4));

        let reference =
            run_incremental_analysis_on_groups(&groups, CacheDelivery::InMemory, &options);

        let mut rng = TestRng::new(0xA11CE);
        for round in 0..12 {
            let mut shuffled = groups.clone();
            // Fisher-Yates.
            for i in (1..shuffled.len()).rev() {
                shuffled.swap(i, rng.below(i + 1));
            }
            let permuted =
                run_incremental_analysis_on_groups(&shuffled, CacheDelivery::InMemory, &options);
            assert_analyses_agree(
                &format!("shard permutation, round {round}"),
                &reference,
                &permuted,
            );
        }
    }

    /// Idempotence under duplicate delivery: a diamond in the dependency graph
    /// can deliver one library's cache more than once, so merging a shard twice
    /// must not change the result.
    #[test]
    fn duplicate_shard_delivery_does_not_matter() {
        let modules = corpus();
        let options = verbose_test_options();
        let groups = partition_modules(&modules, Shards::new(3));

        let reference =
            run_incremental_analysis_on_groups(&groups, CacheDelivery::InMemory, &options);

        for duplicated in 0..groups.len() {
            let mut with_duplicate = groups.clone();
            with_duplicate.push(groups[duplicated].clone());
            let actual = run_incremental_analysis_on_groups(
                &with_duplicate,
                CacheDelivery::InMemory,
                &options,
            );
            assert_analyses_agree(
                &format!("shard {duplicated} delivered twice"),
                &reference,
                &actual,
            );
        }
    }

    /// Serialization transparency: routing every cache through the wire format,
    /// the way Buck does, must not change the result. This is the round-trip
    /// property stated end to end rather than per field.
    #[test]
    fn serialized_caches_match_in_memory_caches() {
        let modules = corpus();
        let options = verbose_test_options();

        for shards in 1..=4 {
            let groups = partition_modules(&modules, Shards::new(shards));
            let in_memory =
                run_incremental_analysis_on_groups(&groups, CacheDelivery::InMemory, &options);
            let serialized =
                run_incremental_analysis_on_groups(&groups, CacheDelivery::Serialized, &options);
            assert_analyses_agree(
                &format!("{shards} shard(s), serialized vs in-memory"),
                &in_memory,
                &serialized,
            );
        }
    }

    /// Sharding a program more finely must not change its result either -- this
    /// is grouping invariance restated as a refinement, which is the shape a
    /// hierarchical reduce will need.
    #[test]
    fn shard_count_does_not_matter() {
        let modules = corpus();
        let options = verbose_test_options();
        let reference = run_incremental_analysis_on_groups(
            &partition_modules(&modules, Shards::new(1)),
            CacheDelivery::InMemory,
            &options,
        );

        for shards in 2..=6 {
            let actual = run_incremental_analysis_on_groups(
                &partition_modules(&modules, Shards::new(shards)),
                CacheDelivery::InMemory,
                &options,
            );
            assert_analyses_agree(&format!("{shards} shards vs 1"), &reference, &actual);
        }
    }
}
