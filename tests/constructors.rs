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

    /// A class's own `__new__` runs on instantiation just as its `__init__`
    /// does. The cache-backed path covers this in `tests/cache.rs`; this pins
    /// the direct path, whose candidate set used to omit the own `__new__`.
    #[test]
    fn test_unsafe_new_called() {
        let code = r#"
import foo

class A:
    def __new__(cls):
        foo.x = 42
        return super().__new__(cls)

a = A()  # E: unsafe-function-call
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_new_not_called() {
        let code = r#"
import foo

class A:
    def __new__(cls):
        foo.x = 42
        return super().__new__(cls)
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

class A(metaclass=Meta):  # E: unsafe-function-call
    pass

a = A()  # E: unsafe-function-call
"#;
        check(code);
    }

    /// The metaclass also runs for a class that only inherits it from a base.
    #[test]
    fn test_inherited_metaclass_runs_at_class_creation() {
        let code = r#"
from foo import REGISTRY

class Meta(type):
    def __new__(mcs, name, bases, attrs):
        cls = super().__new__(mcs, name, bases, attrs)
        REGISTRY[name] = cls
        return cls

class Base(metaclass=Meta):  # E: unsafe-function-call
    pass

class Robot(Base):  # E: unsafe-function-call
    pass
"#;
        check(code);
    }

    /// `type.__new__(mcs, ...)` runs nothing more than `super().__new__(...)` does.
    #[test]
    fn test_metaclass_calling_type_new_directly_stays_safe() {
        let code = r#"
class Meta(type):
    def __new__(mcs, name, bases, attrs):
        return type.__new__(mcs, name, bases, attrs)

    def __init__(cls, name, bases, attrs):
        type.__init__(cls, name, bases, attrs)

class Base(metaclass=Meta):
    pass
"#;
        check(code);
    }

    #[test]
    fn test_metaclass_writing_its_own_module_stays_safe() {
        let code = r#"
REGISTRY = {}

class Meta(type):
    def __new__(mcs, name, bases, attrs):
        cls = super().__new__(mcs, name, bases, attrs)
        REGISTRY[name] = cls
        return cls

class Base(metaclass=Meta):
    pass

class Robot(Base):
    pass
"#;
        check(code);
    }

    /// `Base.__init_subclass__` runs at the `class A(Base)` statement itself,
    /// before anything instantiates `A`. This is the registry pattern: defining
    /// the subclass is what registers it.
    #[test]
    fn test_init_subclass_runs_at_class_creation() {
        let code = r#"
import foo

class Base:
    def __init_subclass__(cls):
        foo.x = 42

class A(Base):  # E: unsafe-function-call
    pass
"#;
        check(code);
    }

    /// The registry pattern as it appears in practice: the base is imported,
    /// and defining the subclass is the whole point of the import.
    #[test]
    fn test_init_subclass_from_an_imported_base() {
        let base = r#"
import foo

class Base:
    def __init_subclass__(cls):
        foo.registry = cls
"#;
        let leaf = r#"
from base import Base

class Leaf(Base):  # E: unsafe-function-call
    pass
"#;
        check_all(vec![("base", base), ("leaf", leaf)]);
    }

    /// A class's own `__init_subclass__` runs for its subclasses, never at its
    /// own class statement, because `type.__new__` calls
    /// `super().__init_subclass__`.
    #[test]
    fn test_own_init_subclass_does_not_run_for_itself() {
        let code = r#"
import foo

class Base:
    pass

class A(Base):
    def __init_subclass__(cls):
        foo.x = 42

class B(A):  # E: unsafe-function-call
    pass
"#;
        check(code);
    }

    #[test]
    fn test_safe_init_subclass_is_not_an_error() {
        let code = r#"
class Base:
    def __init_subclass__(cls):
        pass

class A(Base):
    pass
"#;
        check(code);
    }
}
