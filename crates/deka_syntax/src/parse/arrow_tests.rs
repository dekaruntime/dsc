//! Arrow function literals (rfd#67 part 2, dsc#252).
//!
//! `(params) => expr` and `(params) => { … }` parse to `Expr::Function`
//! with a `form` tag. Every positive case here fails to parse (or parses to
//! `Expr::Paren`) if the `(`-lookahead in `parse_prefix` is removed.
use bumpalo::Bump;

use super::parse;
use crate::ast::{BinOp, Expr, FunctionForm, Stmt};

fn errors(source: &str) -> Vec<String> {
    let arena = Bump::new();
    let result = parse(source, &arena);
    result
        .errors
        .iter()
        .map(|e| format!("{}:{}: {}", e.line, e.column, e.message))
        .collect()
}

fn assert_parses(source: &str) {
    let got = errors(source);
    assert!(got.is_empty(), "{source}\n{got:?}");
}

/// Parse `source` and hand its first statement's initializer to `f`.
fn with_first_value(source: &str, f: impl FnOnce(&Expr<'_>)) {
    let arena = Bump::new();
    let result = parse(source, &arena);
    assert!(result.errors.is_empty(), "{source}\n{:?}", result.errors);
    let program = result.program.unwrap();
    match &program.statements[0] {
        Stmt::Const { value, .. } | Stmt::Let { value, .. } => f(value),
        Stmt::Expr { expr, .. } => f(expr),
        other => panic!("{source}: unexpected first statement {other:?}"),
    }
}

#[test]
fn expression_body_is_one_synthesized_return() {
    with_first_value("const f = (x) => x + 1", |value| {
        let Expr::Function {
            params,
            return_type,
            body,
            is_async,
            form,
            ..
        } = value
        else {
            panic!("expected a function literal, got {value:?}");
        };
        assert_eq!(*form, FunctionForm::ArrowExpr);
        assert!(!is_async);
        assert!(return_type.is_none(), "arrows have no return-type slot");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].binding.to_string(), "x");
        assert!(params[0].ty.is_none());
        let [Stmt::Return {
            value: Some(Expr::Binary { op: BinOp::Add, .. }),
            span,
        }] = body
        else {
            panic!("expected one `return x + 1`, got {body:?}");
        };
        // The synthesized return carries the expression's own span, so a
        // return-type diagnostic lands on the expression.
        assert_eq!((span.start.line, span.start.column), (1, 18));
    });
}

#[test]
fn block_body_keeps_its_statements() {
    with_first_value("const f = (x) => { const y = x * 2\n return y }", |value| {
        let Expr::Function { body, form, .. } = value else {
            panic!("expected a function literal, got {value:?}");
        };
        assert_eq!(*form, FunctionForm::ArrowBlock);
        assert_eq!(body.len(), 2);
        assert!(matches!(body[0], Stmt::Const { .. }));
        assert!(matches!(body[1], Stmt::Return { .. }));
    });
    // An empty block body is a literal that returns nothing.
    with_first_value("const f = () => {}", |value| {
        let Expr::Function { body, form, .. } = value else {
            panic!("expected a function literal, got {value:?}");
        };
        assert_eq!(*form, FunctionForm::ArrowBlock);
        assert!(body.is_empty());
    });
}

#[test]
fn parameters_use_the_fn_grammar() {
    // Zero parameters, and a mix of annotated and omitted annotations: the
    // parameter list is `parse_params`, exactly as for `fn`.
    with_first_value("const f = () => 1", |value| {
        let Expr::Function { params, .. } = value else {
            panic!("expected a function literal, got {value:?}");
        };
        assert!(params.is_empty());
    });
    with_first_value("const f = (x: number, y, z = 3) => x", |value| {
        let Expr::Function { params, .. } = value else {
            panic!("expected a function literal, got {value:?}");
        };
        assert_eq!(params.len(), 3);
        assert!(params[0].ty.is_some());
        assert!(params[1].ty.is_none());
        assert!(params[2].default_value.is_some());
    });
    // A tuple parameter, as `fn f([k, v]: [number, string])` allows.
    with_first_value("const f = ([k, v]) => k", |value| {
        let Expr::Function { params, .. } = value else {
            panic!("expected a function literal, got {value:?}");
        };
        assert_eq!(params[0].binding.to_string(), "[k, v]");
    });
}

#[test]
fn async_arrow() {
    for source in ["const f = async () => { return 1 }", "const f = async (x) => x"] {
        with_first_value(source, |value| {
            let Expr::Function { is_async, form, .. } = value else {
                panic!("expected a function literal, got {value:?}");
            };
            assert!(*is_async, "{source}");
            assert!(form.is_arrow(), "{source}");
        });
    }
    let got = errors("const f = async 1");
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].contains("expected `fn` or `(…) =>` after `async`"),
        "{got:?}"
    );
}

#[test]
fn arrows_nest_and_take_full_expressions() {
    // An expression body extends as far as any expression would: a ternary,
    // a nested arrow, a chained call.
    with_first_value("const f = (x) => xs.map((y) => x * y).length", |value| {
        let Expr::Function { body, .. } = value else {
            panic!("expected a function literal, got {value:?}");
        };
        let [Stmt::Return {
            value: Some(Expr::FieldAccess { object, .. }),
            ..
        }] = body
        else {
            panic!("expected `return <call>.length`, got {body:?}");
        };
        let Expr::Call { args, .. } = object else {
            panic!("expected a call, got {object:?}");
        };
        assert!(
            matches!(args[0], Expr::Function { form: FunctionForm::ArrowExpr, .. }),
            "{:?}",
            args[0]
        );
    });
    with_first_value("const f = (x) => x > 1 ? \"big\" : \"small\"", |value| {
        let Expr::Function { body, .. } = value else {
            panic!("expected a function literal, got {value:?}");
        };
        assert!(matches!(
            body,
            [Stmt::Return {
                value: Some(Expr::Ternary { .. }),
                ..
            }]
        ));
    });
    assert_parses("useEffect(() => { echo(\"x\") })");
    assert_parses("const doubled = xs.map((x) => x * 2)");
    assert_parses("run(() =>\n  1)");
    assert_parses("run(() =>\n  { return 1 })");
}

#[test]
fn parenthesized_expressions_are_unchanged() {
    // The lookahead commits only when `=>` follows the matching `)`.
    for (source, is_paren) in [
        ("const a = (x)", true),
        ("const a = (1 + 2) * 3", false),
        ("const a = (\n  1 + 2\n)", true),
        ("const a = f((x), y)", false),
        ("const a = c ? (x) : y", false),
    ] {
        with_first_value(source, |value| {
            assert_eq!(matches!(value, Expr::Paren { .. }), is_paren, "{source}: {value:?}");
            assert!(!contains_function(value), "{source}: {value:?}");
        });
    }
    // Match arms keep their own `=>`; an arrow can still be an arm's body.
    assert_parses("const r = match v { Ok(x) => x, Err(_) => 0 }");
    assert_parses("const r = match n { 1 => (x) => x, _ => (y) => y }");
}

fn contains_function(expr: &Expr<'_>) -> bool {
    match expr {
        Expr::Function { .. } => true,
        Expr::Paren { expr, .. } => contains_function(expr),
        Expr::Binary { left, right, .. } => contains_function(left) || contains_function(right),
        Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            contains_function(condition)
                || contains_function(then_branch)
                || contains_function(else_branch)
        }
        Expr::Call { args, .. } => args.iter().any(contains_function),
        _ => false,
    }
}

#[test]
fn newline_before_the_arrow_ends_the_statement() {
    // `(x)` is a complete parenthesized expression; the `=>` on the next
    // line starts a statement that cannot begin with `=>`.
    let got = errors("const f = (x)\n=> x");
    assert!(!got.is_empty());
    assert!(got[0].starts_with("2:1: "), "{got:?}");
}

#[test]
fn bare_parameter_names_the_parenthesized_form() {
    // Sami, rfd#67: parameters always need parens. The diagnostic must say
    // what to write, not just that `=>` was unexpected (dsc#243).
    for (source, position, name) in [
        ("const f = x => x + 1", "1:13", "x"),
        ("xs.map(item => item * 2)", "1:13", "item"),
    ] {
        let got = errors(source);
        assert_eq!(got.len(), 1, "{source}\n{got:?}");
        assert_eq!(
            got[0],
            format!(
                "{position}: arrow function parameters must be parenthesized: write `({name}) =>` instead of `{name} =>`"
            ),
            "{source}"
        );
    }
}

#[test]
fn multiline_jsx_body_needs_parens() {
    // dsc#245's rule, applied to an expression body.
    let bare = "const items = xs.map((i) =>\n  <li>\n    {i}\n  </li>\n)";
    let got = errors(bare);
    assert!(
        got.iter()
            .any(|e| e == "2:3: a multi-line JSX arrow body must be wrapped in parentheses"),
        "{got:?}"
    );
    assert_parses("const items = xs.map((i) => (\n  <li>\n    {i}\n  </li>\n))");
    assert_parses("const items = xs.map((i) => <li>{i}</li>)");
    // A block body has ordinary returns, so the return rule applies there.
    let got = errors("const items = xs.map((i) => {\n  return <li>\n    {i}\n  </li>\n})");
    assert!(
        got.iter()
            .any(|e| e.ends_with("a multi-line JSX return must be wrapped in parentheses")),
        "{got:?}"
    );
}
