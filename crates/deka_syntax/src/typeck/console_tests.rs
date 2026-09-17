//! `console` as a DekaScript global (rfd#44's console addition).
//!
//! `console` is a host surface, typed by [`super::expr::Checker::check_console_call`]
//! (`crates/deka_syntax/src/typeck/expr.rs`), not a stdlib import and not a
//! value — consistent with `deka.panic`/`deka.ui`. These tests cover every
//! method's arity/typing, the `Printable` predicate's accept/reject
//! boundary, and that `console` cannot be bound, aliased, or misused as a
//! plain identifier.

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
        .map(|e| e.message)
        .collect()
}

fn assert_ok(source: &str) {
    let got = errors(source);
    assert!(got.is_empty(), "{source}\n{got:?}");
}

fn assert_teaches(source: &str, needle: &str) {
    let got = errors(source);
    assert!(
        got.iter().any(|m| m.contains(needle)),
        "{source}\nexpected a diagnostic containing `{needle}`\ngot: {got:?}"
    );
}

// ---------------------------------------------------------------------
// `console` is not a value
// ---------------------------------------------------------------------

#[test]
fn console_is_not_a_value() {
    assert_teaches("const c = console;", "`console` is not a value");
}

#[test]
fn console_field_torn_off_is_not_a_value() {
    // `console.log` without a call also has no first-class form.
    assert_teaches("const f = console.log;", "`console` is not a value");
}

#[test]
fn console_alone_as_expression_statement_is_not_a_value() {
    assert_teaches("console;", "`console` is not a value");
}

#[test]
fn every_declared_console_method_is_recognized() {
    // `crate::console::METHODS` is the single list the checker's unknown-
    // method diagnostic and the LSP completion both read (dsc's "share the
    // function, don't test the agreement" rule) — this guards that the
    // checker's match in `check_console_call` actually recognizes every name
    // the list declares, so the two cannot silently drift apart.
    for method in crate::console::METHODS {
        let got = errors(&format!("console.{method}();"));
        assert!(
            !got.iter().any(|m| m.contains("has no method")),
            "declared method `{method}` was not recognized by the checker: {got:?}"
        );
    }
    assert_eq!(crate::console::METHODS.len(), 19, "rfd#44 lists 19 methods");
}

#[test]
fn unknown_console_method_names_the_method_not_the_value_diagnostic() {
    let got = errors("console.nope(1);");
    assert!(
        got.iter().any(|m| m.contains("console has no method `nope`")),
        "{got:?}"
    );
    assert!(
        !got.iter().any(|m| m.contains("is not a value")),
        "a mistyped method call must not read as \"console is not a value\": {got:?}"
    );
}

// ---------------------------------------------------------------------
// log / info / debug / warn / error — `(...values: Printable[]) void`
// ---------------------------------------------------------------------

#[test]
fn log_family_accepts_any_number_of_printable_values() {
    for method in ["log", "info", "debug", "warn", "error"] {
        assert_ok(&format!("console.{method}();"));
        assert_ok(&format!("console.{method}(\"hi\", 1, true);"));
    }
}

// ---------------------------------------------------------------------
// assert(condition: boolean, ...values: Printable[]) void
// ---------------------------------------------------------------------

#[test]
fn assert_requires_a_condition() {
    assert_teaches("console.assert();", "expects a `condition` argument");
}

#[test]
fn assert_condition_must_be_boolean() {
    assert_teaches(
        "console.assert(1);",
        "console.assert: `condition` expects `boolean`, got `number`",
    );
}

#[test]
fn assert_accepts_boolean_condition_and_printable_tail() {
    assert_ok("console.assert(true);");
    assert_ok("console.assert(1 == 2, \"unreachable\", 1);");
}

// ---------------------------------------------------------------------
// count / countReset / time / timeEnd — `(label?: string) void`
// ---------------------------------------------------------------------

#[test]
fn labelled_methods_accept_zero_or_one_string_label() {
    for method in ["count", "countReset", "time", "timeEnd"] {
        assert_ok(&format!("console.{method}();"));
        assert_ok(&format!("console.{method}(\"tick\");"));
    }
}

#[test]
fn labelled_methods_reject_a_non_string_label() {
    for method in ["count", "countReset", "time", "timeEnd"] {
        assert_teaches(
            &format!("console.{method}(1);"),
            &format!("console.{method}: `label` expects `string`, got `number`"),
        );
    }
}

#[test]
fn labelled_methods_reject_more_than_one_argument() {
    for method in ["count", "countReset", "time", "timeEnd"] {
        assert_teaches(
            &format!("console.{method}(\"a\", \"b\");"),
            "expects at most 1 argument",
        );
    }
}

// ---------------------------------------------------------------------
// timeLog(label?: string, ...values: Printable[]) void
// ---------------------------------------------------------------------

#[test]
fn time_log_accepts_optional_label_and_printable_tail() {
    assert_ok("console.timeLog();");
    assert_ok("console.timeLog(\"tick\");");
    assert_ok("console.timeLog(\"tick\", 1, \"done\");");
}

#[test]
fn time_log_rejects_non_string_label() {
    assert_teaches(
        "console.timeLog(1, 2);",
        "console.timeLog: `label` expects `string`, got `number`",
    );
}

// ---------------------------------------------------------------------
// group / groupCollapsed — `(...label: Printable[]) void`; groupEnd — `() void`
// ---------------------------------------------------------------------

#[test]
fn group_family_accepts_printable_labels() {
    assert_ok("console.group();");
    assert_ok("console.group(\"section\", 1);");
    assert_ok("console.groupCollapsed(\"section\");");
}

#[test]
fn group_end_and_clear_take_no_arguments() {
    assert_ok("console.groupEnd();");
    assert_ok("console.clear();");
    assert_teaches("console.groupEnd(1);", "expects no arguments");
    assert_teaches("console.clear(1);", "expects no arguments");
}

// ---------------------------------------------------------------------
// dir(item: Printable, options?: Printable) void
// ---------------------------------------------------------------------

#[test]
fn dir_requires_at_least_one_argument() {
    assert_teaches("console.dir();", "expects 1 or 2 arguments");
}

#[test]
fn dir_accepts_item_and_optional_options() {
    assert_ok("console.dir(1);");
    assert_ok("console.dir(1, \"verbose\");");
}

#[test]
fn dir_rejects_more_than_two_arguments() {
    assert_teaches("console.dir(1, 2, 3);", "expects 1 or 2 arguments");
}

// ---------------------------------------------------------------------
// dirxml / trace — `(...values: Printable[]) void`
// ---------------------------------------------------------------------

#[test]
fn dirxml_and_trace_accept_printable_variadics() {
    assert_ok("console.dirxml();");
    assert_ok("console.dirxml(1, \"a\");");
    assert_ok("console.trace();");
    assert_ok("console.trace(\"boom\");");
}

// ---------------------------------------------------------------------
// table(data: Printable, columns?: Array<string>) void
// ---------------------------------------------------------------------

#[test]
fn table_requires_data() {
    assert_teaches("console.table();", "expects 1 or 2 arguments");
}

#[test]
fn table_accepts_data_and_string_array_columns() {
    assert_ok("console.table(1);");
    assert_ok("console.table(1, [\"a\", \"b\"]);");
}

#[test]
fn table_rejects_non_string_array_columns() {
    assert_teaches(
        "console.table(1, [1, 2]);",
        "console.table: `columns` expects `Array<string>`",
    );
}

// ---------------------------------------------------------------------
// `Printable` accept/reject boundary
// ---------------------------------------------------------------------

#[test]
fn printable_accepts_a_struct() {
    assert_ok("struct S { x: number } console.log(S { x: 1 });");
}

#[test]
fn printable_accepts_nested_option_array_tuple() {
    assert_ok(
        "let v: Option<Array<[number, string]>> = Some([[1, \"a\"]]); console.log(v);",
    );
}

#[test]
fn printable_rejects_a_function_value_with_named_diagnostic() {
    // The exact wording rfd#44 gives as an example.
    assert_teaches(
        "fn double(x: number) number { return x * 2 } console.log(double);",
        "console.log: value of type fn(number) number has no printable form",
    );
}

#[test]
fn printable_rejection_names_the_calling_method() {
    assert_teaches(
        "fn f() void {} console.warn(f);",
        "console.warn: value of type fn() void has no printable form",
    );
    assert_teaches(
        "fn f() void {} console.table(f);",
        "console.table: value of type fn() void has no printable form",
    );
}
