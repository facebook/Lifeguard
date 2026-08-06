/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::imports::ImportGraph;
    use lifeguard::pyrefly::module_name::ModuleName;
    use lifeguard::test_lib::build_import_graph;

    /// Helper to get all imports for a module from the import graph
    fn get_imports(graph: &ImportGraph, module: &str) -> Vec<String> {
        let name = ModuleName::from_str(module);
        graph
            .get_imports(&name)
            .map(|m| m.as_str().to_string())
            .collect()
    }

    #[test]
    fn test_tracks_module_level_imports() {
        // Module-level imports should be tracked
        let main = r#"
import foo
from bar import baz
"#;
        let foo = r#""#;
        let bar = r#"
baz = 1
"#;
        let modules = vec![("__main__", main), ("foo", foo), ("bar", bar)];

        let import_graph = build_import_graph(&modules);

        let imports = get_imports(&import_graph, "__main__");
        assert!(
            imports.contains(&"foo".to_string()),
            "Expected 'foo' in imports: {:?}",
            imports
        );
        assert!(
            imports.contains(&"bar".to_string()),
            "Expected 'bar' in imports: {:?}",
            imports
        );
    }

    #[test]
    fn test_tracks_function_level_imports() {
        // Function-level imports should be tracked
        let main = r#"
def my_function():
    import nested_module
    from nested_package import nested_name
"#;
        let nested_module = r#""#;
        let nested_package = r#"
nested_name = 1
"#;
        let modules = vec![
            ("__main__", main),
            ("nested_module", nested_module),
            ("nested_package", nested_package),
        ];

        let import_graph = build_import_graph(&modules);

        let imports = get_imports(&import_graph, "__main__");

        // Function-level imports are now tracked
        assert!(
            imports.contains(&"nested_module".to_string()),
            "Expected 'nested_module' in imports: {:?}",
            imports
        );
        assert!(
            imports.contains(&"nested_package".to_string()),
            "Expected 'nested_package' in imports: {:?}",
            imports
        );
    }

    #[test]
    fn test_tracks_class_level_imports() {
        // Class-level imports should be tracked
        let main = r#"
class MyClass:
    import class_level_module

    def method(self):
        from method_package import method_name
"#;
        let class_level_module = r#""#;
        let method_package = r#"
method_name = 1
"#;
        let modules = vec![
            ("__main__", main),
            ("class_level_module", class_level_module),
            ("method_package", method_package),
        ];

        let import_graph = build_import_graph(&modules);

        let imports = get_imports(&import_graph, "__main__");

        // Class-level and method-level imports are now tracked
        assert!(
            imports.contains(&"class_level_module".to_string()),
            "Expected 'class_level_module' in imports: {:?}",
            imports
        );
        assert!(
            imports.contains(&"method_package".to_string()),
            "Expected 'method_package' in imports: {:?}",
            imports
        );
    }

    #[test]
    fn test_tracks_decorator_nested_imports() {
        // This test simulates the numba issue: a decorator calls a function
        // that has a nested import. These imports should now be tracked.
        let main = r#"
from decorators import my_decorator

@my_decorator
def decorated_function():
    pass
"#;
        let decorators = r#"
def my_decorator(func):
    # This function-level import is now tracked
    from hidden_dependency import secret
    return func
"#;
        let hidden_dependency = r#"
secret = "value"
"#;
        let modules = vec![
            ("__main__", main),
            ("decorators", decorators),
            ("hidden_dependency", hidden_dependency),
        ];

        let import_graph = build_import_graph(&modules);

        // Check that __main__ imports decorators (module-level, tracked)
        let main_imports = get_imports(&import_graph, "__main__");
        assert!(
            main_imports.contains(&"decorators".to_string()),
            "Expected 'decorators' in __main__ imports: {:?}",
            main_imports
        );

        // Check that decorators now imports hidden_dependency (function-level, now tracked)
        let decorator_imports = get_imports(&import_graph, "decorators");
        assert!(
            decorator_imports.contains(&"hidden_dependency".to_string()),
            "Expected 'hidden_dependency' in decorators imports: {:?}",
            decorator_imports
        );
    }
}
