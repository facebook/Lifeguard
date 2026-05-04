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
        use lifeguard::imports::ImportGraph;
        use lifeguard::project;
        use lifeguard::pyrefly::module_name::ModuleName;
        use lifeguard::pyrefly::sys_info::SysInfo;
        use lifeguard::test_lib::TestSources;
        use lifeguard::traits::SysInfoExt;

        let modules = vec![
            ("a", "from b import foo\nfoo()"),
            ("b", "def foo(): no_effects()"),
        ];
        let sources = TestSources::new_with_stubs(&modules, &["b"]);
        let sys_info = SysInfo::lg_default();
        let (import_graph, exports) = ImportGraph::make_with_exports(&sources, &sys_info);
        let result = project::run_analysis(
            &sources,
            &exports,
            &import_graph,
            &sys_info,
            project::CachingMode::Disabled,
        );
        assert!(
            result.safety_map.get(&ModuleName::from_str("b")).is_some(),
            "source-overriding stub should remain in the safety map"
        );
    }
}
