//! Statement-level diagnostic tests.

use bumpalo::Bump;

use super::parse;
use super::FUNCTION_KEYWORD_ERROR;
use crate::diagnostics::Diagnostic;

fn errors(source: &str) -> Vec<Diagnostic> {
    let arena = Bump::new();
    parse(source, &arena).errors
}

#[test]
fn retired_function_keyword_at_statement_position() {
    let got = errors("function hello() {}\n");
    assert_eq!(got.len(), 1, "expected exactly one diagnostic, got {got:?}");
    assert_eq!(got[0].message, FUNCTION_KEYWORD_ERROR);
    assert_eq!(got[0].line, 1);
    assert_eq!(got[0].column, 1);
    assert_eq!(got[0].underline_length, 8);
}

#[test]
fn retired_function_keyword_after_export() {
    let got = errors("export function hello() {}\n");
    assert_eq!(got.len(), 1, "expected exactly one diagnostic, got {got:?}");
    assert_eq!(got[0].message, FUNCTION_KEYWORD_ERROR);
    assert_eq!(got[0].line, 1);
    assert_eq!(got[0].column, 8);
    assert_eq!(got[0].underline_length, 8);
}

#[test]
fn string_containing_function_is_not_diagnosed() {
    let got = errors("const s = \"function\"\n");
    assert!(got.is_empty(), "expected no diagnostics, got {got:?}");
}
