/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! What an `if __name__ == "__main__":` block contributed to one module.
//!
//! The guard bodies are analysed in the map phase, which cannot know which
//! module a binary starts from; the reduce, which can, drops what they produced
//! everywhere else.
//!
//! Provenance is recorded per fact, as the analyser walks the body, and carries
//! no position. That keeps the artifact independent of where in the file the
//! guard sits, so an edit that moves it without changing a fact leaves the cache
//! bytes alone and the reduce keeps its Buck cache hit.

use pyrefly_python::module_name::ModuleName;
use serde::Deserialize;
use serde::Serialize;

use crate::cache::artifact::CachedModule;
use crate::cache::artifact::CachedModuleSafety;
use crate::cache::artifact::CachedSafety;
use crate::hasher::AHashSet;

/// One module's `__main__` guard provenance: which import edges only a guard
/// body writes.
///
/// Per-fact provenance lives on the facts themselves; only edges need a record
/// here, because an edge is a set member rather than an occurrence and so has
/// nowhere to carry a flag.
#[derive(Default, Serialize, Deserialize)]
pub struct MainGuardFacts {
    /// Edges no module-scope statement outside a guard writes.
    imports: AHashSet<ModuleName>,
}

impl MainGuardFacts {
    pub(crate) fn new(imports: AHashSet<ModuleName>) -> Self {
        Self { imports }
    }

    pub(crate) fn imports(&self) -> &AHashSet<ModuleName> {
        &self.imports
    }

    /// Combine two records of the same module.
    ///
    /// Guard-only edges intersect, like missing imports and for the same
    /// reason: one producer seeing an edge unguarded is enough to keep it.
    pub(crate) fn merge(&mut self, other: MainGuardFacts) {
        self.imports.retain(|m| other.imports.contains(m));
    }
}

/// Drop everything `module`'s `__main__` guard produced, returning how much
/// went. The caller decides which modules this applies to; here the guard is
/// assumed not to run.
pub(super) fn drop_main_guarded(module: &mut CachedModule) -> usize {
    // Named exhaustively (no `..`) so that a new fact on `CachedModule` is a
    // compile error here until someone decides whether a guard can produce it.
    let CachedModule {
        name: _,
        safety,
        imports,
        missing_imports,
        ambiguous_imports,
        side_effect_imports,
        function_safety: _,
        mutation_candidates,
        property_candidates,
        main_guard,
    } = module;

    // An edge written only inside the guard is not a dependency of this module:
    // the body never runs here, and a function defined in it is never even
    // defined. No call analysis is needed to say so, which is why edges can go
    // while a deferred import cannot.
    let mut dropped = 0;
    if !main_guard.imports.is_empty() {
        let before = imports.len()
            + missing_imports.len()
            + side_effect_imports.len()
            + ambiguous_imports.len();
        imports.retain(|m| !main_guard.imports.contains(m));
        missing_imports.retain(|m| !main_guard.imports.contains(m));
        // A guard-body import is not a side-effect import either: it is unused
        // *and* unexecuted, so nothing has to be loaded eagerly on its behalf in
        // the modules that import this one.
        side_effect_imports.retain(|m| !main_guard.imports.contains(m));
        // An ambiguous candidate is an edge the reduce may still resolve into a
        // real dependency, so a guard-only one has to go with the rest: for a
        // module whose guard does not run, that dependency does not exist.
        ambiguous_imports.retain(|m| !main_guard.imports.contains(m));
        dropped += before
            - imports.len()
            - missing_imports.len()
            - side_effect_imports.len()
            - ambiguous_imports.len();
    }

    // Candidates hang off the module, not off its safety record, so they are
    // filtered whether or not the module's own analysis succeeded.
    dropped += retain_unguarded(mutation_candidates, |c| c.from_main_guard);
    dropped += retain_unguarded(property_candidates, |c| c.from_main_guard);

    let CachedSafety::Ok(safety) = safety else {
        return dropped;
    };
    let CachedModuleSafety {
        errors,
        force_imports_eager_overrides,
        // No provenance of its own: implicit imports are derived after
        // traversal, from pending and called imports, so a guarded one is not
        // distinguishable here while the whole-program path prunes it outright.
        implicit_imports: _,
    } = safety;
    dropped += retain_unguarded(errors, |e| e.from_main_guard);
    dropped += retain_unguarded(force_imports_eager_overrides, |e| e.from_main_guard);
    dropped
}

/// Keep only the items a guard did not produce, reporting how many went.
fn retain_unguarded<T>(items: &mut Vec<T>, guarded: impl Fn(&T) -> bool) -> usize {
    let before = items.len();
    items.retain(|item| !guarded(item));
    before - items.len()
}

#[cfg(test)]
mod tests {
    use ruff_text_size::TextRange;

    use super::*;
    use crate::errors::ErrorKind;
    use crate::errors::SafetyError;
    use crate::module_safety::MutationCandidate;
    use crate::module_safety::MutationCandidateSite;

    fn names(names: &[&str]) -> AHashSet<ModuleName> {
        names.iter().map(|n| ModuleName::from_str(n)).collect()
    }

    #[test]
    fn a_guard_only_edge_survives_the_merge_only_where_every_record_agrees() {
        let mut facts = MainGuardFacts::new(names(&["a", "b"]));
        facts.merge(MainGuardFacts::new(names(&["b", "c"])));
        assert_eq!(
            facts.imports(),
            &names(&["b"]),
            "one producer writing `a` outside a guard is enough to keep the edge",
        );
    }

    /// An ambiguous `from pkg import child` written only inside a guard is still
    /// a guard-produced edge: the reduce may resolve it into a real dependency,
    /// and for a non-entry module that dependency does not exist.
    #[test]
    fn dropping_takes_guard_only_ambiguous_edges() {
        let mut module = CachedModule::empty(ModuleName::from_str("pkg.mod"));
        module.ambiguous_imports = names(&["pkg.guarded_child", "pkg.plain_child"]);
        module.main_guard = MainGuardFacts::new(names(&["pkg.guarded_child"]));

        assert_eq!(
            drop_main_guarded(&mut module),
            1,
            "the guard-only candidate"
        );
        assert_eq!(
            module.ambiguous_imports,
            names(&["pkg.plain_child"]),
            "a candidate also written outside the guard stays resolvable",
        );
    }

    #[test]
    fn dropping_takes_the_facts_a_guard_produced_and_leaves_the_rest() {
        let mut module = CachedModule::empty(ModuleName::from_str("pkg.mod"));
        module.imports = names(&["guarded", "plain"]);
        module.main_guard = MainGuardFacts::new(names(&["guarded"]));
        let error = |guarded: bool| {
            let e = SafetyError::new(
                ErrorKind::UnsafeFunctionCall,
                "pkg.dep.call()".to_owned(),
                TextRange::default(),
            );
            if guarded { e.from_main_guard() } else { e }
        };
        module.safety = CachedSafety::Ok(CachedModuleSafety {
            errors: vec![error(false), error(true)],
            ..CachedModuleSafety::default()
        });

        assert_eq!(drop_main_guarded(&mut module), 2, "one edge and one error");
        assert_eq!(module.imports, names(&["plain"]));
        let CachedSafety::Ok(safety) = &module.safety else {
            panic!("the record was built Ok and dropping does not change that");
        };
        assert_eq!(
            safety.errors.len(),
            1,
            "only the error the guard produced goes",
        );
        assert!(
            !safety.errors[0].from_main_guard,
            "the survivor is the unguarded one",
        );
    }

    /// Two errors alike in every cached field but provenance. Telling them apart
    /// is what lets a cache carry no positions at all.
    #[test]
    fn identical_errors_are_separated_by_provenance_alone() {
        let mut module = CachedModule::empty(ModuleName::from_str("pkg.mod"));
        let error = SafetyError::new(
            ErrorKind::UnsafeFunctionCall,
            "pkg.dep.call()".to_owned(),
            TextRange::default(),
        );
        module.safety = CachedSafety::Ok(CachedModuleSafety {
            errors: vec![error, error.from_main_guard()],
            ..CachedModuleSafety::default()
        });

        assert_eq!(drop_main_guarded(&mut module), 1);
        let CachedSafety::Ok(safety) = &module.safety else {
            panic!("built Ok");
        };
        assert_eq!(safety.errors.len(), 1, "only the guarded one goes");
    }

    /// A module whose analysis failed still has candidates, and a guard body
    /// that does not run produced them just the same.
    #[test]
    fn candidates_are_dropped_even_when_the_modules_analysis_failed() {
        let mut module = CachedModule::empty(ModuleName::from_str("pkg.mod"));
        module.safety = CachedSafety::AnalysisError {
            message: "boom".to_owned(),
        };
        let candidate = |guarded: bool| MutationCandidate {
            callee: ModuleName::from_str("dep.sink"),
            site: MutationCandidateSite::ModuleScope {
                call: ModuleName::from_str("dep.sink"),
            },
            arg_offset: 0,
            imported_args: Default::default(),
            range: TextRange::default(),
            from_main_guard: guarded,
        };
        module.mutation_candidates = vec![candidate(false), candidate(true)];

        assert_eq!(drop_main_guarded(&mut module), 1);
        assert_eq!(module.mutation_candidates.len(), 1);
    }
}
