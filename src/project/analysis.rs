/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Parallel per-module analysis and the project analysis pipeline.

use std::sync::OnceLock;
use std::sync::mpsc;
use std::sync::mpsc::Sender;

use pyrefly_python::module_name::ModuleName;
use rayon::prelude::*;
use tracing::warn;

use crate::analyzer;
use crate::analyzer::AnalyzedModule;
use crate::config::AnalysisConfig;
use crate::exports::Exports;
use crate::hasher::AHashSet;
use crate::imports::ImportGraph;
use crate::module_parser::ParsedModule;
use crate::module_safety::SafetyResult;
use crate::names::enclosing_module_str;
use crate::project::implicit_imports::compute_implicit_imports;
use crate::project::implicit_imports::compute_side_effect_imports;
use crate::project::info::ProjectInfo;
use crate::project::info::resolve_enclosing_module;
use crate::project::state::GlobalAnalysisState;
use crate::project::types::AnalysisMap;
use crate::project::types::AnalysisOutput;
use crate::project::types::ExecutionMode;
use crate::project::types::ParseErrors;
use crate::project::types::SafetyMap;
use crate::source_map::AstResult;
use crate::source_map::ModuleProvider;
use crate::stubs::Stubs;
use crate::tracing::time;

/// Shared immutable context for per-module analysis.
struct AnalysisContext<'a> {
    exports: &'a Exports,
    import_graph: &'a ImportGraph,
    stubs: &'a Stubs,
    config: &'a AnalysisConfig,
}

/// Run the full analysis pipeline.
pub fn run_analysis(
    sources: &impl ModuleProvider,
    exports: &Exports,
    import_graph: &ImportGraph,
    config: &AnalysisConfig,
    mode: ExecutionMode,
    in_scope: &AHashSet<ModuleName>,
) -> AnalysisOutput {
    let (analysis_map, parse_errors) =
        analyze_all(sources, exports, import_graph, config, in_scope);
    let side_effect_imports = time("  Computing side-effect imports", || {
        compute_side_effect_imports(&analysis_map)
    });
    let info = time("  Building project info", || {
        ProjectInfo::new(analysis_map, exports)
    });
    let safety_map = time("  Collecting errors", || {
        info.collect_errors_from_project(mode, import_graph)
    });
    time("  Filtering out stubs", || {
        filter_out_stubs(&safety_map, sources)
    });

    // Filter the class facts to classes this pass owns.
    let owned = |class: &ModuleName| {
        resolve_enclosing_module(class, |module| safety_map.contains_key(module)).is_some()
    };
    let (class_bases, class_properties) = time("  Extracting class facts", || {
        rayon::join(
            || {
                info.classes
                    .base_edges()
                    .into_par_iter()
                    .filter(|(class, _)| owned(class))
                    .collect()
            },
            || {
                info.classes
                    .property_edges()
                    .into_par_iter()
                    .filter(|(class, _)| owned(class))
                    .collect()
            },
        )
    });
    let constructor_callees = time("  Resolving constructor callees", || {
        let mut resolved = info.resolved_constructor_callees();
        resolved.retain(|(class, _)| owned(class));
        resolved
    });

    // Deallocating ProjectInfo takes seconds on large projects. Hand it to a
    // dedicated background thread so the dealloc is non-blocking
    drop_in_background(info);

    AnalysisOutput {
        safety_map,
        side_effect_imports,
        parse_errors,
        class_bases,
        constructor_callees,
        class_properties,
    }
}

/// Dedicated thread for deallocating project info
fn drop_in_background<T: Send + 'static>(value: T) {
    static DROPPER: OnceLock<Sender<Box<dyn FnOnce() + Send>>> = OnceLock::new();
    let sender = DROPPER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name("lifeguard-dealloc".to_owned())
            .spawn(move || rx.into_iter().for_each(|dropper| dropper()))
            .expect("failed to spawn background dropper thread");
        tx
    });
    // If the designated thread exits unexpectedly, drop inline rather than leak.
    if let Err(err) = sender.send(Box::new(move || drop(value))) {
        warn!("background deallocation thread unavailable; dropping inline");
        let dropper = err.0;
        dropper();
    }
}

/// Filter out stubs without sources from the safety map
fn filter_out_stubs(safety_map: &SafetyMap, sources: &impl ModuleProvider) {
    for name in sources.module_names_iter() {
        if sources.is_stub(name) && !sources.overrides_source(name) {
            safety_map.remove(name);
        }
    }
}

fn analyze_module(
    mod_name: ModuleName,
    module: &ParsedModule,
    ctx: &AnalysisContext,
) -> (ModuleName, AnalyzedModule) {
    let output = analyzer::analyze(module, ctx.exports, ctx.import_graph, ctx.stubs, ctx.config);
    (mod_name, output)
}

/// Analyze every module in `in_scope` (the modules the import-graph pass parsed)
/// and build an analysis map. Parse errors are collected and returned separately.
pub fn analyze_all(
    sources: &impl ModuleProvider,
    exports: &Exports,
    import_graph: &ImportGraph,
    config: &AnalysisConfig,
    in_scope: &AHashSet<ModuleName>,
) -> (AnalysisMap, ParseErrors) {
    let ctx = AnalysisContext {
        exports,
        import_graph,
        stubs: sources.stubs(),
        config,
    };

    let parse_errors = ParseErrors::new();

    let mut analysis_map: AnalysisMap = time("  Building analysis map", || {
        sources
            .module_names_par_iter()
            .filter(|mod_name| in_scope.contains(mod_name))
            .filter_map(|mod_name| {
                let ast_result = sources.parse(mod_name)?;
                match ast_result {
                    AstResult::ParserError(e) => {
                        parse_errors.insert(*mod_name, e.to_string());
                        None
                    }
                    AstResult::Ok(ref module) => Some(analyze_module(*mod_name, module, &ctx)),
                }
            })
            .collect()
    });

    time("  Getting implicit imports", || {
        compute_implicit_imports(&mut analysis_map, import_graph)
    });
    (analysis_map, parse_errors)
}

impl ProjectInfo {
    /// Decompose FQN-keyed function safety verdicts into per-module local-name
    /// maps and embed them in the corresponding ModuleSafety entries.
    fn finalize_safety_map(&self, state: GlobalAnalysisState, mode: ExecutionMode) -> SafetyMap {
        let (safety_map, function_safety) = state.into_parts();
        if mode == ExecutionMode::Incremental {
            let modules: Vec<ModuleName> = safety_map.iter().map(|entry| *entry.key()).collect();
            let module_strs: AHashSet<&str> = modules.iter().map(|m| m.as_str()).collect();

            // Consumed rather than iterated by reference: every verdict is moved
            // into the module that owns it, so none of them is cloned.
            function_safety.into_par_iter().for_each(|(fqn, mut info)| {
                let Some((module, local_name)) =
                    enclosing_module_str(fqn.as_str(), |p| module_strs.contains(p))
                else {
                    return;
                };
                if let Some(mut safety_entry) = safety_map.get_mut(&module) {
                    if let SafetyResult::Ok(module_safety) = safety_entry.value_mut() {
                        if let Some(mutated) = self.resolve_cached_mutated_params_for(&fqn) {
                            info.mutated_params = mutated;
                        }
                        module_safety
                            .function_safety
                            .insert(local_name.to_string(), info);
                    }
                }
            });
        }
        safety_map
    }

    fn collect_errors_from_project(
        &self,
        mode: ExecutionMode,
        import_graph: &ImportGraph,
    ) -> SafetyMap {
        let state = GlobalAnalysisState::new(&self.analysis_map);

        // Determinism fix: compute all function/constructor safety verdicts up
        // front, BEFORE the module-scope error pass, so that pass only ever reads
        // a complete, order-independent verdict cache.
        let call_graph = time("    Building the call graph", || self.build_call_graph());
        time("    Marking recursive functions", || {
            self.mark_recursive_functions_unsafe(&call_graph, &state)
        });
        self.mark_declared_safe(&call_graph, &state);
        time("    Precompute safety in dependency order", || {
            self.precompute_safety_in_dependency_order(&call_graph, &state)
        });

        // Single-pass resolution: resolve recoverable `UnsafeMissingDep` verdicts
        // in-process (the incremental path does this at reduce time) so the
        // module-scope error pass below reads resolved verdicts directly.
        if mode == ExecutionMode::WholeProgram {
            time("    Resolve mutation candidates", || {
                self.resolve_whole_program(&state, import_graph)
            });
        }

        self.analysis_map.par_iter().for_each(|(mod_name, result)| {
            let defs = &result.definitions;
            for scope in &defs.eager_scopes {
                if let Err(e) = self.collect_errors_from_scope(mod_name, scope, mode, &state) {
                    state.set_analysis_error(mod_name, e);
                    return;
                }
            }
            self.check_load_imports_eagerly(mod_name, result, &state);
            if let Err(e) = self.collect_implicit_imports(mod_name, result, &state) {
                state.set_analysis_error(mod_name, e);
            }
        });

        let safety_map = self.finalize_safety_map(state, mode);

        if mode == ExecutionMode::WholeProgram {
            self.apply_subclass_registrations(&safety_map, import_graph);
        }
        if mode == ExecutionMode::Incremental {
            for (module, candidates) in self.collect_mutation_candidates(import_graph) {
                if let Some(mut entry) = safety_map.get_mut(&module) {
                    if let SafetyResult::Ok(ms) = entry.value_mut() {
                        ms.mutation_candidates = candidates;
                    }
                }
            }
        }
        safety_map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every class fact a pass ships has to name a class this pass owns.
    #[test]
    fn class_facts_are_scoped_to_the_classes_the_pass_owns() {
        // `datetime.timedelta` is a bundled stub class with a constructor, so it
        // reaches the class table without the module ever being owned here.
        let module = r#"
            import datetime

            SPAN = datetime.timedelta(days=1)
        "#;

        let sources = crate::test_lib::TestSources::new(&[("m", module)]);
        let config = AnalysisConfig::default();
        let (import_graph, exports, in_scope) = ImportGraph::make_with_exports(&sources, &config);
        let output = run_analysis(
            &sources,
            &exports,
            &import_graph,
            &config,
            ExecutionMode::Incremental,
            &in_scope,
        );

        let owned = |class: &ModuleName| {
            resolve_enclosing_module(class, |module| output.safety_map.contains_key(module))
                .is_some()
        };
        let assert_owned = |kind: &str, classes: Vec<ModuleName>| {
            let leaked: Vec<String> = classes
                .into_iter()
                .filter(|c| !owned(c))
                .map(|c| c.as_str().to_owned())
                .collect();
            assert!(
                leaked.is_empty(),
                "{kind} names classes this pass does not own: {leaked:?}",
            );
        };

        assert_owned(
            "class_bases",
            output.class_bases.iter().map(|(c, _)| *c).collect(),
        );
        assert_owned(
            "class_properties",
            output.class_properties.iter().map(|(c, _)| *c).collect(),
        );
        assert_owned(
            "constructor_callees",
            output.constructor_callees.iter().map(|(c, _)| *c).collect(),
        );
    }
}
