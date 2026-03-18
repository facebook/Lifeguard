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
    fn test_unsafe_init_called() {
        let code = r#"
 import foo

class A:
    def __init__(self):
        foo.x = 42

a = A()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_init_not_called() {
        let code = r#"
 import foo

class A:
    def __init__(self):
        foo.x = 42
"#;
        check(code);
    }

    #[test]
    fn test_safe_init_unsafe_body() {
        let code = r#"
 import foo

class A:
    foo.x = 42  # E: imported-module-assignment

    def __init__(self):
        pass

a = A() # no error
"#;
        check(code);
    }

    #[test]
    fn test_metaclass_called() {
        let code = r#"
REGISTRY = {}

class Meta:
    def __new__(cls, name, bases, attrs):
        REGISTRY[name] = cls

class A(metaclass=Meta):
    pass

a = A()
"#;
        check(code);
    }

    #[test]
    fn test_metaclass_not_called() {
        let code = r#"
REGISTRY = {}

class Meta:
    def __new__(cls, name, bases, attrs):
        REGISTRY[name] = cls

class A(metaclass=Meta):
    pass
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_post_init_called() {
        // __post_init__ is called by dataclass-generated __init__
        let code = r#"
 import foo

class A:
    def __post_init__(self):
        foo.x = 42

a = A()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_post_init_not_called() {
        let code = r#"
 import foo

class A:
    def __post_init__(self):
        foo.x = 42
"#;
        check(code);
    }

    #[test]
    fn test_metaclass_init() {
        // Mark a class unsafe if its metaclass.__init__ has unsafe behaviour
        let code = r#"
from foo import REGISTRY

class Meta:
    def __init__(cls, name, bases, attrs):
        REGISTRY[name] = cls

class A(metaclass=Meta):
    pass

a = A()  # E: unsafe-function-call
"#;
        check(code);
    }
}
