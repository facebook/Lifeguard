/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Call graph construction and dependency-ordered safety precomputation.

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use ruff_text_size::TextRange;
use tracing::debug;

use crate::csr_graph::CsrGraph;
use crate::effects::Effect;
use crate::effects::EffectKind;
use crate::errors::is_parameterized_decorator_effect;
use crate::hasher::AHashMap;
use crate::hasher::HashMapExt;
use crate::manual_override;
use crate::module_safety::FunctionSafety;
use crate::module_safety::FunctionSafetyInfo;
use crate::project::calls::Call;
use crate::project::calls::CallStack;
use crate::project::info::ProjectInfo;
use crate::project::state::GlobalAnalysisState;
use crate::tracing::time;

/// The call graph over every function and class, with its cycle members already
/// identified. Retained after cycle marking because the same graph gives the
/// dependency order the verdict precompute runs in.
pub(super) struct CallGraph {
    csr: CsrGraph,
    /// Node index to name.
    names: Vec<ModuleName>,
    /// Whether each node participates in a call cycle.
    in_cycle: Vec<bool>,
}

impl ProjectInfo {
    /// The callee a call effect dispatches to, or `None` when the project view
    /// cannot name one. A `super()` call or a class statement resolves through the
    /// MRO above the name it was recorded under, which is never the callee.
    fn call_graph_target(
        &self,
        eff: &Effect,
        is_node: impl Fn(&ModuleName) -> bool,
    ) -> Option<ModuleName> {
        match eff.kind {
            EffectKind::SuperMethodCall | EffectKind::ClassCreation => {
                self.resolve_dispatch_above(&eff.name)
            }
            _ if is_node(&eff.name) => Some(eff.name),
            _ => self.resolve_callable(&eff.name),
        }
    }

    /// Build the call graph over every function and class, and identify the nodes
    /// that participate in a cycle.
    ///
    /// Nodes are functions and classes; edges are runnable calls out of a function
    /// scope plus constructor dispatch, mirroring `check_constructor_call`.
    pub(super) fn build_call_graph(&self) -> CallGraph {
        let class_names: Vec<ModuleName> = self.classes.par_keys().copied().collect();
        let capacity = self.functions.len() + class_names.len();
        let mut indexes: AHashMap<ModuleName, u32> = AHashMap::with_capacity(capacity);
        let mut names: Vec<ModuleName> = Vec::with_capacity(capacity);
        for name in self.functions.keys().chain(class_names.iter()) {
            indexes.entry(*name).or_insert_with(|| {
                names.push(*name);
                (names.len() - 1) as u32
            });
        }
        // A name can be both a function and a class, so the node count is the
        // deduplicated total rather than the sum.
        let n_nodes = names.len();

        // Collect call-graph edges in parallel. Edge order does not affect the SCC
        // result, so the nondeterministic cross-thread merge order is fine.
        let indexes = &indexes;
        // Runnable-call edges out of each function scope (function/method/
        // decorator/constructor calls).
        let mut edges: Vec<(u32, u32)> = self
            .functions
            .par_iter()
            .flat_map_iter(|(func, _)| {
                let from = indexes[func];
                self.effect_table
                    .get(func)
                    .into_iter()
                    .flatten()
                    .filter(|e| e.kind.is_runnable())
                    .flat_map(move |e| {
                        // `check_call_safety` resolves the callee through the MRO
                        // or a re-export before reading its verdict, so the edge has
                        // to name the same target. `Sub.static_method` is not itself
                        // a node: without this the caller can level before
                        // `Base.static_method` and race the verdict it reads.
                        //
                        // Only consulted when the direct lookup missed, which is
                        // the only case resolution can help.
                        let target = self.call_graph_target(e, |name| indexes.contains_key(name));
                        let callee = target
                            .and_then(|target| indexes.get(&target))
                            .map(|&to| (from, to));
                        // A parameterized decorator call also runs the factory's
                        // immediate nested functions, and
                        // `check_decorator_nested_functions` reads their verdicts
                        // at this call site. Without these edges a nested function
                        // can share a level with the caller that reads it, which
                        // is the partial-verdict race the ordering exists to
                        // prevent. The edges are from the call site rather than
                        // from the factory, so they describe reads that actually
                        // happen and cannot invent a cycle through a factory whose
                        // child never runs.
                        let nested = is_parameterized_decorator_effect(e)
                            .then(|| target.and_then(|target| self.nested_functions.get(&target)))
                            .flatten()
                            .into_iter()
                            .flatten()
                            .filter_map(move |child| indexes.get(child).map(|&to| (from, to)));
                        // A class statement also reads its metaclass methods' verdicts.
                        let metaclass = (e.kind == EffectKind::ClassCreation)
                            .then(|| self.metaclass_creation_callees(&e.name))
                            .into_iter()
                            .flatten()
                            .filter_map(move |method| indexes.get(&method).map(|&to| (from, to)));
                        callee.into_iter().chain(nested).chain(metaclass)
                    })
            })
            .collect();
        // Constructor dispatch edges, mirroring check_constructor_call.
        let ctor_edges: Vec<(u32, u32)> = class_names
            .par_iter()
            .flat_map_iter(|cls_name| {
                let from = indexes[cls_name];
                self.constructor_methods(*cls_name).filter_map(move |m| {
                    // `constructor_methods` names the method on the class
                    // syntactically, but `check_call_body` resolves it through the
                    // MRO, so an inherited `__init__` is read from its ancestor and
                    // needs the edge that names it.
                    indexes
                        .get(&m)
                        .copied()
                        .or_else(|| {
                            self.resolve_callable(&m)
                                .and_then(|target| indexes.get(&target).copied())
                        })
                        .map(|to| (from, to))
                })
            })
            .collect();
        edges.extend(ctor_edges);

        let csr = CsrGraph::from_edges(n_nodes, &edges);
        let in_cycle = csr.nodes_in_cycles();
        debug_assert_eq!(
            names.len(),
            in_cycle.len(),
            "a node id indexes both vectors"
        );
        CallGraph {
            csr,
            names,
            in_cycle,
        }
    }

    /// Mark every function/class in a call cycle `Unsafe`, before any verdict is
    /// computed.
    ///
    ///
    /// This is the order-free equivalent of "recursive calls are unsafe", and it
    /// leaves the rest of the call graph acyclic, which is what makes a dependency
    /// order over the remaining nodes possible.
    pub(super) fn mark_recursive_functions_unsafe(
        &self,
        graph: &CallGraph,
        state: &GlobalAnalysisState,
    ) {
        for (i, is_cyclic) in graph.in_cycle.iter().enumerate() {
            if *is_cyclic {
                state.mark_unsafe(&graph.names[i]);
            }
        }
    }

    /// Seed safelisted definitions so cached verdicts agree with `check_call`.
    pub(super) fn mark_declared_safe(&self, graph: &CallGraph, state: &GlobalAnalysisState) {
        for name in graph
            .names
            .iter()
            .filter(|n| manual_override::declared_safe(n))
        {
            state.mark_safe(name);
        }
    }

    /// Compute every function and class verdict in dependency order.
    ///
    /// A node is visited only once every callee it reaches has a final verdict.
    ///
    /// NOTE: Cycle members are settled before this runs, so they are absent from
    /// the levels and act as finished memo entries for the callers that reach them.
    pub(super) fn precompute_safety_in_dependency_order(
        &self,
        graph: &CallGraph,
        state: &GlobalAnalysisState,
    ) {
        let levels = time("      Leveling the call graph", || {
            graph.csr.dependency_levels(&graph.in_cycle)
        });
        debug!(
            "call graph: {} levels, widest {} nodes, {} settled as cyclic",
            levels.len(),
            levels.iter().map(Vec::len).max().unwrap_or(0),
            graph.in_cycle.iter().filter(|cyclic| **cyclic).count(),
        );

        for level in &levels {
            level.par_iter().for_each(|&node| {
                let name = graph.names[node as usize];
                // Classes first, matching `check_call`'s dispatch: a name in both
                // tables is analyzed as a constructor.
                if self.classes.contains(&name) {
                    self.precompute_constructor_verdict(&name, state);
                } else if let Some(module) = self.functions.get(&name) {
                    self.precompute_function_verdict(&name, module, state);
                }
            });
        }
    }

    fn precompute_constructor_verdict(&self, cls_name: &ModuleName, state: &GlobalAnalysisState) {
        if state.function_safety.contains_key(cls_name) {
            return;
        }
        let effect = Effect::new(EffectKind::FunctionCall, *cls_name, TextRange::default());
        // cls_name as caller_module makes UnsafeIfImported → Unsafe (conservative for cache).
        let call = Call {
            caller_module: cls_name,
            effect: &effect,
            func: *cls_name,
            stack: CallStack::default(),
            is_module_scope: false,
        };
        match self.check_constructor_call(&call, state) {
            Ok(true) => state.mark_safe(cls_name),
            Ok(false) => {
                // `check_constructor_call` just ran each constructor method through
                // `check_call_body`, so a recorded verdict is that method's final
                // one and a missing entry means it had no body to analyze.
                let mut combined = FunctionSafetyInfo::new(FunctionSafety::Safe);
                for method in self.constructor_methods(*cls_name) {
                    if let Some(info) = state.function_safety.get(&method) {
                        combined.verdict.insert(info.verdict);
                        if info.verdict.has(FunctionSafety::UnsafeMissingDep) {
                            // The class waits on the method, not on the callees
                            // that left it unresolved: deciding from those callees
                            // would let the class clear while the method stayed
                            // unsafe for an unrelated reason.
                            combined.missing_dep_callees.insert(method);
                        }
                    }
                }
                // Only an entirely recoverable fold is worth caching. `Unsafe` never
                // clears, and an empty fold means no method accounted for the
                // failure, which must not be cached as a safe class.
                let recoverable =
                    !combined.verdict.is_safe() && !combined.verdict.has(FunctionSafety::Unsafe);
                if recoverable {
                    state.set_function_safety(cls_name, combined);
                } else {
                    state.mark_unsafe(cls_name);
                }
            }
            Err(_) => state.mark_unsafe(cls_name),
        }
    }

    fn precompute_function_verdict(
        &self,
        func_name: &ModuleName,
        func_module: &ModuleName,
        state: &GlobalAnalysisState,
    ) {
        if state.function_safety.contains_key(func_name) {
            return;
        }
        let effect = Effect::new(EffectKind::FunctionCall, *func_name, TextRange::default());
        let mut call = Call {
            caller_module: func_module,
            effect: &effect,
            func: *func_name,
            stack: CallStack::default(),
            is_module_scope: false,
        };
        if let Err(e) = self.check_call_body(&mut call, state) {
            tracing::warn!("precompute_function_verdict: {}: {}", func_name.as_str(), e);
            state.mark_unsafe(func_name);
        }
        if !state.function_safety.contains_key(func_name) {
            state.mark_safe(func_name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AnalysisConfig;
    use crate::imports::ImportGraph;
    use crate::project::analyze_all;

    /// Assert that `callee` is placed in an earlier call-graph level than `caller`,
    /// which is what lets the caller read a finished verdict for it.
    fn assert_leveled_before(module: &str, callee: &str, caller: &str) {
        let sources = crate::test_lib::TestSources::new(&[("m", module)]);
        let config = AnalysisConfig::default();
        let (import_graph, exports, in_scope) = ImportGraph::make_with_exports(&sources, &config);
        let (analysis_map, _) = analyze_all(&sources, &exports, &import_graph, &config, &in_scope);
        let project = ProjectInfo::new(analysis_map, &exports);

        let graph = project.build_call_graph();
        let levels = graph.csr.dependency_levels(&graph.in_cycle);
        let level_of = |name: &str| {
            let module_name = ModuleName::from_str(name);
            levels
                .iter()
                .position(|level| {
                    level
                        .iter()
                        .any(|&node| graph.names[node as usize] == module_name)
                })
                .unwrap_or_else(|| panic!("{name} should be leveled, names: {:?}", graph.names))
        };

        let callee_level = level_of(callee);
        let caller_level = level_of(caller);
        assert!(
            callee_level < caller_level,
            "{callee} must be leveled before {caller}, got {callee_level} and {caller_level}",
        );
    }

    /// A parameterized decorator applied inside a function: the caller reads the
    /// verdicts of the factory's immediate nested functions, so those have to be
    /// leveled before it.
    ///
    /// `wrapper` is given a deeper call chain than `deco` itself, which is what
    /// makes the omission visible: leveled only through the `caller -> deco` edge,
    /// `caller` lands at level 1 while `wrapper` sits at level 2, so the caller
    /// would run first and compute a verdict its reader is racing.
    #[test]
    fn decorator_nested_functions_are_leveled_before_their_reader() {
        let module = r#"
            def deepest():
                pass

            def deep():
                deepest()

            def deco(arg):
                def wrapper(fn):
                    deep()
                    return fn
                return wrapper

            def caller():
                @deco(1)
                def inner():
                    pass
        "#;

        assert_leveled_before(module, "m.deco.wrapper", "m.caller");
    }

    /// A call that resolves through the MRO reads the base class's verdict, so
    /// the base method has to be leveled before the caller. The edge is easy to
    /// lose: the effect names `Sub.static_method`, which is not a function that
    /// exists, and only `resolve_callable` turns it into `Base.static_method`.
    ///
    /// `Base.static_method` is given a deeper call chain than the caller, so if
    /// the edge is missing the caller levels first and races the verdict it reads.
    #[test]
    fn inherited_call_targets_are_leveled_before_their_caller() {
        let module = r#"
            def deepest():
                pass

            def deep():
                deepest()

            class Base:
                @staticmethod
                def static_method():
                    deep()

            class Sub(Base):
                pass

            def caller():
                Sub.static_method()
        "#;

        assert_leveled_before(module, "m.Base.static_method", "m.caller");
    }

    /// `check_constructor_call` resolves each constructor method through the MRO,
    /// so instantiating `Sub` reads `Base.__init__`'s verdict. Without an edge to
    /// the ancestor the two can share a level, which is the partial-verdict race
    /// the inherited-call edges exist to prevent.
    #[test]
    fn inherited_constructor_methods_are_leveled_before_their_class() {
        let module = r#"
            def deepest():
                pass

            def deep():
                deepest()

            class Base:
                def __init__(self):
                    deep()

            class Sub(Base):
                pass

            def caller():
                Sub()
        "#;

        assert_leveled_before(module, "m.Base.__init__", "m.Sub");
    }

    /// A class statement runs its ancestor's `__init_subclass__`, never its own,
    /// so creating `Sub` reads `Base.__init_subclass__`'s verdict. `Sub` defining
    /// its own makes the recorded name a node, which the edge has to look past.
    #[test]
    fn init_subclass_ancestors_are_leveled_before_class_creation() {
        let module = r#"
            def deepest():
                pass

            def deep():
                deepest()

            class Base:
                def __init_subclass__(cls):
                    deep()

            def caller():
                class Sub(Base):
                    def __init_subclass__(cls):
                        pass
        "#;

        assert_leveled_before(module, "m.Base.__init_subclass__", "m.caller");
    }
}
