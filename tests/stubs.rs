/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::*;

    #[test]
    fn test_unknown_effects() {
        let code = r#"
import lifeguard_test

lifeguard_test.foo() # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_no_effects() {
        let code = r#"
import lifeguard_test

lifeguard_test.bar() # no error
"#;
        check(code);
    }

    #[test]
    fn test_module_level_effects() {
        // TODO(T248043795): Module import should be unsafe
        let code = r#"
import lifeguard_test2 # TODO: unsafe

lifeguard_test2.foo() # E: imported-function-call
"#;
        check_effects(code);
    }

    #[test]
    fn test_module_level_error_produced() {
        // TODO(T248043795): Module import should be unsafe
        let code = r#"
import lifeguard_test2 # TODO: unsafe

lifeguard_test2.foo()
"#;
        check(code);
    }

    #[test]
    fn test_collections_namedtuple() {
        let code = r#"
from collections import namedtuple
Point = namedtuple('Point', ['x', 'y'])
"#;
        check(code);
    }

    #[test]
    fn test_collections_abc_iterable() {
        let code = r#"
from collections.abc import Iterable
x = []
y = isinstance(x, Iterable)

class Boo(Iterable):
    pass
z = Boo()
"#;
        check(code);
    }

    #[test]
    fn test_collections_abc_mapping_register() {
        let code = r#"
from collections.abc import Mapping

class MyContainer:
    pass

Mapping.register(MyContainer)
"#;
        check(code);
    }

    #[test]
    fn test_collections_abc_sequence_register() {
        let code = r#"
from collections.abc import Sequence

class MyContainer:
    pass

Sequence.register(MyContainer)
"#;
        check(code);
    }

    #[test]
    fn test_collections_abc_set_register() {
        let code = r#"
from collections.abc import Set

class MyContainer:
    pass

Set.register(MyContainer)
"#;
        check(code);
    }

    #[test]
    fn test_source_overriding_stub_retained_in_safety_map() {
        use lifeguard::config::AnalysisConfig;
        use lifeguard::imports::ImportGraph;
        use lifeguard::project;
        use lifeguard::pyrefly::module_name::ModuleName;
        use lifeguard::test_lib::TestSources;

        let modules = vec![
            ("a", "from b import foo\nfoo()"),
            ("b", "def foo(): no_effects()"),
        ];
        let sources = TestSources::new_with_stubs(&modules, &["b"]);
        let config = AnalysisConfig::default();
        let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &config);
        let result = project::run_analysis(
            &sources,
            &exports,
            &import_graph,
            &config,
            project::CachingMode::Disabled,
        );
        assert!(
            result.safety_map.get(&ModuleName::from_str("b")).is_some(),
            "source-overriding stub should remain in the safety map"
        );
    }

    #[test]
    fn test_libfb_lazy_classproperty_safe() {
        let code = r#"
from libfb.py.decorators import lazy_classproperty, thread_safe_lazy_classproperty

class C:
    @lazy_classproperty
    def foo(cls):
        return 1

    @thread_safe_lazy_classproperty
    def bar(cls):
        return 2
"#;
        check(code);
    }

    #[test]
    fn test_networkx_not_implemented_for_safe() {
        let code = r#"
from networkx.utils.decorators import not_implemented_for

@not_implemented_for("directed")
def f(g):
    return g
"#;
        check(code);
    }

    #[test]
    fn test_transformers_deprecate_kwarg_safe() {
        let code = r#"
from transformers.utils.deprecation import deprecate_kwarg

@deprecate_kwarg("old", version="5.0.0", new_name="new")
def f(new=1):
    return new
"#;
        check(code);
    }

    #[test]
    fn test_pydantic_stub_class_body_calls_safe() {
        // Regression test for the bundled pydantic/__init__.pyi stub.
        // Pydantic's runtime __init__.py exposes Field/ConfigDict/field_serializer
        // through PEP 562 __getattr__ which lifeguard cannot follow; without the
        // stub these calls inside class bodies are flagged unknown-*-call.
        let code = r#"
from pydantic import BaseModel, ConfigDict, Field, field_serializer

class M(BaseModel):
    model_config = ConfigDict(extra="ignore")
    x: int = Field(default=0)

    @field_serializer("x")
    def _ser(self, v):
        return v
"#;
        check(code);
    }
}
