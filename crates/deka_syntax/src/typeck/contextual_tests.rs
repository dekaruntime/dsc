//! Contextual typing for function literals (rfd#67 part 1, dsc#251).
//!
//! A function literal in a position whose function type is known takes its
//! omitted parameter and return types from that position. Every positive
//! case here fails with `parameter … is missing a type annotation` if the
//! expected type stops reaching `check_function_expr`.
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

#[test]
fn call_argument_infers_parameter_and_return() {
    assert_ok(&format!(
        "{APPLY}\
         const ys = apply([1, 2], fn(x) {{ return x * 2 }})\n\
         const n: number = ys.length"
    ));
}

#[test]
fn inferred_parameter_has_the_expected_type_in_the_body() {
    // `x` is `number`, so a string binding from it is the ordinary
    // mismatch — proof the parameter was typed, not left open.
    assert_one_error(
        &format!("{APPLY}const ys = apply([1], fn(x) {{ const s: string = x; return x }})"),
        "2:49",
        "expected type `string`, found type `number`",
    );
}

#[test]
fn void_callback_needs_no_annotation() {
    assert_ok(
        "fn run(cb: fn() void) void { cb() }\n\
         run(fn() { const _x = 1 })",
    );
}

#[test]
fn builtin_map_callback_solves_element_and_result() {
    // The element type comes from the receiver; `U` is solved from the body.
    assert_ok(
        "const zs = [1, 2, 3].map(fn(x) { return string(x) })\n\
         fn first(xs: Array<string>) number { return xs.length }\n\
         const n = first(zs)",
    );
    assert_ok("const ws = [1, 2, 3].filter(fn(x) { return x > 1 })");
}

#[test]
fn generic_hof_solves_type_parameter_from_later_argument() {
    // The literal is visited after the plain arguments, whatever the
    // parameter order.
    assert_ok(
        "fn each<T>(f: fn(T) void, xs: Array<T>) void { for (const x of xs) { f(x) } }\n\
         each(fn(x) { const _n: number = x }, [1, 2])",
    );
}

#[test]
fn use_effect_needs_no_annotation() {
    assert_ok(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           useEffect(fn() { setCount(1) })\n\
           return <p>{string(count)}</p>\n\
         }",
    );
}

#[test]
fn use_effect_inferred_return_is_the_cleanup_slot() {
    // The omitted return type is `Option<fn() void>`, so a cleanup is
    // accepted and a non-function return is rejected exactly as with the
    // annotation written.
    assert_ok(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           useEffect(fn() { return Some(fn() { setCount(0) }) })\n\
           return <p>{string(count)}</p>\n\
         }",
    );
    assert_one_error(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           useEffect(fn() { return 1 })\n\
           return <p>{string(count)}</p>\n\
         }",
        "3:18",
        "expected return type `Option<fn() void>`, found type `number`",
    );
}

#[test]
fn interface_method_argument_is_contextually_typed() {
    assert_ok(
        "interface Sink { fn each(f: fn(number) void) void }\n\
         fn drain(s: Sink) void { s.each(fn(x) { const _n: number = x }) }",
    );
}

#[test]
fn annotated_binding_is_contextually_typed() {
    assert_ok("const f: fn(number) number = fn(x) { return x + 1 }");
}

#[test]
fn parenthesized_literal_is_contextually_typed() {
    assert_ok(&format!("{APPLY}const ys = apply([1], (fn(x) {{ return x }}))"));
}

#[test]
fn explicit_types_are_never_overridden() {
    // A written parameter type that disagrees with the position is the
    // ordinary argument mismatch, reported at the argument.
    assert_one_error(
        &format!("{APPLY}const ys = apply([1], fn(x: string) {{ return 1 }})"),
        "2:23",
        "expected argument type `fn(number) number`, found type `fn(string) number`",
    );
    // A fully annotated literal is unchanged, including its diagnostics.
    assert_one_error(
        &format!("{APPLY}const ys = apply([1], fn(x: number) number {{ return \"s\" }})"),
        "2:46",
        "expected return type `number`, found type `string`",
    );
}

#[test]
fn conflicting_return_errors_inside_the_argument() {
    assert_one_error(
        &format!("{APPLY}const ys = apply([1], fn(x) {{ return \"s\" }})"),
        "2:31",
        "expected return type `number`, found type `string`",
    );
}

#[test]
fn arity_mismatch_is_one_error_at_the_argument() {
    assert_one_error(
        &format!("{APPLY}const ys = apply([1], fn(x, y) {{ return x }})"),
        "2:23",
        "function literal has 2 parameters, but the expected type `fn(number) number` has 1",
    );
}

#[test]
fn no_expected_type_still_requires_annotations() {
    // The invariant: omitted annotations are legal only where an expected
    // type exists. An untyped binding supplies none.
    assert_one_error(
        "const f = fn(x) { return x }",
        "1:14",
        "parameter `x` is missing a type annotation",
    );
}

#[test]
fn unsolved_type_parameter_supplies_nothing() {
    // `T` has no other argument to solve it from; the open slot is not a
    // type the literal can take.
    assert_one_error(
        "fn call<T>(f: fn(T) void) void { }\n\
         call(fn(x) { const _y = x })",
        "2:9",
        "parameter `x` is missing a type annotation",
    );
}

#[test]
fn generic_callee_reports_body_errors_once() {
    // Before dsc#251 the inference pass and the check pass both reported
    // errors inside a callback passed to a generic function.
    assert_one_error(
        "fn ident<T>(x: T, f: fn(T) T) T { return f(x) }\n\
         const n = ident(1, fn(x: number) number { return \"s\" })",
        "2:43",
        "expected return type `number`, found type `string`",
    );
}
