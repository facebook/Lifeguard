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
    fn test_safe_property() {
        let code = r#"
class A:
    @property
    def f(self):
        return 42

a = A()
a.f
"#;
        check(code);
    }

    #[test]
    fn test_unsafe_property() {
        let code = r#"
class A:
    @property
    def f(self):
        raise()

a = A()
a.f  # E: unsafe-method-call
"#;
        check(code);
    }

    #[test]
    fn test_safe_imported_property() {
        let code1 = r#"
class A:
    @property
    def f(self):
        return 42
"#;

        let code2 = r#"
from m1 import A
a = A()
a.f
"#;
        check_all(vec![("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_unsafe_imported_property() {
        let code1 = r#"
class A:
    @property
    def f(self):
        raise()

"#;

        let code2 = r#"
from m1 import A
a = A()
a.f  # E: unsafe-method-call
"#;
        check_all(vec![("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_indirect_import() {
        let code0 = r#"
class A:
    def f(self):
        return "hi"

class B:
    def f(self):
        return "bye"
"#;
        let code1 = r#"
from m0 import A
from m0 import B as C
"#;

        let code2 = r#"
from m1 import A, C
a = A()
c = C()
"#;
        check_all(vec![("m0", code0), ("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_safe_indirect_import_reassignment() {
        let code0 = r#"
class A:
    def g(self):
        return "hi"

class B:
    def f(self):
        return "bye"
"#;
        let code1 = r#"
from m0 import A

from m0 import B as A 
"#;

        let code2 = r#"
from m1 import A
a = A()
a.f()
"#;
        check_all(vec![("m0", code0), ("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_safe_indirect_import_class_reassignment() {
        let code0 = r#"
class B:
    def f(self):
        return "bye"
"#;
        let code1 = r#"
from m0 import B

class B:
    def g(self):
        pass
"#;
        let code2 = r#"
from m1 import B
b = B()
b.g()
"#;
        check_all(vec![("m0", code0), ("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_safe_indirect_import_function_reassignment() {
        let code0 = r#"
class B:
    def f(self):
        return "bye"
"#;
        let code1 = r#"
from m0 import B

def B(arg):
    print(arg)

"#;
        let code2 = r#"
from m1 import B
B("hi")
"#;
        check_all(vec![("m0", code0), ("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_sys_modules_getattr() {
        let code = r#"
import sys
a = getattr(sys, "modules")
setattr(sys, "modules", {})# E: imported-module-assignment
delattr(sys, "modules")  # TODO: prohibited-call
"#;
        check(code);
    }

    #[test]
    fn test_indirect_import_method_call() {
        let code0 = r#"
class A:
    def f(self):
        return "hi"

"#;
        let code1 = r#"
from m0 import A as C
"#;

        let code2 = r#"
from m1 import C
c = C()
c.f()
c.g() # E: unknown-method-call
"#;

        check_all(vec![("m0", code0), ("m1", code1), ("m2", code2)]);
    }

    #[test]
    fn test_unresolvable_base() {
        let code = r#"
foo.bar # E: unknown-object
"#;
        check(code);
    }

    #[test]
    fn test_comprehension_targets_resolve_for_attr_access() {
        let code = r#"
class Message:
    value = "message"

messages = [Message()]

list_values = [message.value for message in messages]
set_values = {message.value for message in messages}
dict_values = {message.value: message for message in messages}
generator_values = (message.value for message in messages)
message.UH_OH = 5
message.UH_OH # E: unknown-object
"#;
        check(code);
    }
}
