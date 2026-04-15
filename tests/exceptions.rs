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
    fn test_unhandled_exception() {
        let code = r#"
raise ValueError("bye!")  # E: unhandled-exception
"#;
        check(code);
    }

    #[test]
    fn test_handled_exception() {
        let code = r#"
try:
    raise ValueError("bye!")
except Exception as e:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_handled_exception_exact_match() {
        let code = r#"
try:
    raise ValueError("bye!")
except ValueError:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_unhandled_exception_wrong_type() {
        let code = r#"
try:
    raise ValueError("bye!")  # E: unhandled-exception
except TypeError:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_handled_exception_tuple() {
        let code = r#"
try:
    raise ValueError("bye!")
except (TypeError, ValueError):
    ...
"#;
        check(code);
    }

    #[test]
    fn test_unhandled_exception_tuple_no_match() {
        let code = r#"
try:
    raise ValueError("bye!")  # E: unhandled-exception
except (TypeError, KeyError):
    ...
"#;
        check(code);
    }

    #[test]
    fn test_handled_exception_catch_all_exception() {
        let code = r#"
try:
    raise ValueError("bye!")
except Exception:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_handled_exception_catch_all_base_exception() {
        let code = r#"
try:
    raise KeyboardInterrupt()
except BaseException:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_handled_exception_nested_try_outer_catches() {
        let code = r#"
try:
    try:
        raise ValueError("bye!")
    except TypeError:
        ...
except ValueError:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_unhandled_exception_nested_try_neither_catches() {
        let code = r#"
try:
    try:
        raise ValueError("bye!")  # E: unhandled-exception
    except TypeError:
        ...
except KeyError:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_bare_raise_in_try() {
        let code = r#"
try:
    raise
except TypeError:
    ...
"#;
        check(code);
    }

    #[test]
    fn test_raise_in_handler() {
        let code = r#"
try:
    raise ValueError("bye!")
except Exception as e:
    raise ValueError("unhandled!")  # E: unhandled-exception
"#;
        check(code);
    }

    #[test]
    fn test_raise_in_else() {
        let code = r#"
try:
    raise ValueError("bye!")
except Exception as e:
    pass
else:
    raise ValueError("unhandled!")  # E: unhandled-exception
"#;
        check(code);
    }

    #[test]
    fn test_raise_in_finally() {
        let code = r#"
try:
    raise ValueError("bye!")
except Exception as e:
    pass
finally:
    raise ValueError("unhandled!")  # E: unhandled-exception
"#;
        check(code);
    }
}
