//! Arrow function literals in the checker (rfd#67 part 2, dsc#252).
//!
//! An arrow is a function literal whose types come from its position
//! (dsc#251); these tests cover what is specific to the arrow spelling: the
//! no-context diagnostic, and the `(…) => expr` body. Every positive case
//! fails if arrows stop reaching `check_function_expr` with their form.
use bumpalo::Bump;

fn errors(source: &str) -> Vec<String> {
    let arena = Bump::new();
    let parsed = crate::parse(source, &arena);
    assert!(
        parsed.errors.is_empty(),
        "parse: {:?}\n{source}",
        parsed.errors
    );
    super::check_program(&parsed.program.unwrap(), source)
        .errors
        .into_iter()
        .map(|e| format!("{}:{}: {}", e.line, e.column, e.message))
        .collect()
}

fn assert_ok(source: &str) {
    let got = errors(source);
    assert!(got.is_empty(), "{source}\n{got:?}");
}

/// Exactly one diagnostic, at `line:column`, containing `needle`.
fn assert_one_error(source: &str, position: &str, needle: &str) {
    let got = errors(source);
    assert_eq!(got.len(), 1, "{source}\n{got:?}");
    assert!(
        got[0].starts_with(&format!("{position}: ")) && got[0].contains(needle),
        "{source}\nexpected `{position}: …{needle}…`\ngot: {got:?}"
    );
}

const APPLY: &str = "fn apply(xs: Array<number>, f: fn(number) number) Array<number> { return xs.map(f) }\n";
const RUN: &str = "fn run(cb: fn() void) void { cb() }\nfn tick() void { }\n";

#[test]
fn map_callback_infers_its_parameter() {
    assert_ok(
        "const doubled = [1, 2, 3].map((x) => x * 2)\n\
         const s: string = doubled.join(\",\")",
    );
    // `x` is `number`: a string binding from it is the ordinary mismatch,
    // proof the parameter was typed from the receiver, not left open.
    assert_one_error(
        "const ys = [1].map((x) => { const s: string = x\n return x })",
        "1:47",
        "expected type `string`, found type `number`",
    );
}

#[test]
fn use_effect_block_body() {
    // The dsc#252 target: `useEffect(() => { echo("x") })`.
    assert_ok(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           useEffect(() => { setCount(1) })\n\
           return <p>{string(count)}</p>\n\
         }",
    );
}

#[test]
fn use_effect_expression_body() {
    // A void expression body is a statement, not a `return void` into the
    // `Option<fn() void>` cleanup slot; a cleanup is still accepted; a
    // non-function value is still rejected, at the expression.
    assert_ok(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           useEffect(() => setCount(1))\n\
           return <p>{string(count)}</p>\n\
         }",
    );
    assert_ok(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           useEffect(() => Some(() => setCount(0)))\n\
           return <p>{string(count)}</p>\n\
         }",
    );
    assert_one_error(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           useEffect(() => 1)\n\
           return <p>{string(count)}</p>\n\
         }",
        "3:17",
        "expected return type `Option<fn() void>`, found type `number`",
    );
}

#[test]
fn void_expression_body_in_a_void_slot() {
    assert_ok(&format!("{RUN}run(() => tick())"));
    // A non-void value in a `void` slot is not silently dropped: the
    // expression body is the return value, and the block spelling is how
    // to say it is not.
    const EACH: &str = "fn each(xs: Array<number>, f: fn(number) void) void { for (const x of xs) { f(x) } }\n\
                        let total = 0\n";
    assert_ok(&format!("{EACH}each([1, 2], (x) => {{ total = total + x }})"));
    assert_one_error(
        &format!("{EACH}each([1, 2], (x) => total = total + x)"),
        "3:21",
        "expected return type `void`, found type `number`",
    );
}

#[test]
fn void_expression_body_where_a_value_is_required() {
    assert_one_error(
        &format!("{RUN}{APPLY}const ys = apply([1], (x) => tick())"),
        "4:30",
        "expected return type `number`, found type `void`",
    );
}

#[test]
fn expression_body_return_mismatch_lands_on_the_expression() {
    assert_one_error(
        &format!("{APPLY}const ys = apply([1], (x) => \"s\")"),
        "2:30",
        "expected return type `number`, found type `string`",
    );
}

#[test]
fn arity_mismatch_is_reported_for_arrows_too() {
    assert_one_error(
        &format!("{APPLY}const ys = apply([1], (x, y) => x)"),
        "2:23",
        "function literal has 2 parameters, but the expected type `fn(number) number` has 1",
    );
}

#[test]
fn no_expected_type_says_it_cannot_infer() {
    // dsc#243's complaint: a bare parser error. Now the checker names the
    // problem and both ways out. The `fn` spelling keeps its own wording.
    assert_one_error(
        "const f = (x) => x + 1",
        "1:12",
        "cannot infer the type of parameter `x`: no function type is expected at this position — annotate it (`(x: T) => …`) or use `fn(x: T) R { … }`",
    );
    assert_one_error(
        "const f = fn(x) { return x }",
        "1:14",
        "parameter `x` is missing a type annotation",
    );
}

#[test]
fn annotations_and_bodies_stand_in_for_context() {
    // A written parameter type needs no context, and a zero-parameter arrow
    // infers its return type from the body as every literal always has.
    assert_ok(
        "const inc = (x: number) => x + 1\n\
         const n: number = inc(1)",
    );
    assert_ok(
        "const one = () => 1\n\
         const n: number = one()",
    );
    assert_ok(
        "const f: fn(number) number = (x) => x * 2\n\
         const n: number = f(2)",
    );
}

#[test]
fn async_arrow_returns_a_promise() {
    assert_ok(
        "const later = async () => 1\n\
         const p: Promise<number> = later()",
    );
}
