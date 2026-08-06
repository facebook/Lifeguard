/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[cfg(test)]
mod tests {
    use lifeguard::test_lib::*;
    // Port over tests from safer_lazy_imports/analyzer/tests/test_ownership.py
    // Kept as a conformance corpus against the original analyzer, so overlap
    // with the topical test files is deliberate.

    #[test]
    fn test_list_modify() {
        let code1 = r#"
            l1 = [1, 2, 3]
        "#;
        let code2 = r#"
            from m1 import l1
            l1[0] = 2  # E: imported-module-assignment
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_imported_list_append_is_unsafe() {
        // This call mutates the imported value and should therefore be unsafe
        let code1 = r#"
            l1 = [1, 2, 3]
        "#;
        let code2 = r#"
            from m1 import l1
            l1.append(4)  # E: unknown-function-call 
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_dict_modify() {
        let code1 = r#"
            d1 = {1: 2, 3: 4}
        "#;
        let code2 = r#"
            from m1 import d1
            d1[5] = 6  # E: imported-module-assignment
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_dict_set_default() {
        // This call mutates the imported value and should therefore be unsafe
        let code1 = r#"
            d1 = {1: 2, 3: 4}
        "#;
        let code2 = r#"
            from m1 import d1
            d1.setdefault(5, 6)  # E: unknown-function-call
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_func_modify() {
        let code1 = r#"
            d1 = {1: 2, 3: 4}
        "#;
        let code2 = r#"
            def f(value):
                value[5] = 1
        "#;
        let code3 = r#"
            from m1 import d1
            from m2 import f
            f(d1)  # E: imported-var-argument
        "#;
        check_all(vec![("m1", code1), ("m2", code2), ("m3", code3)])
    }

    #[test]
    fn test_decorator_modify() {
        let code1 = r#"
            state = [0]
            def dec(func):
                state[0] = state[0] + 1
                return func
        "#;
        let code2 = r#"
            from m1 import dec
            @dec  # E: unsafe-decorator-call
            def g():
                pass
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_decorator_ok() {
        let code1 = r#"
            def dec(cls):
                cls.x = 1
                return cls
        "#;
        let code2 = r#"
            from m1 import dec
            @dec
            class C:
                x: int = 0
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_dict_ok() {
        let code1 = r#"
            def f():
                return {1: 2, 3: 4}
        "#;
        let code2 = r#"
            from m1 import f
            x = f()
            x[5] = 6
            x.setdefault(1, 9)  # E: unknown-method-call
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_property_side_effect() {
        let code1 = r#"
            l = []
            class C:
                @property 
                def l(self):
                    l.append(1)
                    return l
        "#;
        let code2 = r#"
            from m1 import C
            c = C()
            c.l # E: unsafe-method-call
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_bound_method_ownership() {
        let code1 = r#"
            class C:
                def f(cls) -> None:
                    pass
        "#;
        let code2 = r#"
            from m1 import C
            c = C()
            x = c.f
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_bound_classmethod_ownership() {
        let code1 = r#"
            class C:
                @classmethod
                def f(cls) -> None:
                    pass
        "#;
        let code2 = r#"
            from m1 import C
            x = C.f
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_func_dunder_dict_modification() {
        let code1 = r#"
            def f():
                pass
        "#;
        let code2 = r#"
            from m1 import f

            f.__dict__["foo"] = 1 # E: imported-module-assignment
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_func_dunder_dict_keys() {
        let code1 = r#"
            def f():
                pass
            f.foo = "bar"
        "#;
        let code2 = r#"
            from m1 import f
            x, = f.__dict__
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_registry_pattern_decorator() {
        let code1 = r#"
            registry = {}
            def addToRegistry(cls):
                registry[cls.__name__] = cls
        "#;
        let code2 = r#"
            import m1

            @m1.addToRegistry # E: unsafe-decorator-call
            class classOne:
                pass
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    fn test_registry_pattern_metaclass() {
        let code1 = r#"
            registry = {}

            class MetaCls(type):
                def __new__(cls, name, bases, attrs):
                    registry[name] = cls
                    return super().__new__(cls, name, bases, attrs)
        "#;
        let code2 = r#"
            import m1

            class SubClassOne(metaclass=m1.MetaCls):
                pass

            class SubClassTwo(metaclass=m1.MetaCls):
                pass

            a = SubClassOne() # E: unsafe-function-call
            b = SubClassTwo() # E: unsafe-function-call
        "#;
        check_all(vec![("m1", code1), ("m2", code2)])
    }

    #[test]
    // Added test, not ported
    fn test_registry_pattern_metaclass_with_imported_global() {
        let code1 = r#"
            from m3 import registry

            class MetaCls(type):
                def __new__(cls, name, bases, attrs):
                    registry[name] = cls
                    return super().__new__(cls, name, bases, attrs)
        "#;
        let code2 = r#"
            import m1

            class SubClassOne(metaclass=m1.MetaCls):
                pass

            class SubClassTwo(metaclass=m1.MetaCls):
                pass

            a = SubClassOne() # E: unsafe-function-call
            b = SubClassTwo() # E: unsafe-function-call
        "#;
        let code3 = r#"
            registry = {}
        "#;
        check_all(vec![("m1", code1), ("m2", code2), ("m3", code3)])
    }
}
