//! DekaScript recursive-descent parser (Compiler v2).

use std::cell::Cell;
use std::rc::Rc;

use bumpalo::Bump;

use crate::ast::{Program, Span};
use crate::diagnostics::Diagnostic;
use crate::lexer::{Lexer, Token, TokenKind};

#[cfg(test)]
mod arrow_tests;
#[cfg(test)]
mod stmt_tests;
mod expr;
mod jsx;
mod pattern;
mod stmt;
pub use stmt::expr_has_top_level_await;
mod ty;
mod util;

pub struct ParseResult<'a> {
    pub program: Option<Program<'a>>,
    pub errors: Vec<Diagnostic>,
}

/// Maximum parser recursion depth before a `nesting too deep` diagnostic is
/// emitted instead of recursing further (dsc#72).
///
/// The parser is recursive descent, so every nesting level costs stack frames.
/// The worst chain (parenthesised/array/unary expressions) measures ~22 KB of
/// stack per level in a debug build; calls/blocks are ~10 KB/level and types
/// ~4 KB/level. Callers do not all have the 8 MiB main-thread stack:
/// `cargo test` threads, Rust's default spawned-thread stack, and tokio/LSP
/// workers all get ~2 MiB. 64 levels keeps the worst case at ~1.4 MiB — under
/// a 2 MiB stack with margin — while 8 MiB main-thread callers (the `dsc`
/// CLI) get ~6x headroom. Realistic code is well under 50 levels deep; 64 is
/// deliberately just above the range any human-written source plausibly
/// reaches (serde uses 128 with ~100 B/level frames; our frames are ~200x
/// larger, so our limit is ~200x smaller).
const MAX_NESTING_DEPTH: usize = 64;

/// Diagnostic message for the retired `function` keyword. Kept in one place so
/// the LSP quick-fix path can match it exactly.
pub const FUNCTION_KEYWORD_ERROR: &str =
    "`function` is not a DekaScript keyword; use `fn` (e.g. `fn name(args) ReturnType { ... }`); \
     `function` was retired with the PHP-era syntax";

/// RAII guard counting one live recursive parse frame. Acquired on entry to
/// every recursive parse function; `Drop` decrements, so early-return and
/// error paths (`?`) cannot leak the count.
struct DepthGuard {
    depth: Rc<Cell<usize>>,
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }
}

/// Parse a full `.ds` source file into a DekaScript AST.
pub fn parse<'a>(source: &'a str, arena: &'a Bump) -> ParseResult<'a> {
    let (program, errors) = parse_with_recovery(source, arena);
    ParseResult {
        program: if errors.is_empty() { program } else { None },
        errors,
    }
}

/// Parse like [`parse`], but keep the partially recovered program when errors
/// exist: `parse_program` already recovers at statement boundaries, so the
/// surviving statements still describe the scopes around them. Tooling that
/// must answer queries mid-edit (completion, hover) uses this; compilers keep
/// using [`parse`], which withholds the program on any error.
pub fn parse_recovering<'a>(source: &'a str, arena: &'a Bump) -> ParseResult<'a> {
    let (program, errors) = parse_with_recovery(source, arena);
    ParseResult { program, errors }
}

fn parse_with_recovery<'a>(
    source: &'a str,
    arena: &'a Bump,
) -> (Option<Program<'a>>, Vec<Diagnostic>) {
    let mut lexer = Lexer::new(source);
    let mut tokens: Vec<Token> = Vec::new();

    loop {
        let tok = lexer.next_token();
        // Comments are treated as whitespace. Newlines are kept so the parser
        // can implement optional semicolon insertion.
        if tok.kind == TokenKind::Comment {
            continue;
        }
        let is_eof = tok.kind == TokenKind::Eof;
        tokens.push(tok);
        if is_eof {
            break;
        }
    }

    let mut errors: Vec<Diagnostic> = lexer.diagnostics().to_vec();
    let mut parser = Parser::new(arena, source, tokens);
    let mut program = parser.parse_program();
    errors.extend(parser.errors);

    // Resolve syntactic ambiguities (e.g. enum member access) before handing
    // the AST to consumers.
    if let Some(ref mut program) = program {
        crate::canonicalize::resolve_enum_constructors(program, arena);
    }

    (program, errors)
}

/// True for tokens that are keywords (or keyword-like literals) in
/// statement/expression position but may still serve as names after `.` or
/// as object-literal keys. Keywords are contextual in name position:
/// `{ type: "text" }` and `o.type` (HTML props) must parse even though
/// `type` starts an alias declaration in statement position.
fn kind_is_name_capable(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::True
            | TokenKind::False
            | TokenKind::None
            | TokenKind::Const
            | TokenKind::Let
            | TokenKind::Mut
            | TokenKind::Function
            | TokenKind::Fn
            | TokenKind::Struct
            | TokenKind::Enum
            | TokenKind::Interface
            | TokenKind::Type
            | TokenKind::Alias
            | TokenKind::Import
            | TokenKind::Export
            | TokenKind::From
            | TokenKind::As
            | TokenKind::If
            | TokenKind::Else
            | TokenKind::For
            | TokenKind::Of
            | TokenKind::Return
            | TokenKind::Match
            | TokenKind::Unsafe
            | TokenKind::Build
            | TokenKind::Bridge
            | TokenKind::Await
            | TokenKind::Async
            | TokenKind::Pub
            | TokenKind::Break
            | TokenKind::Continue
    )
}

struct Parser<'a> {
    arena: &'a Bump,
    tokens: Vec<Token<'a>>,
    source: &'a str,
    pos: usize,
    prev: Token<'a>,
    errors: Vec<Diagnostic>,
    /// Live recursion depth, shared with outstanding [`DepthGuard`]s via
    /// `Rc<Cell<..>>` so the guard can decrement on drop without borrowing
    /// the parser (which is mutably borrowed by the parse functions
    /// themselves).
    depth: Rc<Cell<usize>>,
}

impl<'a> Parser<'a> {
    fn new(arena: &'a Bump, source: &'a str, tokens: Vec<Token<'a>>) -> Self {
        Self {
            arena,
            pos: 0,
            prev: util::eof_token(),
            errors: Vec::new(),
            tokens,
            source,
            depth: Rc::new(Cell::new(0)),
        }
    }

    /// Enter one level of parse recursion. Returns a guard that decrements
    /// the depth on drop, or `None` (after emitting a positioned diagnostic)
    /// when the nesting limit is exceeded. Every recursive `parse_*` function
    /// must acquire this guard before doing anything else.
    fn enter_recursion(&mut self) -> Option<DepthGuard> {
        let next = self.depth.get() + 1;
        if next > MAX_NESTING_DEPTH {
            self.error(format!(
                "nesting too deep (limit is {MAX_NESTING_DEPTH} levels)"
            ));
            return None;
        }
        self.depth.set(next);
        Some(DepthGuard {
            depth: Rc::clone(&self.depth),
        })
    }

    // ------------------------------------------------------------------
    // Token helpers
    // ------------------------------------------------------------------

    fn current(&self) -> &Token<'a> {
        &self.tokens[self.pos]
    }

    fn current_kind(&self) -> TokenKind {
        self.current().kind
    }

    fn current_span(&self) -> Span {
        self.current().span
    }

    fn current_text(&self) -> &'a str {
        self.current().text
    }

    fn at(&self, kind: TokenKind) -> bool {
        self.current_kind() == kind
    }

    fn at_end(&self) -> bool {
        self.at(TokenKind::Eof)
    }

    fn advance(&mut self) {
        self.prev = self.current().clone();
        if !self.at_end() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Option<()> {
        if self.eat(kind) {
            Some(())
        } else {
            self.error(format!(
                "expected `{}`, found `{}`",
                util::token_name(kind),
                util::token_name(self.current_kind())
            ));
            None
        }
    }

    fn expect_identifier(&mut self) -> Option<&'a str> {
        self.skip_newlines();
        if self.at(TokenKind::Identifier) {
            let name = self.bump_str(self.current_text());
            self.advance();
            Some(name)
        } else {
            self.error(format!(
                "expected identifier, found `{}`",
                util::token_name(self.current_kind())
            ));
            None
        }
    }

    /// Accept `identifier` or any keyword as a field/enum-case name. Keywords
    /// are contextual in name position: after `.` the token can only be a
    /// name, so `o.type` (HTML props) must parse. `none` is included via the
    /// same rule so `Option.None` parses for canonicalization.
    fn expect_field_name(&mut self) -> Option<&'a str> {
        self.skip_newlines();
        if kind_is_name_capable(self.current_kind()) {
            let name = self.bump_str(self.current_text());
            self.advance();
            Some(name)
        } else {
            self.error(format!(
                "expected identifier, found `{}`",
                util::token_name(self.current_kind())
            ));
            None
        }
    }

    fn span_from(&self, start: crate::ast::Pos, start_byte: usize) -> Span {
        Span {
            start,
            end: self.prev.span.end,
            byte_start: start_byte,
            byte_end: self.prev.span.byte_end,
        }
    }

    fn span_start(&self) -> (crate::ast::Pos, usize) {
        (self.current_span().start, self.current_span().byte_start)
    }

    fn skip_newlines(&mut self) {
        while self.at(TokenKind::Newline) {
            self.advance();
        }
    }

    /// Look past any immediately-following newlines and return the kind of
    /// the first non-newline token. Does not advance the parser.
    fn peek_after_newlines(&self) -> TokenKind {
        let mut i = self.pos;
        while i < self.tokens.len() && self.tokens[i].kind == TokenKind::Newline {
            i += 1;
        }
        self.tokens.get(i).map(|t| t.kind).unwrap_or(TokenKind::Eof)
    }

    /// True when the current `bridge` token starts an ambient declaration
    /// block (`bridge crypto { ... }`) rather than a bridge call expression
    /// (`bridge crypto.random_bytes(...)`): lookahead is `identifier {`,
    /// skipping stray newlines, without consuming any tokens.
    fn bridge_decl_ahead(&self) -> bool {
        let mut i = self.pos + 1;
        while i < self.tokens.len() && self.tokens[i].kind == TokenKind::Newline {
            i += 1;
        }
        if self.tokens.get(i).map(|t| t.kind) != Some(TokenKind::Identifier) {
            return false;
        }
        i += 1;
        while i < self.tokens.len() && self.tokens[i].kind == TokenKind::Newline {
            i += 1;
        }
        matches!(self.tokens.get(i).map(|t| t.kind), Some(TokenKind::LBrace))
    }

    fn bump_str(&self, s: &str) -> &'a str {
        self.arena.alloc_str(s)
    }

    fn error(&mut self, message: impl Into<String>) {
        let pos = self.current_span().start;
        self.errors
            .push(Diagnostic::error(pos.line, pos.column, message));
    }

    /// Like `error`, but reports at an explicit position instead of the
    /// current token — used when the offending span has already been
    /// consumed (e.g. dsc#245's unparenthesized multi-line JSX return).
    fn error_at(&mut self, pos: crate::ast::Pos, message: impl Into<String>) {
        self.errors
            .push(Diagnostic::error(pos.line, pos.column, message));
    }

    /// dsc#227: `function` is the first keyword newcomers type, but it was
    /// retired with the PHP-era syntax. Point the diagnostic at the keyword
    /// itself with an 8-character underline so editors can offer a `fn` rewrite,
    /// then skip past the rest of the invalid declaration so we do not emit a
    /// second "expected expression" diagnostic on the closing brace.
    fn reject_retired_function_keyword(&mut self, in_block: bool) {
        let span = self.current_span();
        self.errors.push(
            Diagnostic::error(span.start.line, span.start.column, FUNCTION_KEYWORD_ERROR)
                .with_underline(8),
        );
        self.advance(); // skip `function`
        self.synchronize();
        if !in_block && self.at(TokenKind::RBrace) {
            self.advance();
        }
    }

    /// Diagnose the colon at its position, then consume it so the rest of the
    /// signature can still be parsed without cascading errors (#171, #188).
    /// Keep this validation permanent, including summoned signatures.
    fn reject_return_type_colon(&mut self) {
        if self.at(TokenKind::Colon) {
            self.error("return types take no colon; remove `:`");
            self.advance();
        }
    }

    /// Reject a nested binding pattern once, at the first inner pattern.
    /// Consume the whole outer pattern to avoid cascading parser errors. Flat
    /// patterns are left untouched, including in positions that reject them.
    fn reject_nested_tuple_pattern(&mut self) -> bool {
        if !self.at(TokenKind::LBracket) {
            return false;
        }
        let mut depth = 0;
        let mut inner = None;
        let mut inner_end = None;
        let mut outer_end = None;
        for (offset, token) in self.tokens[self.pos..].iter().enumerate() {
            match token.kind {
                TokenKind::LBracket => {
                    depth += 1;
                    if depth == 2 && inner.is_none() {
                        inner = Some(token.span);
                    }
                }
                TokenKind::RBracket => {
                    if depth == 2 && inner_end.is_none() {
                        inner_end = Some(token.span.byte_end);
                    }
                    depth -= 1;
                    if depth == 0 {
                        outer_end = Some(self.pos + offset);
                        break;
                    }
                }
                TokenKind::Eof => break,
                _ => {}
            }
        }
        let (Some(inner), Some(inner_end), Some(outer_end)) = (inner, inner_end, outer_end) else {
            return false;
        };
        self.errors.push(
            Diagnostic::error(
                inner.start.line,
                inner.start.column,
                "nested tuple patterns are not supported; destructure in two steps: `const [pair, label] = ...` then `const [x, y] = pair`",
            )
            .with_underline(inner_end - inner.byte_start),
        );
        while self.pos <= outer_end {
            self.advance();
        }
        true
    }

    fn synchronize(&mut self) {
        while !self.at_end() && !self.at(TokenKind::Semicolon) && !self.at(TokenKind::RBrace) {
            self.advance();
        }
        if self.at(TokenKind::Semicolon) {
            self.advance();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinOp, Expr, Pattern, Stmt, TemplatePart, Type, UnOp};

    // dsc#72: the parser must refuse pathological nesting with a positioned
    // diagnostic instead of recursing until the process aborts (SIGABRT).
    // These run on the ~2 MiB test-thread stack — the tightest stack any
    // caller has — so they also pin the stack-safety of the chosen limit.

    #[test]
    fn nesting_below_limit_parses() {
        let arena = Bump::new();
        let source = format!("const x = {}{}{}", "(".repeat(32), "1", ")".repeat(32));
        let result = parse(&source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn nesting_at_limit_boundary() {
        // Each nested block is one `parse_statement` recursion level, so 64
        // blocks is exactly MAX_NESTING_DEPTH live frames: must parse.
        let arena = Bump::new();
        let source = "{".repeat(MAX_NESTING_DEPTH) + &"}".repeat(MAX_NESTING_DEPTH);
        let result = parse(&source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);

        // One more level must fail with the depth diagnostic, not an abort.
        let source =
            "{".repeat(MAX_NESTING_DEPTH + 1) + &"}".repeat(MAX_NESTING_DEPTH + 1);
        let result = parse(&source, &arena);
        assert!(result.program.is_none());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("nesting too deep")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn deep_nesting_yields_positioned_diagnostic_never_abort() {
        // Thousands of levels of every recursive parse path: expression
        // nesting (parens, arrays, calls, unary), statement nesting (blocks),
        // type nesting (generics), and JSX children nesting. Before dsc#72
        // each of these aborted the process; now the depth limit fires first.
        let mut sources: Vec<String> = vec![
            format!("const x = {}{}", "(".repeat(5000), "1"),
            format!("const x = {}1", "[".repeat(5000)),
            format!("const x = {}{}", "f(".repeat(5000), "1"),
            format!("const x = {}1", "!".repeat(5000)),
            "{".repeat(5000),
            format!(
                "const x: {}number{} = 1",
                "Option<".repeat(5000),
                ">".repeat(5000)
            ),
            format!(
                "export fn P() {{ return {}text{} }}",
                "<div>".repeat(5000),
                "</div>".repeat(5000)
            ),
        ];
        // Balanced variants too: they used to overflow while building the AST,
        // not on the error path.
        sources.push(format!(
            "const x = {}{}{}",
            "(".repeat(5000),
            "1",
            ")".repeat(5000)
        ));

        for source in &sources {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(
                result.program.is_none() && !result.errors.is_empty(),
                "deep input must not parse: {}",
                &source[..source.len().min(40)]
            );
            assert!(
                result
                    .errors
                    .iter()
                    .any(|e| e.message.contains("nesting too deep")),
                "expected a depth diagnostic for {}: {:?}",
                &source[..source.len().min(40)],
                result.errors
            );
            for e in &result.errors {
                assert!(e.line >= 1 && e.column >= 1, "unpositioned: {e:?}");
            }
        }
    }

    #[test]
    fn flat_tuple_parameters_parse() {
        for source in [
            "fn f([a, b]: [number, number]) number { return a + b }",
            "const f = fn([k, v]: [string, number]) string { return k };",
            "fn f(prefix: string, [a, b,]: [number, number], suffix: string) {}",
            "fn f([\n a,\n b,\n]: [number, number]) {}",
            "fn f([a, b]) {}",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.errors.is_empty(), "{source}: {:?}", result.errors);
            assert!(result.program.is_some());
        }
        let arena = Bump::new();
        let result = parse("fn f([a, b]: [number, number]) {}", &arena);
        let program = result.program.unwrap();
        let crate::Stmt::Function { params, .. } = &program.statements[0] else {
            panic!("function")
        };
        assert!(matches!(
            params[0].binding,
            crate::ParamBinding::Tuple(["a", "b"])
        ));
    }

    #[test]
    fn tuple_parameters_require_tuple_annotations() {
        for source in [
            "fn f([x, y]: number[]) {}",
            "const f = fn([x, y]: number[]) {};",
            "fn f([a, b]: Array<number>) {}",
            "const f = fn([a, b]: number) {};",
            "fn f([x, y]: [number, number][]) {}",
            "fn f([x, y]: [number, number] | number) {}",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.program.is_none(), "{source}");
            assert_eq!(result.errors.len(), 1, "{source}: {:?}", result.errors);
            assert_eq!(result.errors[0].message, "destructuring parameter requires a tuple type with exact arity — annotate as [number, number], or take the array and index with proofs; expected identifier, found ``[`` for a non-tuple parameter");
        }
    }

    #[test]
    fn nested_tuple_patterns_teach_at_the_inner_pattern() {
        let mut messages = Vec::new();
        for (source, line, column, length) in [
            ("const [[x, y], label] = f();", 1, 8, 6),
            ("let [label, [x, y]] = f();", 1, 13, 6),
            ("fn f([[x, y], label]) {}", 1, 7, 6),
            ("fn f([[x, y], label]: [[number, number], string]) {}", 1, 7, 6),
            ("const f = fn ([[x, y], label]) {};", 1, 16, 6),
            ("const [[[x, y], z], label] = f();", 1, 8, 11),
            ("let [[[x, y], z], label] = f();", 1, 6, 11),
            ("fn f([[[x, y], z], label]) {}", 1, 7, 11),
            ("const [\n  [x, y],\n  label\n] = f();", 2, 3, 6),
            ("const [[x, y], [z, w]] = f();", 1, 8, 6),
            ("fn f(\n  [[x, y], label]\n) {}", 2, 4, 6),
            ("fn f(prefix: number, [label, [x, y]]) {}", 1, 30, 6),
            ("interface F { fn f([[x, y], label]) }", 1, 21, 6),
            (r#"summon { f([[x, y], label]) number } from "./f.mjs""#, 1, 13, 6),
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.program.is_none(), "{source}");
            assert_eq!(result.errors.len(), 1, "{source}: {:?}", result.errors);
            let error = &result.errors[0];
            assert_eq!(
                (error.line, error.column, error.underline_length),
                (line, column, length),
                "{source}"
            );
            assert_eq!(error.severity, crate::diagnostics::Severity::Error);
            messages.push(error.message.clone());
        }
        // One golden for the wording; every position shares that diagnostic.
        insta::assert_snapshot!(messages[0], @"nested tuple patterns are not supported; destructure in two steps: `const [pair, label] = ...` then `const [x, y] = pair`");
        assert!(messages.iter().all(|message| message == &messages[0]));
    }

    #[test]
    fn flat_tuple_bindings_are_unchanged() {
        for (source, expected_const) in [
            ("const [k, v]: [number, string] = [1, \"value\"];", true),
            ("let [k, v]: [number, string] = [1, \"value\"];", false),
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            let program = result.program.unwrap();
            let Stmt::TupleBinding {
                names, is_const, ..
            } = &program.statements[0]
            else {
                panic!("expected tuple binding");
            };
            assert_eq!(*names, ["k", "v"]);
            assert_eq!(*is_const, expected_const);
            let checked = crate::check_program(&program, source);
            assert!(checked.errors.is_empty(), "{:?}", checked.errors);
        }
    }

    #[test]
    fn return_type_colon_is_rejected_at_the_colon() {
        for source in [
            "fn f(): number {}",
            "fn f() : number {}",
            "fn (p P) get(): number { return p.x }",
            "fn (p P) get() : number { return p.x }",
            "super struct P { x: number }\nfn (p P) get(): number { return p.x }",
            "super struct P { x: number }\nfn (p P) get() : number { return p.x }",
            "export async fn f(): number { return 1 }",
            "const f = fn (): number { return 1 }",
            "const f = fn () : number { return 1 }",
            r#"summon { parse(json: string): Exception<Claims, JsError> } from "./parse.mjs""#,
            r#"summon { parse(json: string) : Exception<Claims, JsError> } from "./parse.mjs""#,
            r#"summon { total get(): number } from "./shim.mjs""#,
            r#"summon { fn get(): number } from "./shim.mjs""#,
            r#"summon { total fn get() : number } from "./shim.mjs""#,
            "alias Callback = fn(number): string",
            "alias Callback = fn(number) : string",
            r#"summon { total map(f: fn(number): string) string } from "./shim.mjs""#,
            "interface P { fn get(): number }",
            "interface P { fn get() : number }",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.program.is_none(), "{source}");
            assert_eq!(result.errors.len(), 1, "{source}: {:?}", result.errors);
            let error = &result.errors[0];
            assert_eq!(error.message, "return types take no colon; remove `:`");
            let colon = source.rfind(':').unwrap();
            let prefix = &source[..colon];
            assert_eq!(
                error.line,
                prefix.bytes().filter(|&b| b == b'\n').count() + 1
            );
            assert_eq!(error.column, prefix.rsplit('\n').next().unwrap().len() + 1);

            // Removing exactly the diagnosed colon must make the signature valid.
            let corrected = format!("{}{}", &source[..colon], &source[colon + 1..]);
            let result = parse(&corrected, &arena);
            assert!(result.errors.is_empty(), "{corrected}: {:?}", result.errors);
            assert!(result.program.is_some(), "{corrected}");
        }
    }

    #[test]
    fn return_type_colon_diagnostics_fixture() {
        let source = include_str!(
            "../../../../tests/fixtures/diagnostics/return_type_colon/return_type_colon.fail.ds"
        );
        let arena = Bump::new();
        let result = parse(source, &arena);
        assert!(result.program.is_none());
        assert_eq!(result.errors.len(), 5, "{:?}", result.errors);
        for (error, (line, column)) in
            result
                .errors
                .iter()
                .zip([(1, 7), (2, 8), (3, 25), (4, 18), (6, 15)])
        {
            assert_eq!(error.message, "return types take no colon; remove `:`");
            assert_eq!(error.column, column);
            assert_eq!(error.line, line);
        }
    }

    #[test]
    fn parse_multiline_call_args() {
        let arena = Bump::new();
        let result = parse("echo(\n  \"hi\"\n)", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn parse_multiline_call_args_with_commas() {
        let arena = Bump::new();
        let result = parse("f(\n  a,\n  b,\n  c\n)", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn parse_multiline_index_access() {
        let arena = Bump::new();
        let result = parse("const x = arr[\n  0\n]", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn parse_keyword_as_object_key() {
        let arena = Bump::new();
        let result = parse("const o = { type: \"text\" }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn parse_keyword_as_member_name() {
        let arena = Bump::new();
        let result = parse("const t = o.type", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn statement_keyword_without_colon_is_not_an_object_key() {
        // `break` in a match arm body must keep erroring as a bad object key,
        // not parse as a keyword key (deka#481 colon-lookahead rule).
        let arena = Bump::new();
        let result = parse(
            "for (let i = 0; i < 3; i = i + 1) {\n  match (i) {\n    2 => { break },\n    _ => {}\n  }\n}",
            &arena,
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("expected object key")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn parse_const_number() {
        let arena = Bump::new();
        let result = parse("const x = 42;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert_eq!(program.statements.len(), 1);

        match &program.statements[0] {
            Stmt::Const {
                name, ty, value, ..
            } => {
                assert_eq!(name.to_string(), "x");
                assert!(ty.is_none());
                match value {
                    Expr::Number { value, .. } => assert_eq!(*value, 42.0),
                    _ => panic!("expected number literal"),
                }
            }
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_function_add() {
        let arena = Bump::new();
        let result = parse(
            "fn add(a: number, b: number) number { return a + b; }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert_eq!(program.statements.len(), 1);

        match &program.statements[0] {
            Stmt::Function {
                name,
                params,
                return_type,
                body,
                ..
            } => {
                assert_eq!(name.to_string(), "add");
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].binding.to_string(), "a");
                assert_eq!(params[1].binding.to_string(), "b");
                assert!(matches!(
                    &params[0].ty,
                    Some(Type::Named { name, .. }) if name.to_string() == "number"
                ));
                assert!(matches!(
                    &params[1].ty,
                    Some(Type::Named { name, .. }) if name.to_string() == "number"
                ));
                assert!(matches!(
                    return_type,
                    Some(Type::Named { name, .. }) if name.to_string() == "number"
                ));
                assert_eq!(body.len(), 1);

                match &body[0] {
                    Stmt::Return {
                        value:
                            Some(Expr::Binary {
                                op: BinOp::Add,
                                left,
                                right,
                                ..
                            }),
                        ..
                    } => {
                        match *left {
                            Expr::Identifier { name, .. } => assert_eq!(name.to_string(), "a"),
                            _ => panic!("expected identifier `a`"),
                        }
                        match *right {
                            Expr::Identifier { name, .. } => assert_eq!(name.to_string(), "b"),
                            _ => panic!("expected identifier `b`"),
                        }
                    }
                    _ => panic!("expected return a + b"),
                }
            }
            _ => panic!("expected function declaration"),
        }
    }

    #[test]
    fn parse_console_log_call() {
        let arena = Bump::new();
        let result = parse("console.log(\"hello\");", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert_eq!(program.statements.len(), 1);

        match &program.statements[0] {
            Stmt::Expr { expr, .. } => match expr {
                Expr::Call {
                    callee,
                    type_args,
                    args,
                    ..
                } => {
                    assert!(type_args.is_empty());
                    assert_eq!(args.len(), 1);
                    match &args[0] {
                        Expr::String { value, .. } => assert_eq!(value.to_string(), "hello"),
                        _ => panic!("expected string argument"),
                    }
                    match callee {
                        Expr::FieldAccess { object, field, .. } => {
                            match object {
                                Expr::Identifier { name, .. } => {
                                    assert_eq!(name.to_string(), "console")
                                }
                                _ => panic!("expected identifier `console`"),
                            }
                            assert_eq!(field.to_string(), "log");
                        }
                        _ => panic!("expected field access callee"),
                    }
                }
                _ => panic!("expected call expression"),
            },
            _ => panic!("expected expression statement"),
        }
    }

    #[test]
    fn parse_error_missing_expression() {
        let arena = Bump::new();
        let result = parse("const x = ;", &arena);
        assert!(result.program.is_none());
        assert!(!result.errors.is_empty());
    }

    #[test]
    fn parse_nested_generic_type_splits_shr() {
        // The lexer emits `>>` as one Shr token; the type parser must split
        // it when closing nested generics (deka#465).
        let arena = Bump::new();
        let result = parse("const x: Option<Option<number>> = None;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { ty, .. } => match ty {
                Some(Type::Generic { base, args, .. }) => {
                    assert_eq!(base.to_string(), "Option");
                    assert_eq!(args.len(), 1);
                    match &args[0] {
                        Type::Generic { base, args, .. } => {
                            assert_eq!(base.to_string(), "Option");
                            assert_eq!(args.len(), 1);
                            match &args[0] {
                                Type::Named { name, .. } => {
                                    assert_eq!(name.to_string(), "number")
                                }
                                _ => panic!("expected number"),
                            }
                        }
                        _ => panic!("expected inner Option<number> generic"),
                    }
                }
                _ => panic!("expected Option<Option<number>> generic type"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_triple_nested_generic_type() {
        // `>>>` lexes as Shr + Gt; the split must compose with a plain Gt.
        let arena = Bump::new();
        let result = parse("const x: Option<Option<Option<number>>> = None;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn parse_let_with_type_and_optional() {
        let arena = Bump::new();
        let result = parse("let y: Option<string> = \"hi\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Let {
                name, ty, value, ..
            } => {
                assert_eq!(name.to_string(), "y");
                match ty {
                    Some(Type::Generic { base, args, .. }) => {
                        assert_eq!(base.to_string(), "Option");
                        assert_eq!(args.len(), 1);
                        match &args[0] {
                            Type::Named { name, .. } => assert_eq!(name.to_string(), "string"),
                            _ => panic!("expected string"),
                        }
                    }
                    _ => panic!("expected Option<string> generic type"),
                }
                match value {
                    Expr::String { value, .. } => assert_eq!(value.to_string(), "hi"),
                    _ => panic!("expected string literal"),
                }
            }
            _ => panic!("expected let declaration"),
        }
    }

    #[test]
    fn parse_function_type() {
        let arena = Bump::new();
        let result = parse("const f: fn(number) string = None;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { ty, .. } => match ty {
                Some(Type::Function { params, ret, .. }) => {
                    assert_eq!(params.len(), 1);
                    match &params[0] {
                        Type::Named { name, .. } => assert_eq!(name.to_string(), "number"),
                        _ => panic!("expected number param type"),
                    }
                    match ret {
                        Type::Named { name, .. } => assert_eq!(name.to_string(), "string"),
                        _ => panic!("expected string return type"),
                    }
                }
                _ => panic!("expected function type"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_union_type() {
        // `string | number` in a binding annotation (rfd#42, deka#530).
        let arena = Bump::new();
        let result = parse("const x: string | number = 1;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { ty, .. } => match ty {
                Some(Type::Union { members, .. }) => {
                    assert_eq!(members.len(), 2);
                    assert!(matches!(&members[0], Type::Named { name, .. } if name == &"string"));
                    assert!(matches!(&members[1], Type::Named { name, .. } if name == &"number"));
                }
                _ => panic!("expected union type, got {:?}", ty),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_union_type_in_param_and_return() {
        let arena = Bump::new();
        let result = parse(
            "fn f(a: string | number) string | number { return a; }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function {
                params,
                return_type,
                ..
            } => {
                assert!(
                    matches!(&params[0].ty, Some(Type::Union { members, .. }) if members.len() == 2)
                );
                assert!(
                    matches!(return_type, Some(Type::Union { members, .. }) if members.len() == 2)
                );
            }
            _ => panic!("expected function declaration"),
        }
    }

    #[test]
    fn parse_union_postfix_question_binds_tighter_than_bar() {
        // `A | B?` must parse as `A | (B?)`, not `(A | B)?` (rfd#42).
        let arena = Bump::new();
        let result = parse("const x: string | number? = 1;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { ty, .. } => match ty {
                Some(Type::Union { members, .. }) => {
                    assert_eq!(members.len(), 2);
                    assert!(matches!(&members[0], Type::Named { name, .. } if name == &"string"));
                    assert!(
                        matches!(&members[1], Type::Option { inner, .. } if matches!(&**inner, Type::Named { name, .. } if name == &"number"))
                    );
                }
                _ => panic!("expected union type, got {:?}", ty),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_grouped_union_with_postfix_question() {
        // `(A | B)?` groups the union, then `?` applies to the whole group.
        let arena = Bump::new();
        let result = parse("const x: (string | number)? = None;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { ty, .. } => match ty {
                Some(Type::Option { inner, .. }) => {
                    assert!(matches!(&**inner, Type::Union { members, .. } if members.len() == 2));
                }
                _ => panic!("expected optional union type, got {:?}", ty),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_union_inside_generic_args() {
        let arena = Bump::new();
        let result = parse("const x: Array<string | number> = [];", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { ty, .. } => match ty {
                Some(Type::Generic { base, args, .. }) => {
                    assert_eq!(base, &"Array");
                    assert_eq!(args.len(), 1);
                    assert!(matches!(&args[0], Type::Union { members, .. } if members.len() == 2));
                }
                _ => panic!("expected generic with union args, got {:?}", ty),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_union_in_struct_field_and_type_alias() {
        let arena = Bump::new();
        let result = parse(
            "struct Box { v: string | number } alias Alias = string | number;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn parse_unary_and_binary_precedence() {
        let arena = Bump::new();
        let result = parse("const z = -a.b + c * d;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Binary {
                    op: BinOp::Add,
                    left,
                    right,
                    ..
                } => {
                    // left: -(a.b)
                    match *left {
                        Expr::Unary {
                            op: UnOp::Neg,
                            operand,
                            ..
                        } => match operand {
                            Expr::FieldAccess { object, field, .. } => {
                                match object {
                                    Expr::Identifier { name, .. } => {
                                        assert_eq!(name.to_string(), "a")
                                    }
                                    _ => panic!("expected identifier `a`"),
                                }
                                assert_eq!(field.to_string(), "b");
                            }
                            _ => panic!("expected field access inside unary"),
                        },
                        _ => panic!("expected unary on left"),
                    }
                    // right: c * d
                    match *right {
                        Expr::Binary {
                            op: BinOp::Mul,
                            left,
                            right,
                            ..
                        } => {
                            match left {
                                Expr::Identifier { name, .. } => {
                                    assert_eq!(name.to_string(), "c")
                                }
                                _ => panic!("expected identifier `c`"),
                            }
                            match right {
                                Expr::Identifier { name, .. } => {
                                    assert_eq!(name.to_string(), "d")
                                }
                                _ => panic!("expected identifier `d`"),
                            }
                        }
                        _ => panic!("expected multiplication on right"),
                    }
                }
                _ => panic!("expected binary add"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_match_expression() {
        let arena = Bump::new();
        let result = parse("const x = match o { Some(n) => n, None => 0 };", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Match { arms, .. } => {
                    assert_eq!(arms.len(), 2);
                }
                _ => panic!("expected match expression"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_enum_constructor() {
        let arena = Bump::new();
        let result = parse("const o = Some(5);", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::EnumConstructor {
                    enum_name,
                    case_name,
                    ..
                } => {
                    assert_eq!(enum_name.to_string(), "Option");
                    assert_eq!(case_name.to_string(), "Some");
                }
                _ => panic!("expected enum constructor, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_struct_declaration_and_literal() {
        let arena = Bump::new();
        let result = parse(
            "struct Point { x: number; y: number } const p = Point { x: 1, y: 2 };",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert_eq!(program.statements.len(), 2);
        match &program.statements[1] {
            Stmt::Const { value, .. } => match value {
                Expr::StructLiteral { name, fields, .. } => {
                    assert_eq!(name.to_string(), "Point");
                    assert_eq!(fields.len(), 2);
                }
                _ => panic!("expected struct literal, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_struct_embed() {
        let arena = Bump::new();
        let result = parse(
            "struct Legs {} struct Robot { Legs } const r = Robot { Legs: Legs {} };",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[1] {
            Stmt::Struct { embeds, .. } => {
                assert_eq!(embeds.len(), 1);
                assert_eq!(embeds[0].name, "Legs");
            }
            _ => panic!("expected struct declaration"),
        }
    }

    #[test]
    fn parse_struct_fields_without_commas() {
        let arena = Bump::new();
        let result = parse("struct Point {\n  x: number\n  y: number\n}", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Struct { fields, .. } => assert_eq!(fields.len(), 2),
            _ => panic!("expected struct declaration"),
        }
    }

    #[test]
    fn parse_empty_statement() {
        let arena = Bump::new();
        let result = parse("const x = 1;;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert!(matches!(program.statements[1], Stmt::Empty { .. }));
    }

    #[test]
    fn parse_unary_plus() {
        let arena = Bump::new();
        let result = parse("const x = +5;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Unary { op: UnOp::Plus, .. } => {}
                _ => panic!("expected unary plus, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_struct_optional_field_postfix() {
        let arena = Bump::new();
        let result = parse("struct User { name: string; email: string? }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Struct { fields, .. } => {
                assert_eq!(fields.len(), 2);
                assert!(matches!(fields[1].ty, Type::Option { .. }));
            }
            _ => panic!("expected struct declaration"),
        }
    }

    #[test]
    fn parse_struct_optional_field_prefix() {
        let arena = Bump::new();
        let result = parse("struct User { name: string; email?: string }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Struct { fields, .. } => {
                assert_eq!(fields.len(), 2);
                assert!(!fields[0].optional);
                assert!(fields[1].optional);
                assert!(matches!(fields[1].ty, Type::Option { .. }));
            }
            _ => panic!("expected struct declaration"),
        }
    }

    #[test]
    fn parse_option_none_field_access() {
        let arena = Bump::new();
        let result = parse("const n = Option.None;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::EnumConstructor {
                    enum_name,
                    case_name,
                    ..
                } => {
                    assert_eq!(*enum_name, "Option");
                    assert_eq!(*case_name, "None");
                }
                _ => panic!("expected enum constructor, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_enum_qualified_pattern() {
        let arena = Bump::new();
        let result = parse(
            "enum Color { Red, Green } const c = Color.Red; const out = match (c) { Color.Red => 1, Color.Green => 2 };",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[2] {
            Stmt::Const { value, .. } => match value {
                Expr::Match { arms, .. } => {
                    assert_eq!(arms.len(), 2);
                    assert!(matches!(
                        arms[0].pattern,
                        Pattern::Constructor { name: "Red", .. }
                    ));
                    assert!(matches!(
                        arms[1].pattern,
                        Pattern::Constructor { name: "Green", .. }
                    ));
                }
                _ => panic!("expected match expression"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_user_defined_enum_constructor() {
        let arena = Bump::new();
        let result = parse(
            "enum Color { Red, Green, Blue } const c = Color.Red;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[1] {
            Stmt::Const { value, .. } => match value {
                Expr::EnumConstructor {
                    enum_name,
                    case_name,
                    payload,
                    ..
                } => {
                    assert_eq!(enum_name.to_string(), "Color");
                    assert_eq!(case_name.to_string(), "Red");
                    assert!(payload.is_none());
                }
                _ => panic!("expected enum constructor, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_receiver_method() {
        let arena = Bump::new();
        let result = parse(
            "struct Point { x: number; y: number } fn (p Point) distance(other: Point) number { return 0; }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[1] {
            Stmt::ReceiverMethod {
                receiver_type,
                name,
                params,
                return_type,
                ..
            } => {
                assert_eq!(receiver_type.to_string(), "Point");
                assert_eq!(name.to_string(), "distance");
                assert_eq!(params.len(), 1);
                assert!(return_type.is_some());
            }
            _ => panic!("expected receiver method"),
        }
    }

    #[test]
    fn parse_generic_type_params() {
        // rfd#56 phase 1: user code may declare type parameters, unbounded
        // form. All six declaration kinds that carry the AST field parse
        // `<T>` and keep it in `type_params`.
        let cases: &[(&str, &str)] = &[
            ("fn id<T>(x: T) T { return x; }", "id"),
            ("fn (p Point) distance<T>(x: T) T { return x; }", "distance"),
            ("struct Box<T> { value: T }", "Box"),
            ("enum Maybe<T> { Some(T), Nothing }", "Maybe"),
            ("alias Pair<A, B> = Array<number>", "Pair"),
            ("interface Container<T> { fn get() T }", "Container"),
        ];
        for (source, decl) in cases {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(
                result.errors.is_empty(),
                "{source:?} must parse, got: {:?}",
                result.errors
            );
            let program = result.program.unwrap();
            let params = match &program.statements[0] {
                Stmt::Function { name, type_params, .. } => {
                    assert_eq!(name, decl);
                    *type_params
                }
                Stmt::ReceiverMethod { name, type_params, .. } => {
                    assert_eq!(name, decl);
                    *type_params
                }
                Stmt::Struct { name, type_params, .. } => {
                    assert_eq!(name, decl);
                    *type_params
                }
                Stmt::Enum { name, type_params, .. } => {
                    assert_eq!(name, decl);
                    *type_params
                }
                Stmt::TypeAlias { name, type_params, .. } => {
                    assert_eq!(name, decl);
                    *type_params
                }
                Stmt::Interface { name, type_params, .. } => {
                    assert_eq!(name, decl);
                    *type_params
                }
                other => panic!("expected a declaration, got {other:?}"),
            };
            assert!(!params.is_empty(), "{source:?} keeps its type params");
        }

        // Newtypes have no type-parameter slot (the AST carries none): the
        // rejection stays, with a newtype-specific message.
        let arena = Bump::new();
        let result = parse("type Pair<T> number", &arena);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("newtype declarations cannot declare type parameters")),
            "newtype generics stay rejected, got: {:?}",
            result.errors
        );

        // Non-generic declarations keep an empty type_params field.
        let arena = Bump::new();
        let result = parse("fn id(x: number) number { return x; }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function { type_params, .. } => assert!(type_params.is_empty()),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn parse_generic_receiver_method() {
        // dsc#101: the receiver binds the type parameter —
        // `fn (s Signal<T>) get() T` — instead of the method declaring it.
        let arena = Bump::new();
        let result = parse(
            "struct Signal<T> { value: T } fn (s Signal<T>) get() T { return s.value }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[1] {
            Stmt::ReceiverMethod {
                receiver_type,
                receiver_type_args,
                name,
                type_params,
                ..
            } => {
                assert_eq!(receiver_type.to_string(), "Signal");
                assert_eq!(receiver_type_args.len(), 1);
                assert_eq!(receiver_type_args[0].name, "T");
                assert!(receiver_type_args[0].bound.is_none());
                assert!(type_params.is_empty());
                assert_eq!(name.to_string(), "get");
            }
            _ => panic!("expected receiver method"),
        }

        // A receiver parameter may carry a bound (rfd#56 phase 2), parsed
        // with the declaration type-param grammar.
        let arena = Bump::new();
        let result = parse(
            "interface Named { name: string }\n\
             struct Holder<T> { value: T }\n\
             fn (x Holder<T: Named>) name() string { return x.value.name }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[2] {
            Stmt::ReceiverMethod {
                receiver_type_args, ..
            } => {
                assert_eq!(receiver_type_args.len(), 1);
                assert_eq!(receiver_type_args[0].name, "T");
                assert!(receiver_type_args[0].bound.is_some());
            }
            _ => panic!("expected receiver method"),
        }

        // A mutating receiver binds the same way.
        let arena = Bump::new();
        let result = parse(
            "struct Signal<T> { value: T } fn (s mut Signal<T>) set(next: T) { s.value = next }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[1] {
            Stmt::ReceiverMethod {
                receiver_type_args,
                receiver_mutable,
                ..
            } => {
                assert_eq!(receiver_type_args.len(), 1);
                assert!(*receiver_mutable);
            }
            _ => panic!("expected receiver method"),
        }

        // A receiver without type arguments keeps an empty field.
        let arena = Bump::new();
        let result = parse(
            "struct Point { x: number } fn (p Point) dist() number { return p.x }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[1] {
            Stmt::ReceiverMethod {
                receiver_type_args, ..
            } => assert!(receiver_type_args.is_empty()),
            _ => panic!("expected receiver method"),
        }
    }

    #[test]
    fn parse_struct_literal_type_args_rejected() {
        // dsc#101: `Signal<T> { ... }` puts explicit type arguments at a
        // construction site, which the design infers (rfd#56). The old
        // behavior parsed `<`/`>` as comparisons and reported `unknown
        // identifier T`; now the parse rejects the spelling directly.
        for source in [
            "struct Signal<T> { value: T }\nconst s = Signal<T> { value: 1 }",
            "struct Signal<T> { value: T }\nconst s = Signal<number> { value: 1 }",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(
                result
                    .errors
                    .iter()
                    .any(|e| e.message.contains("infers its type arguments from the field values")),
                "{source:?} got: {:?}",
                result.errors
            );
        }
    }

    #[test]
    fn parse_type_param_bounds() {
        // rfd#56 phase 2: `<T: Bound>` where Bound is any type expression
        // already writable — an interface, a union, or a concrete type. One
        // rule, not two mechanisms.
        let arena = Bump::new();
        let result = parse(
            "fn greet<T: Named>(x: T) T { return x; }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function { type_params, .. } => {
                assert_eq!(type_params.len(), 1);
                assert_eq!(type_params[0].name, "T");
                match type_params[0].bound.as_ref() {
                    Some(Type::Named { name, .. }) => assert_eq!(name, &"Named"),
                    other => panic!("expected interface bound, got {other:?}"),
                }
            }
            _ => panic!("expected function"),
        }

        // Union bound: `A | B | C` parses as one bound expression.
        let arena = Bump::new();
        let result = parse(
            "fn render<T: Product | Bundle | GiftCard>(item: T) T { return item; }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function { type_params, .. } => match type_params[0].bound.as_ref() {
                Some(Type::Union { members, .. }) => {
                    let names: Vec<_> = members
                        .iter()
                        .map(|m| match m {
                            Type::Named { name, .. } => *name,
                            other => panic!("expected named member, got {other:?}"),
                        })
                        .collect();
                    assert_eq!(names, ["Product", "Bundle", "GiftCard"]);
                }
                other => panic!("expected union bound, got {other:?}"),
            },
            _ => panic!("expected function"),
        }

        // A bound does not make the parameter mandatory: mixed lists, and the
        // unbounded form, keep `bound: None`.
        let arena = Bump::new();
        let result = parse("fn pair<T: Named, U>(a: T, b: U) U { return b; }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function { type_params, .. } => {
                assert!(type_params[0].bound.is_some());
                assert!(type_params[1].bound.is_none());
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn parse_import_named() {
        let arena = Bump::new();
        let result = parse("import { add } from \"./math.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import {
                specifiers, source, ..
            } => {
                assert_eq!(source, &"./math.ds");
                assert_eq!(specifiers.len(), 1);
                assert_eq!(specifiers[0].imported, "add");
                assert_eq!(specifiers[0].local, "add");
            }
            _ => panic!("expected import"),
        }
    }

    #[test]
    fn parse_import_aliased() {
        let arena = Bump::new();
        let result = parse("import { add as plus } from \"./math.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import { specifiers, .. } => {
                assert_eq!(specifiers.len(), 1);
                assert_eq!(specifiers[0].imported, "add");
                assert_eq!(specifiers[0].local, "plus");
            }
            _ => panic!("expected import"),
        }
    }

    #[test]
    fn parse_import_type_whole_statement() {
        // rfd#12 ESM alignment amendment (dsc#281): `import type { … }` marks
        // every specifier type-only.
        let arena = Bump::new();
        let result = parse("import type { Foo, Bar } from \"./types.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import {
                specifiers, source, ..
            } => {
                assert_eq!(source, &"./types.ds");
                assert_eq!(specifiers.len(), 2);
                assert!(specifiers.iter().all(|s| s.is_type_only));
            }
            _ => panic!("expected import"),
        }
    }

    #[test]
    fn parse_import_type_inline_mixed() {
        // The inline form marks only the flagged specifier type-only, so a
        // mixed statement keeps its value import (dsc#281).
        let arena = Bump::new();
        let result = parse("import { type Foo, bar } from \"./mixed.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import { specifiers, .. } => {
                assert_eq!(specifiers.len(), 2);
                assert_eq!(specifiers[0].imported, "Foo");
                assert!(specifiers[0].is_type_only);
                assert_eq!(specifiers[1].imported, "bar");
                assert!(!specifiers[1].is_type_only);
            }
            _ => panic!("expected import"),
        }
    }

    #[test]
    fn parse_import_side_effect() {
        let arena = Bump::new();
        let result = parse("import \"./side-effects.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import {
                specifiers, source, ..
            } => {
                assert!(specifiers.is_empty());
                assert_eq!(source, &"./side-effects.ds");
            }
            _ => panic!("expected import"),
        }
    }

    #[test]
    fn parse_export_const() {
        let arena = Bump::new();
        let result = parse("export const x: number = 42;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Export { decl, .. } => match decl {
                crate::ast::ExportDecl::Const { name, ty, .. } => {
                    assert_eq!(name, &"x");
                    assert!(ty.is_some());
                }
                _ => panic!("expected const export"),
            },
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn parse_export_function() {
        let arena = Bump::new();
        let result = parse(
            "export fn add(a: number, b: number) number { return a + b; }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Export { decl, .. } => match decl {
                crate::ast::ExportDecl::Function { name, params, .. } => {
                    assert_eq!(name, &"add");
                    assert_eq!(params.len(), 2);
                }
                _ => panic!("expected function export"),
            },
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn parse_export_default_function() {
        // rfd#12 ESM alignment amendment (dsc#280): a default export of a
        // named function declaration keeps its own local name, exported
        // under the sentinel key `"default"`.
        let arena = Bump::new();
        let result = parse("export default fn Page() string { return \"hi\"; }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Export { decl, .. } => match decl {
                crate::ast::ExportDecl::Function {
                    name, is_default, ..
                } => {
                    assert_eq!(name, &"Page");
                    assert!(is_default);
                }
                _ => panic!("expected default function export"),
            },
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn parse_export_default_async_function() {
        let arena = Bump::new();
        let result = parse(
            "export default async fn Page() string { return \"hi\"; }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Export { decl, .. } => match decl {
                crate::ast::ExportDecl::Function {
                    is_async,
                    is_default,
                    ..
                } => {
                    assert!(is_async);
                    assert!(is_default);
                }
                _ => panic!("expected default function export"),
            },
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn parse_export_default_named_binding() {
        // `export default app` desugars to `export { app as default }`
        // (rfd#12 ESM alignment amendment): it is a named binding, not a
        // new AST shape.
        let arena = Bump::new();
        let result = parse("const app = 1; export default app;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[1] {
            Stmt::Export { decl, .. } => match decl {
                crate::ast::ExportDecl::NamedGroup { names, source } => {
                    assert_eq!(names.len(), 1);
                    assert_eq!(names[0].name, "app");
                    assert_eq!(names[0].alias, Some("default"));
                    assert!(source.is_none());
                }
                _ => panic!("expected named-group export"),
            },
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn parse_export_default_anonymous_function_rejected() {
        let arena = Bump::new();
        let result = parse("export default fn () { return 1; }", &arena);
        assert!(
            result.errors.iter().any(|e| e.message.contains("name")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn parse_export_default_object_literal_rejected() {
        let arena = Bump::new();
        let result = parse("export default { a: 1 };", &arena);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("named declaration") || e.message.contains("named binding")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn parse_export_reexport_default_as_named() {
        // `export { default as json } from "./json"` (rfd#12 ESM alignment
        // amendment item 4) parses through the existing named-group grammar:
        // `default` is a plain identifier token, not a keyword.
        let arena = Bump::new();
        let result = parse("export { default as json } from \"./json\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Export { decl, .. } => match decl {
                crate::ast::ExportDecl::NamedGroup { names, source } => {
                    assert_eq!(names[0].name, "default");
                    assert_eq!(names[0].alias, Some("json"));
                    assert_eq!(source, &Some("./json"));
                }
                _ => panic!("expected named-group export"),
            },
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn parse_import_default() {
        let arena = Bump::new();
        let result = parse("import Page from \"./page.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import {
                specifiers, source, ..
            } => {
                assert_eq!(source, &"./page.ds");
                assert_eq!(specifiers.len(), 1);
                assert_eq!(specifiers[0].imported, "default");
                assert_eq!(specifiers[0].local, "Page");
            }
            _ => panic!("expected import"),
        }
    }

    #[test]
    fn parse_import_default_mixed() {
        let arena = Bump::new();
        let result = parse("import Page, { helper } from \"./page.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import { specifiers, .. } => {
                assert_eq!(specifiers.len(), 2);
                assert_eq!(specifiers[0].imported, "default");
                assert_eq!(specifiers[0].local, "Page");
                assert_eq!(specifiers[1].imported, "helper");
                assert_eq!(specifiers[1].local, "helper");
            }
            _ => panic!("expected import"),
        }
    }

    #[test]
    fn parse_export_named_group() {
        let arena = Bump::new();
        let result = parse("const answer = 42; export { answer };", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[1] {
            Stmt::Export { decl, .. } => match decl {
                crate::ast::ExportDecl::NamedGroup { names, .. } => {
                    assert_eq!(names.len(), 1);
                    assert_eq!(names[0].name, "answer");
                    assert!(names[0].alias.is_none());
                }
                _ => panic!("expected named-group export"),
            },
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn parse_export_default_non_binding_expression_rejected() {
        // Default exports are now allowed (rfd#12 ESM alignment amendment,
        // dsc#280), but only as a named declaration or a named binding — a
        // bare literal is neither.
        let arena = Bump::new();
        let result = parse("export default 42;", &arena);
        assert!(!result.errors.is_empty());
        assert!(
            result.errors.iter().any(|e| e.message.contains("named")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn parse_array_literal() {
        let arena = Bump::new();
        let result = parse("const a = [1, 2, 3];", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Array { elements, .. } => {
                    assert_eq!(elements.len(), 3);
                }
                _ => panic!("expected array literal"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_array_spread() {
        let arena = Bump::new();
        let result = parse("const a = [...b];", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Array { elements, .. } => {
                    assert_eq!(elements.len(), 1);
                    assert!(matches!(elements[0], Expr::Spread { .. }));
                }
                _ => panic!("expected array literal"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_mixed_keyed_unkeyed_array_rejected() {
        let arena = Bump::new();
        let result = parse("const x = [\"a\", 1: \"b\"];", &arena);
        assert!(!result.errors.is_empty(), "expected a parse error");
        assert!(
            result.errors.iter().any(|e| {
                e.message.contains("expected") && e.message.contains("]") && e.message.contains(":")
            }),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn parse_object_literal() {
        let arena = Bump::new();
        let result = parse("const o = { a: 1, b: \"two\" };", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Object { fields, .. } => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].key, "a");
                    assert_eq!(fields[1].key, "b");
                }
                _ => panic!("expected object literal"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_object_spread() {
        let arena = Bump::new();
        let result = parse("const o = { ...base, x: 1 };", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Object { fields, .. } => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].key, "");
                    assert_eq!(fields[1].key, "x");
                }
                _ => panic!("expected object literal"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_index_access() {
        let arena = Bump::new();
        let result = parse("const x = arr[0];", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::IndexAccess { object, index, .. } => {
                    assert!(matches!(object, Expr::Identifier { name, .. } if name == &"arr"));
                    assert!(matches!(index, Expr::Number { value, .. } if *value == 0.0));
                }
                _ => panic!("expected index access"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_await() {
        let arena = Bump::new();
        let result = parse("const x = await fetch();", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Await { expr, .. } => {
                    assert!(matches!(expr, Expr::Call { .. }));
                }
                _ => panic!("expected await expression"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_pipe() {
        let arena = Bump::new();
        let result = parse("const y = x |> double;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Binary {
                    op: BinOp::Pipe,
                    left,
                    right,
                    ..
                } => {
                    assert!(matches!(left, Expr::Identifier { name, .. } if name == &"x"));
                    assert!(matches!(right, Expr::Identifier { name, .. } if name == &"double"));
                }
                _ => panic!("expected pipe expression, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_unsafe_expression() {
        // Intentional unsafe fixture (RFD 21): raw-JS parsing and nested braces.
        let arena = Bump::new();
        let result = parse("const r = unsafe { JSON.parse('{}') };", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Unsafe { source, .. } => {
                    assert_eq!(source.trim(), "JSON.parse('{}')");
                }
                _ => panic!("expected unsafe expression, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_unsafe_block_with_nested_braces() {
        // Intentional unsafe fixture (RFD 21): raw-JS parsing and nested braces.
        let arena = Bump::new();
        let result = parse(
            "const r = unsafe { function f() { return 1; } f() };",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Unsafe { source, .. } => {
                    assert!(source.contains("function f()"));
                    assert!(source.contains("f()"));
                }
                _ => panic!("expected unsafe expression, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_element() {
        let arena = Bump::new();
        let result = parse("const el = <div class=\"box\" />;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.tag, "div");
                    assert_eq!(element.attributes.len(), 1);
                    assert_eq!(element.attributes[0].name, "class");
                }
                _ => panic!("expected jsx element, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_namespaced_attribute() {
        let arena = Bump::new();
        let result = parse("const el = <Cart client:load userId={id} />;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.tag, "Cart");
                    let names: Vec<&str> = element.attributes.iter().map(|a| a.name).collect();
                    assert!(names.contains(&"client:load"), "got {names:?}");
                    assert!(names.contains(&"userId"), "got {names:?}");
                }
                _ => panic!("expected jsx element, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_hyphenated_attributes() {
        // dsc#69: every valid HTML attribute name must be writable, not just
        // data-*/aria-*. `data-count` exercises `{a - b}` as an attribute
        // value: the subtraction must survive next to the joined name.
        let arena = Bump::new();
        let result = parse(
            "const el = <p data-x=\"1\" aria-label=\"y\" http-equiv=\"z\" data-count={a - b}>t</p>;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.tag, "p");
                    let names: Vec<&str> = element.attributes.iter().map(|a| a.name).collect();
                    assert_eq!(
                        names,
                        vec!["data-x", "aria-label", "http-equiv", "data-count"]
                    );
                    match &element.attributes[3].value {
                        Some(Expr::Binary { op, .. }) => {
                            assert_eq!(*op, BinOp::Sub);
                        }
                        other => panic!("expected subtraction in attribute value, got {:?}", other),
                    }
                }
                _ => panic!("expected jsx element, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_subtraction_untouched_outside_attribute_names() {
        // dsc#69: joining `-` in attribute-name position must not leak into
        // ordinary code — `a - b` and `x - 1` stay subtraction.
        let arena = Bump::new();
        let result = parse("const n = a - b; const m = x - 1;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        for stmt in program.statements.iter() {
            match stmt {
                Stmt::Const { value, .. } => match value {
                    Expr::Binary { op, .. } => assert_eq!(*op, BinOp::Sub),
                    other => panic!("expected subtraction, got {:?}", other),
                },
                other => panic!("expected const declaration, got {:?}", other),
            }
        }
    }

    #[test]
    fn parse_jsx_keyword_attributes() {
        // dsc#77: any keyword is legal in JSX attribute-name position —
        // attribute-name position is not statement/expression position, so a
        // keyword there is just a name. `type`/`for`/`class` are the common
        // form attributes; the rest of the keyword list is swept in
        // parse_jsx_keyword_attribute_sweep.
        let arena = Bump::new();
        let result = parse(
            "const el = <form><label for=\"email\">Email</label><input type=\"text\" id=\"email\" /><button type=\"submit\">Send</button></form>;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.tag, "form");
                    let form = element;
                    match &form.children[0] {
                        Expr::JsxElement { element, .. } => {
                            assert_eq!(element.tag, "label");
                            assert_eq!(element.attributes[0].name, "for");
                        }
                        other => panic!("expected label element, got {:?}", other),
                    }
                    match &form.children[1] {
                        Expr::JsxElement { element, .. } => {
                            assert_eq!(element.tag, "input");
                            let names: Vec<&str> =
                                element.attributes.iter().map(|a| a.name).collect();
                            assert_eq!(names, vec!["type", "id"]);
                        }
                        other => panic!("expected input element, got {:?}", other),
                    }
                    match &form.children[2] {
                        Expr::JsxElement { element, .. } => {
                            assert_eq!(element.tag, "button");
                            assert_eq!(element.attributes[0].name, "type");
                        }
                        other => panic!("expected button element, got {:?}", other),
                    }
                }
                _ => panic!("expected jsx element, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_keyword_attribute_sweep() {
        // dsc#77: the general rule, not an enumeration — every keyword the
        // lexer emits must be writable as an attribute name, alone and as a
        // hyphen continuation. `super` is included: its reservation only
        // applies where a declaration could start.
        let arena = Bump::new();
        let result = parse(
            "const el = <div if=\"1\" else=\"2\" for=\"3\" of=\"4\" return=\"5\" match=\"6\" unsafe=\"7\" super=\"8\" async=\"9\" await=\"10\" break=\"11\" continue=\"12\" fn=\"13\" let=\"14\" const=\"15\" mut=\"16\" function=\"17\" struct=\"18\" enum=\"19\" interface=\"20\" type=\"21\" alias=\"22\" import=\"23\" export=\"24\" from=\"25\" as=\"26\" pub=\"27\" build=\"28\" bridge=\"29\" true=\"30\" false=\"31\" None=\"32\" if-led=\"33\" />;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    let names: Vec<&str> = element.attributes.iter().map(|a| a.name).collect();
                    assert_eq!(
                        names,
                        vec![
                            "if", "else", "for", "of", "return", "match", "unsafe", "super",
                            "async", "await", "break", "continue", "fn", "let", "const", "mut",
                            "function", "struct", "enum", "interface", "type", "alias",
                            "import", "export", "from", "as", "pub", "build", "bridge", "true",
                            "false", "None", "if-led"
                        ]
                    );
                }
                _ => panic!("expected jsx element, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_unsafe_boolean_attribute() {
        // dsc#77: `unsafe` as a boolean attribute must not trip the lexer's
        // pending-brace machinery (`unsafe { ... }` raw-JS blocks), and an
        // `unsafe={...}` value must lex as an ordinary expression, with code
        // after the element unaffected.
        let arena = Bump::new();
        let result = parse(
            "const el = <div unsafe unsafe={v} />; const n = a - b;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.attributes[0].name, "unsafe");
                    assert_eq!(element.attributes[1].name, "unsafe");
                }
                _ => panic!("expected jsx element, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
        match &program.statements[1] {
            Stmt::Const { value, .. } => match value {
                Expr::Binary { op, .. } => assert_eq!(*op, BinOp::Sub),
                other => panic!("expected subtraction after element, got {:?}", other),
            },
            other => panic!("expected const declaration, got {:?}", other),
        }
    }

    #[test]
    fn parse_keywords_untouched_outside_jsx_attribute_names() {
        // dsc#77 regression pin: outside attribute-name position, `for`,
        // `if`/`else`, and `type` remain hard keywords with their usual
        // statement meaning.
        let arena = Bump::new();
        let result = parse(
            "type Email string; for (const x of xs) { if (x) { break } else { continue } }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert!(matches!(program.statements[0], Stmt::Newtype { .. }));
        assert!(matches!(program.statements[1], Stmt::ForOf { .. }));
    }

    #[test]
    fn parse_jsx_with_children() {
        let arena = Bump::new();
        let result = parse("const el = <p>hello {name}</p>;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.tag, "p");
                    assert_eq!(element.children.len(), 2);
                    assert!(matches!(element.children[0], Expr::JsxText { .. }));
                    assert!(matches!(element.children[1], Expr::Identifier { .. }));
                }
                _ => panic!("expected jsx element, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_fragment() {
        let arena = Bump::new();
        let result = parse("const el = <><span>a</span><span>b</span></>;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxFragment { children, .. } => {
                    assert_eq!(children.len(), 2);
                }
                _ => panic!("expected jsx fragment, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_member_expression_rejected() {
        let arena = Bump::new();
        let result = parse("const el = <My.Component />;", &arena);
        assert!(
            result.program.is_none(),
            "member expression should fail to parse"
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("member expressions")),
            "expected member expression error, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn parse_jsx_context_provider_member() {
        // rfd#64 lane C: `<Ctx.Provider>` is the one RFD 8 exception.
        let arena = Bump::new();
        let result = parse(
            "const el = <LocaleContext.Provider value={en}>x</LocaleContext.Provider>;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.tag, "LocaleContext.Provider");
                    assert_eq!(element.context_provider(), Some("LocaleContext"));
                    assert_eq!(element.attributes[0].name, "value");
                }
                other => panic!("expected jsx element, got {other:?}"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_react_fragment_member() {
        // dsc#246: `<React.Fragment>` is the other RFD 8 exception, so a
        // keyed fragment (a list of grouped children) has an expression --
        // the bare `<>...</>` shorthand cannot take a `key`.
        let arena = Bump::new();
        let result = parse(
            "const el = <React.Fragment key={id}>x</React.Fragment>;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.tag, "React.Fragment");
                    assert_eq!(element.attributes[0].name, "key");
                }
                other => panic!("expected jsx element, got {other:?}"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_jsx_other_member_expressions_still_rejected() {
        // The narrow exceptions are `.Provider` and `React.Fragment` only --
        // every other member tag, including `Foo.Fragment`, stays rejected
        // under the existing RFD 8 diagnostic.
        let arena = Bump::new();
        for source in [
            "const el = <Foo.Bar />;",
            "const el = <Foo.Fragment />;",
            "const el = <React.Other />;",
        ] {
            let result = parse(source, &arena);
            assert!(result.program.is_none(), "{source} should fail to parse");
            assert!(
                result
                    .errors
                    .iter()
                    .any(|e| e.message.contains("member expressions")),
                "{source}: expected member expression error, got: {:?}",
                result.errors
            );
        }
    }

    #[test]
    fn parse_return_single_line_jsx_needs_no_parens() {
        let arena = Bump::new();
        let result = parse("export fn P() ReactNode { return <div>x</div> }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn parse_return_parenthesized_multiline_jsx_ok() {
        let arena = Bump::new();
        let result = parse(
            "export fn P() ReactNode {\n  return (\n    <div>\n      <span>x</span>\n    </div>\n  )\n}",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    #[test]
    fn parse_return_bare_multiline_jsx_rejected_with_named_diagnostic() {
        // dsc#245: the error must name the rule ("a multi-line JSX return
        // must be wrapped in parentheses"), not a symptom-level message like
        // "expected expression" (see dsc#243 for why that costs a developer
        // their first hour).
        let arena = Bump::new();
        let result = parse(
            "export fn P() ReactNode {\n  return <div>\n    <span>x</span>\n  </div>\n}",
            &arena,
        );
        assert!(
            result.program.is_none() || !result.errors.is_empty(),
            "bare multi-line JSX return must be rejected"
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message == "a multi-line JSX return must be wrapped in parentheses"),
            "expected the named-rule diagnostic, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn parse_return_bare_multiline_jsx_fragment_also_rejected() {
        // The rule covers `Expr::JsxFragment`, not only `Expr::JsxElement`.
        let arena = Bump::new();
        let result = parse(
            "export fn P() ReactNode {\n  return <>\n    <span>x</span>\n  </>\n}",
            &arena,
        );
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message == "a multi-line JSX return must be wrapped in parentheses"),
            "expected the named-rule diagnostic, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn parse_jsx_text_is_verbatim_multi_script() {
        // deka#67 / deka#68: JSX text is raw text, not code. Every script,
        // emoji, combining mark, and code-lexing trigger character must
        // survive byte-for-byte in the JsxText child.
        let text = "Café Yirgacheffe — 18,50 € · don't miss it: $18.50 / £15, #1 @ 100% `tick` ~ \\
日本語 · Ελληνικά · Русский · العربية · የቡና ☕ 🎉 café";
        let source = format!("const el = <p>{text}</p>;");
        let arena = Bump::new();
        let result = parse(&source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(element.children.len(), 1);
                    match &element.children[0] {
                        Expr::JsxText { value, .. } => assert_eq!(value.to_string(), text),
                        other => panic!("expected JsxText, got {:?}", other),
                    }
                }
                other => panic!("expected jsx element, got {:?}", other),
            },
            other => panic!("expected const declaration, got {:?}", other),
        }
    }

    #[test]
    fn parse_jsx_text_never_breaks_into_code_tokens() {
        // Characters that are string openers or lex errors in code must not
        // split a JSX text run (deka#67).
        let arena = Bump::new();
        let result = parse(
            "const el = <p>it's a `test` of $ & % # @ ~ \"quotes\"</p>;",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::JsxElement { element, .. } => {
                    assert_eq!(
                        element.children.len(),
                        1,
                        "text must stay a single verbatim child"
                    );
                }
                other => panic!("expected jsx element, got {:?}", other),
            },
            other => panic!("expected const declaration, got {:?}", other),
        }
    }

    #[test]
    fn parse_jsx_unterminated_reports_line_and_column() {
        // Malformed JSX must produce positioned diagnostics, never panic.
        for source in [
            "const el = <div>\n  text\n",
            "const el = <div>{x</div>;",
            "const el = <div",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(
                !result.errors.is_empty(),
                "expected diagnostics for {:?}",
                source
            );
            for e in &result.errors {
                assert!(
                    e.line >= 1 && e.column >= 1,
                    "diagnostic must carry a line and column: {:?}",
                    e
                );
            }
        }
    }

    #[test]
    fn parse_template_literal() {
        // dsc#89: `${...}` must parse as a DekaScript expression part, not
        // text. This assertion used to pin the bug ( `${x}` as text).
        let arena = Bump::new();
        let result = parse("const s = `hello ${x}`;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::TemplateLiteral { parts, .. } => {
                    assert_eq!(parts.len(), 3);
                    assert!(matches!(parts[0], TemplatePart::Text(text) if text == "hello "));
                    assert!(
                        matches!(parts[1], TemplatePart::Expr(Expr::Identifier { name, .. }) if *name == "x"),
                        "interpolation must be an expression part, got {:?}",
                        parts[1]
                    );
                    assert!(matches!(parts[2], TemplatePart::Text(text) if text.is_empty()));
                }
                _ => panic!("expected template literal, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_template_literal_adjacent_and_multi() {
        let arena = Bump::new();
        let result = parse("const s = `${a} and ${b}!`;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::TemplateLiteral { parts, .. } => {
                    assert_eq!(parts.len(), 5);
                    assert!(matches!(parts[0], TemplatePart::Text(text) if text.is_empty()));
                    assert!(matches!(parts[1], TemplatePart::Expr(Expr::Identifier { name, .. }) if *name == "a"));
                    assert!(matches!(parts[2], TemplatePart::Text(text) if text == " and "));
                    assert!(matches!(parts[3], TemplatePart::Expr(Expr::Identifier { name, .. }) if *name == "b"));
                    assert!(matches!(parts[4], TemplatePart::Text(text) if text == "!"));
                }
                _ => panic!("expected template literal, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_template_literal_nested_and_escaped() {
        // Nested template inside an interpolation, an escaped `\${` (literal
        // text, no interpolation), a backtick string inside an interpolation,
        // and a brace inside a string inside the interpolation (dsc#89
        // done-when matrix).
        let arena = Bump::new();
        let result = parse(
            r#"const s = `${`inner ${x}`} \${literal} ${`deep ${y}`} ${"}"}`;"#,
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::TemplateLiteral { parts, .. } => {
                    let exprs: Vec<&Expr> = parts
                        .iter()
                        .filter_map(|p| match p {
                            TemplatePart::Expr(e) => Some(*e),
                            TemplatePart::Text(_) => None,
                        })
                        .collect();
                    assert_eq!(exprs.len(), 3, "parts: {:?}", parts);
                    // First interpolation is the nested template.
                    match exprs[0] {
                        Expr::TemplateLiteral { parts, .. } => {
                            assert!(matches!(parts[0], TemplatePart::Text(t) if t == "inner "));
                            assert!(
                                matches!(parts[1], TemplatePart::Expr(Expr::Identifier { name, .. }) if *name == "x")
                            );
                        }
                        other => panic!("expected nested template, got {:?}", other),
                    }
                    // `\${literal}` stays text — the backslash-escaped dollar
                    // must not open an interpolation.
                    assert!(
                        parts.iter().any(
                            |p| matches!(p, TemplatePart::Text(t) if t.contains("\\${literal}"))
                        ),
                        "escaped dollar must stay literal text, parts: {:?}",
                        parts
                    );
                    // Third interpolation: another nested template with its
                    // own interpolation (a backtick string inside `${}`).
                    match exprs[1] {
                        Expr::TemplateLiteral { parts, .. } => {
                            assert!(matches!(parts[0], TemplatePart::Text(t) if t == "deep "));
                            assert!(
                                matches!(parts[1], TemplatePart::Expr(Expr::Identifier { name, .. }) if *name == "y")
                            );
                        }
                        other => panic!("expected nested template, got {:?}", other),
                    }
                    // Fourth interpolation: a string literal containing `}`.
                    assert!(matches!(exprs[2], Expr::String { value, .. } if *value == "}"));
                }
                _ => panic!("expected template literal, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_template_literal_braces_and_calls() {
        let arena = Bump::new();
        let result = parse("const s = `sum: ${add(1, 2) { }`;", &arena);
        assert!(!result.errors.is_empty(), "unbalanced brace must fail");
    }

    #[test]
    fn parse_template_literal_unterminated_interpolation() {
        for source in ["const s = `hello ${x`;", "const s = `hello ${x"] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(
                !result.errors.is_empty(),
                "expected diagnostics for {:?}",
                source
            );
        }
    }

    // ------------------------------------------------------------------
    // Optional semicolon insertion
    // ------------------------------------------------------------------

    #[test]
    fn parse_optional_semicolon_top_level() {
        let arena = Bump::new();
        let result = parse("const x = 1\nconst y = 2\n", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert_eq!(program.statements.len(), 2);
    }

    #[test]
    fn parse_optional_semicolon_in_block() {
        let arena = Bump::new();
        let result = parse(
            "fn add(a: number, b: number) number {\n  const c = a + b\n  return c\n}",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function { body, .. } => {
                assert_eq!(body.len(), 2);
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn parse_optional_semicolon_before_closing_brace() {
        let arena = Bump::new();
        let result = parse("fn one() number { return 1 }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function { body, .. } => {
                assert_eq!(body.len(), 1);
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn parse_multiline_expression_does_not_terminate_early() {
        let arena = Bump::new();
        let result = parse("const x = 1 +\n  2 +\n  3\n", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Binary {
                    op, left, right, ..
                } => {
                    assert!(matches!(op, BinOp::Add));
                    // (1 + 2) + 3
                    match left {
                        Expr::Binary { op: inner_op, .. } => {
                            assert!(matches!(inner_op, BinOp::Add));
                        }
                        _ => panic!("expected nested binary on left"),
                    }
                    match right {
                        Expr::Number { value, .. } => assert_eq!(*value, 3.0),
                        _ => panic!("expected 3 on right"),
                    }
                }
                _ => panic!("expected binary expression, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_multiline_function_signature() {
        let arena = Bump::new();
        let result = parse(
            "fn add(\n  a: number,\n  b: number\n) number {\n  return a + b\n}",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function { params, .. } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].binding.to_string(), "a");
                assert_eq!(params[1].binding.to_string(), "b");
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn parse_multiline_type_arguments() {
        let arena = Bump::new();
        let result = parse("const m: Map<\n  string,\n  number\n> = None\n", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { ty, .. } => match ty {
                Some(Type::Generic { base, args, .. }) => {
                    assert_eq!(base.to_string(), "Map");
                    assert_eq!(args.len(), 2);
                }
                _ => panic!("expected generic type"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_semicolon_still_required_inline() {
        let arena = Bump::new();
        let result = parse("const x = 1 const y = 2", &arena);
        assert!(
            result.program.is_none(),
            "expected parse failure without newline or semicolon"
        );
    }

    #[test]
    fn parse_fn_expression_literal() {
        let arena = Bump::new();
        let result = parse(
            "const double = fn (x: number) number { return x * 2 }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Function {
                    params,
                    return_type,
                    body,
                    ..
                } => {
                    assert_eq!(params.len(), 1);
                    assert_eq!(params[0].binding.to_string(), "x");
                    assert!(return_type.is_some());
                    assert_eq!(body.len(), 1);
                }
                _ => panic!("expected function expression, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn parse_for_loop() {
        let arena = Bump::new();
        let result = parse("for (let i = 0; i < 10; i = i + 1) { break }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                assert!(init.is_some());
                assert!(condition.is_some());
                assert!(step.is_some());
                assert_eq!(body.len(), 1);
                assert!(matches!(body[0], Stmt::Break { .. }));
            }
            _ => panic!("expected for loop"),
        }
    }

    #[test]
    fn parse_async_function() {
        let arena = Bump::new();
        let result = parse("async fn value() Promise<number> { return 1 }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Function { is_async, .. } => assert!(*is_async),
            _ => panic!("expected async function"),
        }
    }

    #[test]
    fn super_rejected_everywhere() {
        // `super` is a hard keyword, now available only on `struct` and
        // `enum` declarations (rfd#41, deka#561 PR B); every other use is
        // still rejected.
        for source in [
            "super const x = 1",
            "super(x)",
            "const x = super",
            "super fn f<T>(x: T) T { return x }",
            "super interface I { m: number }",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(
                result.errors.iter().any(|e| e.message.contains("super")),
                "{source:?} must be rejected, got: {:?}",
                result.errors
            );
        }

        // `super struct` / `super enum` parse and carry the mark.
        let arena = Bump::new();
        let result = parse("super struct S { x: number }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        match &result.program.expect("program").statements[0] {
            Stmt::Struct { is_super, .. } => assert!(*is_super),
            other => panic!("expected super struct, got {other:?}"),
        }
        let arena = Bump::new();
        let result = parse("super enum E { A }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        match &result.program.expect("program").statements[0] {
            Stmt::Enum { is_super, .. } => assert!(*is_super),
            other => panic!("expected super enum, got {other:?}"),
        }

        // After `export` the reserved-word diagnostic names the expected
        // export forms instead; either way `super` does not parse.
        let arena = Bump::new();
        let result = parse("export super fn f<T>(x: T) T { return x }", &arena);
        assert!(
            result.errors.iter().any(|e| e.message.contains("super")),
            "export super fn must be rejected, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn super_rejected_as_property() {
        let arena = Bump::new();
        let result = parse("const x = obj.super", &arena);
        assert!(!result.errors.is_empty(), "x.super must not parse");
    }

    #[test]
    fn parse_async_fn_expression() {
        let arena = Bump::new();
        let result = parse("const f = async fn () Promise<number> { return 1 }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Function { is_async, .. } => assert!(*is_async),
                _ => panic!("expected async function expression"),
            },
            _ => panic!("expected const declaration"),
        }
    }

    #[test]
    fn top_level_await_detected() {
        let arena = Bump::new();
        let result = parse(
            "async fn main() Promise<number> { return 1 } const n = await main();",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert!(program.has_top_level_await);
    }

    #[test]
    fn await_inside_function_is_not_top_level() {
        let arena = Bump::new();
        let result = parse(
            "async fn main() Promise<number> { return await other() }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert!(!program.has_top_level_await);
    }

    #[test]
    fn await_inside_closure_is_not_top_level() {
        let arena = Bump::new();
        let result = parse(
            "const f = fn () Promise<number> { return await other() }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert!(!program.has_top_level_await);
    }

    #[test]
    fn await_inside_top_level_for_loop_is_top_level() {
        let arena = Bump::new();
        let result = parse(
            "async fn work() Promise<number> { return 1 } for (let i = 0; i < 3; i = i + 1) { await work() }",
            &arena,
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert!(program.has_top_level_await);
    }

    #[test]
    fn no_await_means_no_top_level_await() {
        let arena = Bump::new();
        let result = parse("const x = 1; fn f() number { return x }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert!(!program.has_top_level_await);
    }

    #[test]
    fn juxtaposition_call_with_string() {
        let arena = Bump::new();
        let result = parse("echo \"hello\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Expr { expr, .. } => match expr {
                Expr::Call { callee, args, .. } => {
                    assert!(
                        matches!(callee, Expr::Identifier { name, .. } if name.to_string() == "echo")
                    );
                    assert_eq!(args.len(), 1);
                    assert!(
                        matches!(args[0], Expr::String { value, .. } if value.to_string() == "hello")
                    );
                }
                _ => panic!("expected call, got {:?}", expr),
            },
            _ => panic!("expected expr statement"),
        }
    }

    #[test]
    fn juxtaposition_call_with_identifier() {
        let arena = Bump::new();
        let result = parse("echo message;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Expr { expr, .. } => match expr {
                Expr::Call { callee, args, .. } => {
                    assert!(
                        matches!(callee, Expr::Identifier { name, .. } if name.to_string() == "echo")
                    );
                    assert_eq!(args.len(), 1);
                    assert!(
                        matches!(args[0], Expr::Identifier { name, .. } if name.to_string() == "message")
                    );
                }
                _ => panic!("expected call, got {:?}", expr),
            },
            _ => panic!("expected expr statement"),
        }
    }

    #[test]
    fn juxtaposition_call_respects_precedence() {
        let arena = Bump::new();
        let result = parse("echo x + 1;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Expr { expr, .. } => match expr {
                Expr::Binary {
                    op: BinOp::Add,
                    left,
                    right,
                    ..
                } => {
                    assert!(matches!(left, Expr::Call { .. }));
                    assert!(matches!(right, Expr::Number { value, .. } if *value == 1.0));
                }
                _ => panic!("expected binary add, got {:?}", expr),
            },
            _ => panic!("expected expr statement"),
        }
    }

    #[test]
    fn juxtaposition_call_does_not_span_newline() {
        let arena = Bump::new();
        let result = parse("echo\n\"hello\";", &arena);
        // `echo` as a standalone expression statement is syntactically valid but
        // unknown at typecheck time; parsing should not swallow the string as an
        // argument.
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        assert_eq!(program.statements.len(), 2);
        assert!(
            matches!(program.statements[0], Stmt::Expr { expr: Expr::Identifier { name, .. }, .. } if name.to_string() == "echo")
        );
        assert!(matches!(
            program.statements[1],
            Stmt::Expr {
                expr: Expr::String { .. },
                ..
            }
        ));
    }

    #[test]
    fn juxtaposition_call_excludes_minus() {
        let arena = Bump::new();
        let result = parse("echo -1;", &arena);
        // Should parse as binary subtraction `echo - 1`, not a call.
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Expr { expr, .. } => {
                assert!(matches!(expr, Expr::Binary { op: BinOp::Sub, .. }));
            }
            _ => panic!("expected expr statement"),
        }
    }

    #[test]
    fn normal_call_syntax_still_works() {
        let arena = Bump::new();
        let result = parse("echo(\"hello\");", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Expr { expr, .. } => match expr {
                Expr::Call { callee, args, .. } => {
                    assert!(
                        matches!(callee, Expr::Identifier { name, .. } if name.to_string() == "echo")
                    );
                    assert_eq!(args.len(), 1);
                }
                _ => panic!("expected call, got {:?}", expr),
            },
            _ => panic!("expected expr statement"),
        }
    }

    #[test]
    fn ternary_precedence_nesting_and_expression_positions() {
        fn shape(expr: &Expr<'_>) -> String {
            match expr {
                Expr::Identifier { name, .. } => name.to_string(),
                Expr::Binary {
                    op, left, right, ..
                } => format!("({op:?} {} {})", shape(left), shape(right)),
                Expr::Ternary {
                    condition,
                    then_branch,
                    else_branch,
                    ..
                } => format!(
                    "(?: {} {} {})",
                    shape(condition),
                    shape(then_branch),
                    shape(else_branch)
                ),
                other => panic!("unexpected expression: {other:?}"),
            }
        }
        for (source, expected) in [
            ("a || b ? c : d", "(?: (Or a b) c d)"),
            ("a && b || c ? d : e", "(?: (Or (And a b) c) d e)"),
            ("a ? b || c : d && e", "(?: a (Or b c) (And d e))"),
            ("a ? b : c ? d : e", "(?: a b (?: c d e))"),
            ("a ? b ? c : d : e", "(?: a (?: b c d) e)"),
            ("x = a ? b : c", "(Assign x (?: a b c))"),
            ("a ? x = b : x = c", "(?: a (Assign x b) (Assign x c))"),
            ("a\n?\nb\n:\nc", "(?: a b c)"),
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.errors.is_empty(), "{source}: {:?}", result.errors);
            let program = result.program.unwrap();
            let Stmt::Expr { expr, .. } = &program.statements[0] else {
                panic!("expected expression")
            };
            assert_eq!(shape(expr), expected, "{source}");
        }
        for source in [
            "const x = f(a ? b : c)",
            "const x = [a ? b : c]",
            "const x = items[a ? b : c]",
            "const x = (a ? b : c) + d",
            "const x = <div title={a ? b : c}>{a ? <span /> : <b />}</div>",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.errors.is_empty(), "{source}: {:?}", result.errors);
        }
    }

    #[test]
    fn ternary_requires_both_arms_and_colon() {
        for source in ["a ? b", "a ? : c", "a ? b :", "a ? b c"] {
            let arena = Bump::new();
            assert!(!parse(source, &arena).errors.is_empty(), "{source}");
        }
    }

    // dsc#244: a newline before the closing `)` of a parenthesized
    // expression must not be a parse error. `parse_expression` already
    // skips *leading* newlines (see `parse_expr`'s own `skip_newlines`
    // call), but the `TokenKind::LParen` prefix arm in `parse_prefix`
    // (crates/deka_syntax/src/parse/expr.rs) went straight from the
    // inner expression to `expect(RParen)` with no trailing
    // `skip_newlines()` — unlike call-argument lists and array literals,
    // which both skip newlines before their closing delimiter. That
    // asymmetry is exactly why multi-line parameter lists parsed fine
    // while multi-line parenthesized expressions did not.
    #[test]
    fn multiline_paren_expression_parses() {
        for source in [
            // Literal.
            "return (\n  42\n)",
            // Arithmetic.
            "const x = (\n  1 + 2\n)",
            // JSX (the shape reported in the issue).
            "export fn Layout(props: LayoutProps) ReactNode {\n  return (\n    <div>\n      <main> {props.children} </main>\n    </div>\n  )\n}",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.errors.is_empty(), "{source}: {:?}", result.errors);
            assert!(result.program.is_some(), "{source}");
        }
    }

    #[test]
    fn nested_multiline_paren_expressions_parse() {
        for source in [
            // A parenthesized expression nested inside another.
            "const x = (\n  (\n    1 + 2\n  )\n)",
            // A multi-line paren expression as a call argument.
            "const x = f(\n  (\n    1 + 2\n  )\n)",
            // Multi-line paren mixed with a trailing call.
            "const x = (\n  a + b\n).toString()",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(result.errors.is_empty(), "{source}: {:?}", result.errors);
            assert!(result.program.is_some(), "{source}");
        }
    }

    // rfd#12 amendment, dsc#282: `import.meta` as a meta-property.

    #[test]
    fn import_meta_field_access_parses() {
        for field in ["url", "dirname", "filename", "main"] {
            let source = format!("const x = import.meta.{field}");
            let arena = Bump::new();
            let result = parse(&source, &arena);
            assert!(result.errors.is_empty(), "{source}: {:?}", result.errors);
            let program = result.program.expect("parses");
            match program.statements[0] {
                Stmt::Const {
                    value: Expr::FieldAccess { object, field: got_field, .. },
                    ..
                } => {
                    assert_eq!(got_field, field);
                    assert!(
                        matches!(object, Expr::ImportMeta { .. }),
                        "expected Expr::ImportMeta, got {object:?}"
                    );
                }
                ref other => panic!("expected const with field access, got {other:?}"),
            }
        }
    }

    #[test]
    fn import_meta_resolve_call_parses() {
        let arena = Bump::new();
        let result = parse("const x = import.meta.resolve(\"./sibling.ds\")", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parses");
        match program.statements[0] {
            Stmt::Const {
                value: Expr::Call { callee, .. },
                ..
            } => {
                assert!(matches!(
                    callee,
                    Expr::FieldAccess { field: "resolve", .. }
                ));
            }
            ref other => panic!("expected const with call, got {other:?}"),
        }
    }

    #[test]
    fn import_meta_without_dot_meta_errors() {
        let arena = Bump::new();
        let result = parse("const x = import.other", &arena);
        assert!(
            result.errors.iter().any(|e| e.message.contains("import.meta")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn dynamic_import_call_errors_with_static_import_message() {
        let arena = Bump::new();
        let result = parse("const x = import(spec)", &arena);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("imports must be static")),
            "{:?}",
            result.errors
        );
    }
}
