//! dsc#272 / rfd#27's 2026-09-16 amendment: bridge calls type from the
//! embedded host declaration file instead of the deleted `BRIDGE_OPS`
//! hand-kept list, which typed every call as `Result<Infer, Infer>`.
//!
//! Each positive case here regresses to a diagnostic (an `unsafe { }`-free
//! stdlib function returning a bridge result stops typechecking, exactly
//! dsc#223 / deka#1115) if `check_bridge_call` goes back to returning a bare
//! `Result<Infer, Infer>`; each negative case regresses to *no* diagnostic if
//! the catalog lookup or arity/type checks are removed.
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

fn assert_one_error(source: &str, needle: &str) {
    let got = errors(source);
    assert_eq!(got.len(), 1, "{source}\n{got:?}");
    assert!(
        got[0].contains(needle),
        "{source}\nexpected an error containing `{needle}`\ngot: {got:?}"
    );
}

/// Issue #272's first listed test, verbatim: a sync bridge call assigned
/// straight to its stdlib wrapper's declared return type, no
/// `unsafe<Result<T,E>> { }` cast — the ceremony rfd#62 retired once the
/// catalog carries real types.
#[test]
fn sync_bridge_call_typechecks_with_declared_signature_and_no_cast() {
    assert_ok(
        "export fn random_bytes(len: number) Result<bytes, string> { return bridge crypto.random_bytes(len) }",
    );
}

/// An async op's call site types as `Promise<Result<T, E>>`, so `await`ing
/// it and returning the awaited value against a `Result<T, E>`-typed
/// function is exactly as legal as any other async stdlib wrapper — proof
/// the async flag still comes from the declaration (`async fn`), not a
/// hand-kept table.
#[test]
fn async_bridge_call_awaits_to_the_declared_result_type() {
    assert_ok(
        "export async fn read_file(path: string) Promise<Result<bytes, string>> { return await bridge fs.read_file(path) }",
    );
}

/// The bare (un-awaited) call itself types as `Promise<Result<T, E>>` — the
/// async flag came from `async fn` in the declaration file, not from a
/// hand-kept table (the deleted `BRIDGE_OPS`).
#[test]
fn async_bridge_call_without_await_types_as_a_promise() {
    assert_ok(
        "export fn read_file(path: string) Promise<Result<bytes, string>> { return bridge fs.read_file(path) }",
    );
}

#[test]
fn wrong_arity_is_a_diagnostic_at_the_call() {
    assert_one_error(
        "export fn f() Result<bytes, string> { return bridge crypto.random_bytes() }",
        "expects 1 argument, found 0",
    );
}

#[test]
fn wrong_argument_type_is_a_diagnostic_at_the_call() {
    assert_one_error(
        "export fn f(len: string) Result<bytes, string> { return bridge crypto.random_bytes(len) }",
        "expected argument type `number`, found type `string`",
    );
}

#[test]
fn unknown_action_on_a_known_kind_is_a_diagnostic() {
    assert_one_error(
        "export fn f() Result<bytes, string> { return bridge crypto.not_a_real_action() }",
        "unknown bridge action `crypto.not_a_real_action`",
    );
}

#[test]
fn unknown_kind_is_a_diagnostic() {
    assert_one_error(
        "export fn f() Result<bytes, string> { return bridge nope.random_bytes() }",
        "unknown bridge kind `nope`",
    );
}

/// rfd#27's decision 1: `bridge` is only legal as a call, and (as of this
/// amendment) as an ambient declaration block inside dsc's own embedded host
/// file. A user file writing that block form directly is always an error,
/// regardless of stdlib/application status.
#[test]
fn ambient_bridge_block_outside_the_host_file_is_an_error() {
    assert_one_error(
        "bridge crypto {\n  fn random_bytes(len: number) Result<bytes, string>\n}\n",
        "ambient `bridge` blocks are only valid in deka's own host declaration file",
    );
}

/// `JsValue` (rfd#39's declaration-files amendment, decision 6) resolves as
/// a type, so `db.stats`'s real declared signature — the one host action
/// that returns a free-form value — typechecks too, not just the primitive
/// ones.
#[test]
fn db_query_takes_js_value_and_returns_a_js_value_result() {
    assert_ok(
        "export fn stats(handle: number) Result<JsValue, string> { return bridge db.stats(handle) }",
    );
}
