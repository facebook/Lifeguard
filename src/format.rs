/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Output formatting utilities

use ruff_python_ast::BoolOp;
use ruff_python_ast::CmpOp;
use ruff_python_ast::Expr;
use ruff_python_ast::Operator;
use ruff_python_ast::UnaryOp;
use serde::Serialize;

// Display error enums as strings
pub trait ErrorString {
    fn error_string(&self) -> String;
}

// When serde serializes an enum variant to json, it returns it as a quoted string wrapped in an
// option. This function unwraps the option and strips off the surrounding quotes.
pub fn bare_string(e: &impl Serialize) -> String {
    let s = serde_json::to_string(e).expect("failed to serialize enum variant to JSON");
    s.trim_matches('"').to_string()
}

/// Format an Expr node in Python-like syntax
pub fn format_expr(expr: &Expr) -> String {
    match expr {
        Expr::BoolOp(x) => {
            let op = match x.op {
                BoolOp::And => " and ",
                BoolOp::Or => " or ",
            };
            x.values
                .iter()
                .map(format_expr)
                .collect::<Vec<_>>()
                .join(op)
        }
        Expr::Named(x) => {
            format!("{} := {}", format_expr(&x.target), format_expr(&x.value))
        }
        Expr::BinOp(x) => {
            let op = match x.op {
                Operator::Add => " + ",
                Operator::Sub => " - ",
                Operator::Mult => " * ",
                Operator::MatMult => " @ ",
                Operator::Div => " / ",
                Operator::Mod => " % ",
                Operator::Pow => " ** ",
                Operator::LShift => " << ",
                Operator::RShift => " >> ",
                Operator::BitOr => " | ",
                Operator::BitXor => " ^ ",
                Operator::BitAnd => " & ",
                Operator::FloorDiv => " // ",
            };
            format!("{}{}{}", format_expr(&x.left), op, format_expr(&x.right))
        }
        Expr::UnaryOp(x) => {
            let op = match x.op {
                UnaryOp::Invert => "~",
                UnaryOp::Not => "not ",
                UnaryOp::UAdd => "+",
                UnaryOp::USub => "-",
            };
            format!("{}{}", op, format_expr(&x.operand))
        }
        Expr::Lambda(x) => {
            let params = if let Some(ref p) = x.parameters {
                if !p.is_empty() {
                    format!(" {}", format_parameters(p))
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            format!("lambda{}: {}", params, format_expr(&x.body))
        }
        Expr::If(x) => {
            format!(
                "{} if {} else {}",
                format_expr(&x.body),
                format_expr(&x.test),
                format_expr(&x.orelse)
            )
        }
        Expr::Dict(x) => {
            if x.items.is_empty() {
                "{}".to_string()
            } else {
                format_collection(
                    &x.items,
                    |item| {
                        if let Some(key) = &item.key {
                            format!("{}: {}", format_expr(key), format_expr(&item.value))
                        } else {
                            format!("**{}", format_expr(&item.value))
                        }
                    },
                    |items, _len| format!("{{{}}}", items),
                )
            }
        }
        Expr::Set(x) => {
            if x.elts.is_empty() {
                "set()".to_string()
            } else {
                format_collection(&x.elts, format_expr, |items, _len| format!("{{{}}}", items))
            }
        }
        Expr::ListComp(x) => {
            format!(
                "[{} {}]",
                format_expr(&x.elt),
                format_comprehensions(&x.generators)
            )
        }
        Expr::SetComp(x) => {
            format!(
                "{{{} {}}}",
                format_expr(&x.elt),
                format_comprehensions(&x.generators)
            )
        }
        Expr::DictComp(x) => {
            format!(
                "{{{}: {} {}}}",
                format_expr(&x.key),
                format_expr(&x.value),
                format_comprehensions(&x.generators)
            )
        }
        Expr::Generator(x) => {
            format!(
                "({} {})",
                format_expr(&x.elt),
                format_comprehensions(&x.generators)
            )
        }
        Expr::Await(x) => format!("await {}", format_expr(&x.value)),
        Expr::Yield(x) => {
            if let Some(value) = &x.value {
                format!("yield {}", format_expr(value))
            } else {
                "yield".to_string()
            }
        }
        Expr::YieldFrom(x) => format!("yield from {}", format_expr(&x.value)),
        Expr::Compare(x) => {
            let mut result = format_expr(&x.left);
            for (op, comparator) in x.ops.iter().zip(x.comparators.iter()) {
                let op_str = match op {
                    CmpOp::Eq => " == ",
                    CmpOp::NotEq => " != ",
                    CmpOp::Lt => " < ",
                    CmpOp::LtE => " <= ",
                    CmpOp::Gt => " > ",
                    CmpOp::GtE => " >= ",
                    CmpOp::Is => " is ",
                    CmpOp::IsNot => " is not ",
                    CmpOp::In => " in ",
                    CmpOp::NotIn => " not in ",
                };
                result.push_str(&format!("{}{}", op_str, format_expr(comparator)));
            }
            result
        }
        Expr::Call(x) => {
            let func = format_expr(&x.func);
            let args = x
                .arguments
                .args
                .iter()
                .map(format_expr)
                .chain(x.arguments.keywords.iter().map(|kw| {
                    if let Some(arg) = &kw.arg {
                        format!("{}={}", arg.as_str(), format_expr(&kw.value))
                    } else {
                        format!("**{}", format_expr(&kw.value))
                    }
                }))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({})", func, args)
        }
        Expr::FString(_x) => "f\"...\"".to_string(), // Simplified representation
        Expr::TString(_x) => "t\"...\"".to_string(), // Simplified representation
        Expr::StringLiteral(x) => format!("\"{}\"", x.value.to_str().escape_debug()),
        Expr::BytesLiteral(x) => format!("b{:?}", x.value),
        Expr::NumberLiteral(x) => format!("{:?}", x.value),
        Expr::BooleanLiteral(x) => if x.value { "True" } else { "False" }.to_string(),
        Expr::NoneLiteral(_) => "None".to_string(),
        Expr::EllipsisLiteral(_) => "...".to_string(),
        Expr::Attribute(x) => {
            format!("{}.{}", format_expr(&x.value), x.attr.as_str())
        }
        Expr::Subscript(x) => {
            format!("{}[{}]", format_expr(&x.value), format_expr(&x.slice))
        }
        Expr::Starred(x) => format!("*{}", format_expr(&x.value)),
        Expr::Name(x) => x.id.to_string(),
        Expr::List(x) => {
            format_collection(&x.elts, format_expr, |items, _len| format!("[{}]", items))
        }
        Expr::Tuple(x) => format_collection(&x.elts, format_expr, |items, len| {
            if len == 1 {
                format!("({},)", items)
            } else {
                format!("({})", items)
            }
        }),
        Expr::Slice(x) => {
            let lower = x.lower.as_ref().map(|e| format_expr(e)).unwrap_or_default();
            let upper = x.upper.as_ref().map(|e| format_expr(e)).unwrap_or_default();
            let step = x
                .step
                .as_ref()
                .map(|e| format!(":{}", format_expr(e)))
                .unwrap_or_default();
            format!("{}:{}{}", lower, upper, step)
        }
        Expr::IpyEscapeCommand(x) => format!("!{}", &x.value),
    }
}

/// Helper function to format a collection of items (list, tuple, set, dict)
/// Takes the items, a mapper function to format each item, and a wrapper function
fn format_collection<T, M, F>(items: &[T], mapper: M, wrapper: F) -> String
where
    M: Fn(&T) -> String,
    F: FnOnce(&str, usize) -> String,
{
    let formatted_items = items.iter().map(mapper).collect::<Vec<_>>().join(", ");
    wrapper(&formatted_items, items.len())
}

fn format_parameters(params: &ruff_python_ast::Parameters) -> String {
    let mut parts = Vec::new();

    // Positional-only parameters
    for param in &params.posonlyargs {
        parts.push(param.parameter.name.to_string());
    }
    if !params.posonlyargs.is_empty() {
        parts.push("/".to_string());
    }

    // Regular parameters
    for param in &params.args {
        parts.push(param.parameter.name.to_string());
    }

    // *args
    if let Some(vararg) = &params.vararg {
        parts.push(format!("*{}", vararg.name));
    }

    // Keyword-only parameters
    for param in &params.kwonlyargs {
        parts.push(param.parameter.name.to_string());
    }

    // **kwargs
    if let Some(kwarg) = &params.kwarg {
        parts.push(format!("**{}", kwarg.name));
    }

    parts.join(", ")
}

fn format_comprehensions(generators: &[ruff_python_ast::Comprehension]) -> String {
    generators
        .iter()
        .map(|generator| {
            let target = format_expr(&generator.target);
            let iter = format_expr(&generator.iter);
            let mut result = format!("for {} in {}", target, iter);

            for if_clause in &generator.ifs {
                result.push_str(&format!(" if {}", format_expr(if_clause)));
            }

            result
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use super::*;

    fn parse_expr(source: &str) -> Expr {
        let parsed = parse_unchecked_source(source, PySourceType::Python);
        let module = parsed.into_syntax();
        match module.body.into_iter().next().expect("empty module") {
            ruff_python_ast::Stmt::Expr(stmt) => *stmt.value,
            other => panic!("Expected expression statement, got {:?}", other),
        }
    }

    fn check(source: &str, expected: &str) {
        let expr = parse_expr(source);
        assert_eq!(format_expr(&expr), expected, "source: {source}");
    }

    #[test]
    fn test_name() {
        check("foo", "foo");
        check("_private", "_private");
    }

    #[test]
    fn test_attribute() {
        check("foo.bar", "foo.bar");
        check("a.b.c", "a.b.c");
    }

    #[test]
    fn test_call_no_args() {
        check("foo()", "foo()");
    }

    #[test]
    fn test_call_with_args() {
        check("foo(a, b)", "foo(a, b)");
        check("foo(a, x=1)", "foo(a, x=Int(1))");
    }

    #[test]
    fn test_call_method() {
        check("obj.method(x)", "obj.method(x)");
    }

    #[test]
    fn test_binop() {
        check("x + y", "x + y");
        check("a * b", "a * b");
        check("a // b", "a // b");
        check("a ** b", "a ** b");
    }

    #[test]
    fn test_unary_op() {
        check("-x", "-x");
        check("not x", "not x");
        check("~x", "~x");
    }

    #[test]
    fn test_lambda_no_params() {
        check("lambda: 1", "lambda: Int(1)");
    }

    #[test]
    fn test_lambda_with_params() {
        check("lambda x, y: x", "lambda x, y: x");
    }

    #[test]
    fn test_dict_empty() {
        check("{}", "{}");
    }

    #[test]
    fn test_dict_with_items() {
        check("{'a': 1, 'b': 2}", "{\"a\": Int(1), \"b\": Int(2)}");
    }

    #[test]
    fn test_list() {
        check("[1, 2, 3]", "[Int(1), Int(2), Int(3)]");
    }

    #[test]
    fn test_list_comprehension() {
        check("[x for x in items]", "[x for x in items]");
        check("[x for x in items if x]", "[x for x in items if x]");
    }

    #[test]
    fn test_slice() {
        check("a[1:2]", "a[Int(1):Int(2)]");
        check("a[1:]", "a[Int(1):]");
        check("a[:2]", "a[:Int(2)]");
        check("a[::2]", "a[::Int(2)]");
    }

    #[test]
    fn test_subscript() {
        check("a[0]", "a[Int(0)]");
        check("d['key']", "d[\"key\"]");
    }

    #[test]
    fn test_bool_op() {
        check("a and b", "a and b");
        check("a or b", "a or b");
    }

    #[test]
    fn test_compare() {
        check("x == 1", "x == Int(1)");
        check("x is None", "x is None");
        check("x not in items", "x not in items");
    }

    #[test]
    fn test_tuple() {
        check("(a, b)", "(a, b)");
    }

    #[test]
    fn test_starred() {
        check("*args", "*args");
    }

    #[test]
    fn test_if_expr() {
        check("a if cond else b", "a if cond else b");
    }

    #[test]
    fn test_literals() {
        check("None", "None");
        check("True", "True");
        check("False", "False");
        check("...", "...");
    }

    #[test]
    fn test_await() {
        // Parse inside an async function to make it valid
        let source = "async def f():\n await x";
        let parsed = parse_unchecked_source(source, PySourceType::Python);
        let module = parsed.into_syntax();
        let func = match &module.body[0] {
            ruff_python_ast::Stmt::FunctionDef(f) => f,
            other => panic!("Expected FunctionDef, got {:?}", other),
        };
        let expr = match &func.body[0] {
            ruff_python_ast::Stmt::Expr(stmt) => &*stmt.value,
            other => panic!("Expected Expr, got {:?}", other),
        };
        assert_eq!(format_expr(expr), "await x");
    }

    #[test]
    fn test_set() {
        check("{1, 2, 3}", "{Int(1), Int(2), Int(3)}");
    }

    #[test]
    fn test_named_expr() {
        // walrus operator parsed inside a context where it's valid
        check("(x := 5)", "x := Int(5)");
    }

    #[test]
    fn test_format_uncovered_binops_and_unary() {
        check("a - b", "a - b");
        check("a @ b", "a @ b");
        check("a / b", "a / b");
        check("a % b", "a % b");
        check("a << b", "a << b");
        check("a >> b", "a >> b");
        check("a | b", "a | b");
        check("a ^ b", "a ^ b");
        check("a & b", "a & b");
        check("+x", "+x");
    }

    #[test]
    fn test_format_comprehensions_and_generators() {
        check("{x for x in items}", "{x for x in items}");
        check("{k: v for k, v in items}", "{k: v for (k, v) in items}");
        check("(x for x in items)", "(x for x in items)");
    }

    #[test]
    fn test_format_misc_uncovered_exprs() {
        check("(a,)", "(a,)");
        check("{**d}", "{**d}");
        check("f(**kw)", "f(**kw)");
        check("f'{x}'", "f\"...\"");
    }
}
