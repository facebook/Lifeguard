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
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::runner::Options;
    use lifeguard::test_lib::CacheDelivery;
    use lifeguard::test_lib::ParityFixture;
    use lifeguard::test_lib::PathRun;
    use lifeguard::test_lib::Shards;
    use lifeguard::test_lib::assert_failing;
    use lifeguard::test_lib::assert_passing;
    use lifeguard::test_lib::assert_paths_agree;
    use lifeguard::test_lib::assert_paths_agree_sharded;
    use lifeguard::test_lib::assert_paths_agree_sharded_with_options;
    use lifeguard::test_lib::partition_modules;
    use lifeguard::test_lib::path_differences;
    use lifeguard::test_lib::run_incremental_analysis_on_groups;
    use lifeguard::test_lib::run_lifeguard_analysis;
    use lifeguard::test_lib::verbose_test_options;
    use starlark_map::small_set::SmallSet;

    /// Assert the star-import gap precisely, so that any new unknown failures
    /// still show up.
    fn assert_star_import_gap(modules: &Vec<(&str, &str)>) {
        let differences = path_differences(modules, &[1, 2, 3]);

        let diverging: Vec<usize> = differences.iter().map(|(count, _)| *count).collect();
        assert_eq!(
            diverging,
            vec![2, 3],
            "one shard can expand the star locally, so the gap needs a split to appear",
        );
        for (count, difference) in &differences {
            assert!(
                difference.starts_with("passing modules:"),
                "{count} shards: expected a passing-module difference, got: {difference}",
            );
            // Located by content and relative position rather than by matching
            // the rendered label, so reformatting the message cannot turn this
            // into an assertion that quietly checks nothing.
            let whole_program = difference.find(r#"["app", "starbase", "starmid"]"#);
            let incremental = difference.find(r#"["starbase", "starmid"]"#);
            assert!(
                matches!((whole_program, incremental), (Some(w), Some(i)) if w < i),
                "{count} shards: expected whole-program to pass `app` and incremental to fail it \
                 (conservative, not a false-safe), got: {difference}",
            );
        }
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
    fn package_init_cycle_agrees() {
        // Closed only by `from mining.taxonomy import ...`, which runs `mining/__init__`.
        let judge = r#"
            from mining.taxonomy import OUTCOMES
            def classify():
                pass
        "#;
        assert_paths_agree_sharded(&[
            ("mining", "from mining.miner import classify"),
            ("mining.miner", "from judges.miner_judge import classify"),
            ("mining.taxonomy", "OUTCOMES = ()"),
            ("judges", "from judges.miner_judge import classify"),
            ("judges.miner_judge", judge),
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
    fn inherited_method_agrees() {
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
        assert_paths_agree_sharded(&vec![("base", base), ("sub", sub), ("app", app)]);
    }

    #[test]
    fn inherited_unsafe_method_agrees() {
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
        assert_paths_agree_sharded(&vec![("base", base), ("sub", sub), ("app", app)]);
    }

    #[test]
    fn overriding_method_shadows_base_agrees() {
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
        assert_paths_agree_sharded(&vec![("base", base), ("sub", sub), ("app", app)]);
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
    fn helper_calling_re_exported_mutator_agrees() {
        let hooks_impl = r#"
            HOOKS = []

            def register(hook):
                HOOKS.append(hook)
        "#;
        let hooks = r#"
            from hooks_impl import register
        "#;
        let app = r#"
            import hooks

            def register_later(hook):
                hooks.register(hook)

            register_later(print)
        "#;
        assert_paths_agree_sharded(&[("hooks_impl", hooks_impl), ("hooks", hooks), ("app", app)]);
    }

    #[test]
    fn re_exported_parameterized_decorator_agrees_in_one_library() {
        // Split across libraries, the reduce still verifies the decorator through the
        // re-export's copied verdict, which lacks the nested `wrap`.
        let deco_impl = r#"
            REGISTRY = []

            def register(name):
                def wrap(f):
                    REGISTRY.append(f)
                    return f
                return wrap
        "#;
        let deco = r#"
            from deco_impl import register
        "#;
        let app = r#"
            from deco import register

            @register("f")
            def f():
                pass
        "#;
        assert_paths_agree(
            &[("deco_impl", deco_impl), ("deco", deco), ("app", app)],
            &[1],
        );
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

    /// Attribute access on a class from another module.
    ///
    /// Accessing `PATTERN` is unsafe, since it triggers an unsafe getter,
    /// but we cannot know that when reading `fix_features.py` at map time.
    #[test]
    fn property_access_agrees_across_a_shard_boundary() {
        let feature_base = r#"
            class Features(set):
                mapping = {}

                def update_mapping(self):
                    self.mapping = dict([(f.name, f) for f in iter(self)])

                @property
                def PATTERN(self):
                    self.update_mapping()
                    return " | ".join([str(f) for f in iter(self)])
        "#;
        let fix_features = r#"
            from feature_base import Features

            class FixFeatures:
                features = Features()
                PATTERN = features.PATTERN
        "#;
        assert_paths_agree_sharded(&[
            ("feature_base", feature_base),
            ("fix_features", fix_features),
        ]);
    }

    /// The other half of the property case: a getter that touches nothing shared
    /// is safe, and reading it must stay safe on both paths. Without this, a
    /// discharge that emitted an error for every recorded access -- rather than
    /// only for a property whose getter has an unsafe verdict -- would still pass
    /// the test above.
    #[test]
    fn safe_property_access_agrees_across_a_shard_boundary() {
        let holder_base = r#"
            class Holder:
                @property
                def value(self):
                    return 1
        "#;
        let reader = r#"
            from holder_base import Holder

            holder = Holder()
            VALUE = holder.value
        "#;
        let modules = vec![("holder_base", holder_base), ("reader", reader)];
        assert_paths_agree_sharded(&modules);
        assert_passing(
            &run_lifeguard_analysis(&modules),
            vec!["holder_base", "reader"],
        );
    }

    /// The reduce uses the stub-only `make() -> C` annotation to recover the
    /// unsafe property across shards. The paths now disagree only on whether
    /// `factory.make` is an unsafe or unknown call.
    #[test]
    fn stub_declared_factory_return_differs_only_in_the_call_kind() {
        let impl_mod = r#"
            class C(set):
                mapping = {}

                def update_mapping(self):
                    self.mapping = dict([(f.name, f) for f in iter(self)])

                @property
                def p(self):
                    self.update_mapping()
                    return 1
        "#;
        let factory = r#"
            from impl_mod import C

            def make() -> C: ...
        "#;
        let app = r#"
            from factory import make

            obj = make()
            value = obj.p
        "#;
        let modules = vec![("impl_mod", impl_mod), ("factory", factory), ("app", app)];
        let differences = path_differences(
            ParityFixture::new(&modules).with_stubs(&["factory"]),
            &[1, 2, 3],
        );

        let diverging: Vec<usize> = differences.iter().map(|(count, _)| *count).collect();
        assert_eq!(
            diverging,
            vec![2, 3],
            "one shard keeps the stub and its caller in one library, so the gap needs a split",
        );
        for (count, difference) in &differences {
            assert!(
                difference.starts_with("aggregated errors:"),
                "{count} shards: expected an error-set difference, got: {difference}",
            );
            // The property error must appear on both sides: its absence was the
            // false-safe this fixture was written for.
            assert_eq!(
                difference.matches("UnsafeMethodCall impl_mod.C.p").count(),
                2,
                "{count} shards: both paths must report the property, got: {difference}",
            );
            assert!(
                difference.contains("UnsafeFunctionCall factory.make")
                    && difference.contains("UnknownFunctionCall factory.make"),
                "{count} shards: expected the call kind to be the only difference, \
                 got: {difference}",
            );
        }
    }

    /// The same recovery has to survive a re-export between the caller and the
    /// factory. The map records the return type against `factory.make`, but the
    /// candidate's receiver names the alias `facade.make`.
    #[test]
    fn a_reexported_factory_still_recovers_the_property() {
        let impl_mod = r#"
            class C(set):
                mapping = {}

                def update_mapping(self):
                    self.mapping = dict([(f.name, f) for f in iter(self)])

                @property
                def p(self):
                    self.update_mapping()
                    return 1
        "#;
        let factory = r#"
            from impl_mod import C

            def make() -> C: ...
        "#;
        let facade = r#"
            from factory import make
        "#;
        let app = r#"
            from facade import make

            obj = make()
            value = obj.p
        "#;
        let modules = vec![
            ("impl_mod", impl_mod),
            ("factory", factory),
            ("facade", facade),
            ("app", app),
        ];
        let differences = path_differences(
            ParityFixture::new(&modules).with_stubs(&["factory"]),
            &[1, 2, 3],
        );

        for (count, difference) in &differences {
            assert_eq!(
                difference.matches("UnsafeMethodCall impl_mod.C.p").count(),
                2,
                "{count} shards: both paths must report the property through the \
                 re-export, got: {difference}",
            );
        }
    }

    /// A `__main__` guard only runs in the module the binary starts from, and
    /// only the reduce knows which that is. So the map analyzes the guard body
    /// and marks what it produces, and the reduce drops the marked facts
    /// everywhere except the entry module.
    ///
    /// `mixed` is the precision half: a filter that dropped a non-entry
    /// module's facts wholesale would still give `library` the right verdict,
    /// and only an unguarded error in the same module says otherwise.
    #[test]
    fn main_guard_facts_apply_only_to_the_entry_module() {
        let unsafe_guard = r#"
            def _run():
                raise RuntimeError("boom")

            if __name__ == "__main__":
                _run()
        "#;
        let mixed = r#"
            def _run():
                raise RuntimeError("boom")

            _run()

            if __name__ == "__main__":
                _run()
        "#;
        let modules = vec![
            ("entry", unsafe_guard),
            ("library", unsafe_guard),
            ("mixed", mixed),
        ];

        let (passing, failing) = verdicts_with_entry_as_main(&modules, Shards::new(1));

        assert_eq!(
            failing,
            vec!["entry".to_owned(), "mixed".to_owned()],
            "the entry module runs its guard, and `mixed` fails on its unguarded call",
        );
        assert!(
            passing.contains(&"library".to_owned()),
            "the same code in a non-entry module never runs: {passing:?}",
        );
    }

    /// `ExecCall` is a `force_imports_eager_overrides` record rather than an
    /// error, and it lands in a different output field, so the filter has to
    /// reach it separately.
    #[test]
    fn main_guard_eager_overrides_apply_only_to_the_entry_module() {
        let exec_in_guard = r#"
            if __name__ == "__main__":
                exec("x = 1")
        "#;
        let modules = vec![("entry", exec_in_guard), ("library", exec_in_guard)];

        let options = entry_as_main_options();
        assert_paths_agree_sharded_with_options(&modules, &options);
        let run = run_serialized(&modules, Shards::new(1), &options);

        let eager: Vec<String> = sorted_names(&run.analysis().output.load_imports_eagerly);
        assert_eq!(
            eager,
            vec!["entry".to_owned()],
            "only the entry module actually reaches its `exec`",
        );
    }

    /// An import written only inside a `__main__` guard is not a dependency of
    /// any module but the entry one -- the body never runs there, so nothing has
    /// to be loaded eagerly for it. Unlike a `def`-local import, this needs no
    /// call analysis to establish: a function defined inside the guard is never
    /// even defined.
    ///
    /// An edge is a set member, not an occurrence, so its provenance is a set
    /// the module carries rather than a flag on the edge itself.
    #[test]
    fn main_guard_import_edges_apply_only_to_the_entry_module() {
        let unsafe_mod = r#"
            def _run():
                raise RuntimeError("boom")

            _run()
        "#;
        let importer = r#"
            if __name__ == "__main__":
                import unsafe_mod
        "#;
        let modules = vec![
            ("unsafe_mod", unsafe_mod),
            ("entry", importer),
            ("library", importer),
        ];

        let options = entry_as_main_options();
        assert_paths_agree_sharded_with_options(&modules, &options);
        let run = run_serialized(&modules, Shards::new(1), &options);
        let deps = |module: &str| {
            run.analysis()
                .output
                .lazy_eligible
                .get(&ModuleName::from_str(module))
                .map(|set| set.iter().map(|n| n.as_str().to_owned()).collect())
                .unwrap_or_else(Vec::new)
        };

        // The control: without the edge reaching the reduce at all, neither
        // module would name it and the assertion below would pass vacuously.
        assert!(
            deps("entry").contains(&"unsafe_mod".to_owned()),
            "the entry module runs its guard, so the import is a real dependency: {:?}",
            deps("entry"),
        );
        assert!(
            !deps("library").contains(&"unsafe_mod".to_owned()),
            "the same import in a non-entry module never runs: {:?}",
            deps("library"),
        );
    }

    /// A mutation candidate is not an error when the guard filter runs: the
    /// reduce confirms it later and *then* emits `ImportedVarArgument`.
    /// Filtering only the errors the map recorded would let a guarded call back
    /// in through that discharge.
    #[test]
    fn main_guard_mutation_candidates_apply_only_to_the_entry_module() {
        let setup = r#"
            def configure(x):
                x.enabled = True
        "#;
        let caller = r#"
            from setup import configure
            from config import settings

            if __name__ == "__main__":
                configure(settings)
        "#;
        let modules = vec![("setup", setup), ("entry", caller), ("library", caller)];

        // Three shards put `setup` in neither caller's library, so the mutation
        // has to travel as a candidate rather than as a local effect.
        let (passing, failing) = verdicts_with_entry_as_main(&modules, Shards::new(3));

        assert!(
            failing.contains(&"entry".to_owned()),
            "the entry module runs the call, so the confirmed candidate stands: {failing:?}",
        );
        assert!(
            passing.contains(&"library".to_owned()),
            "the same call in a non-entry module never runs: {passing:?}",
        );
    }

    /// The same for a property candidate, which the reduce resolves even later
    /// -- after error clearing has finished.
    #[test]
    fn main_guard_property_candidates_apply_only_to_the_entry_module() {
        let feature_base = r#"
            class Features(set):
                mapping = {}

                def update_mapping(self):
                    self.mapping = dict([(f.name, f) for f in iter(self)])

                @property
                def PATTERN(self):
                    self.update_mapping()
                    return " | ".join([str(f) for f in iter(self)])
        "#;
        let reader = r#"
            from feature_base import Features

            if __name__ == "__main__":
                features = Features()
                PATTERN = features.PATTERN
        "#;
        let modules = vec![
            ("feature_base", feature_base),
            ("entry", reader),
            ("library", reader),
        ];

        // Neither reader may share a library with `feature_base`: a reader that
        // can see the class records the effect directly and never reaches the
        // obligation path this test is about.
        let (passing, failing) = verdicts_with_entry_as_main(&modules, Shards::new(3));

        assert!(
            failing.contains(&"entry".to_owned()),
            "the entry module reads the property, so the obligation stands: {failing:?}",
        );
        assert!(
            passing.contains(&"library".to_owned()),
            "the same read in a non-entry module never happens: {passing:?}",
        );
    }

    /// Verbose, so that the path comparison covers implicit imports and cycles
    /// too -- the guard body is analyzed either way, so what it contributes to
    /// those has to match as well.
    fn entry_as_main_options() -> Options {
        Options {
            main_module: Some(ModuleName::from_str("entry")),
            ..verbose_test_options()
        }
    }

    /// Through the wire format, the way Buck delivers caches, so the provenance
    /// the reduce filters on has to survive encoding and not just exist in
    /// memory.
    fn run_serialized(modules: &Vec<(&str, &str)>, shards: Shards, options: &Options) -> PathRun {
        let groups = partition_modules(modules, shards);
        run_incremental_analysis_on_groups(&groups, CacheDelivery::Serialized, options)
    }

    fn sorted_names(names: &SmallSet<ModuleName>) -> Vec<String> {
        let mut out: Vec<String> = names.iter().map(|n| n.as_str().to_owned()).collect();
        out.sort();
        out
    }

    /// Which modules pass and which fail on the incremental path, with `entry`
    /// named as the module the binary runs as `__main__`.
    ///
    /// Also asserts the whole-program path agrees: it reaches the same verdicts
    /// by pruning the guard out of the AST, which is the reading the reduce-time
    /// filter is supposed to reproduce.
    fn verdicts_with_entry_as_main(
        modules: &Vec<(&str, &str)>,
        shards: Shards,
    ) -> (Vec<String>, Vec<String>) {
        let options = entry_as_main_options();
        assert_paths_agree_sharded_with_options(modules, &options);
        let run = run_serialized(modules, shards, &options);
        (
            sorted_names(&run.analysis().summary.passing_modules),
            sorted_names(&run.analysis().summary.failing_modules),
        )
    }

    #[test]
    fn reexported_method_shadowed_by_field_is_unsafe() {
        for shadow in [
            "method = staticmethod(lambda: print('effect'))",
            "method = None",
            "if True:\n  method = None",
            "method, other = None, 0",
            "from builtins import print as method",
            "if True:\n  method: object = None",
        ] {
            for class_body in [
                format!("class Sub(Base):\n {shadow}\n"),
                format!("class Middle(Base):\n {shadow}\nclass Sub(Middle):\n pass\n"),
            ] {
                let origin =
                    format!("class Base:\n @staticmethod\n def method():\n  pass\n{class_body}");
                let modules = [
                    ("origin", origin.as_str()),
                    ("facade", "from origin import Sub\n"),
                    ("app", "from facade import Sub\nSub.method()\n"),
                ];
                assert_failing(&run_lifeguard_analysis(&modules.to_vec()), vec!["app"]);
                assert_paths_agree_sharded(&modules);
            }
        }
    }
}
