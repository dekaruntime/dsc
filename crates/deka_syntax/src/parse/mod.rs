//! DekaScript recursive-descent parser (Compiler v2).

use bumpalo::Bump;

use crate::ast::{Program, Span};
use crate::diagnostics::Diagnostic;
use crate::lexer::{Lexer, Token, TokenKind};

mod expr;
mod jsx;
mod pattern;
mod stmt;
mod ty;
mod util;

pub struct ParseResult<'a> {
    pub program: Option<Program<'a>>,
    pub errors: Vec<Diagnostic>,
}

/// Parse a full `.ds` source file into a DekaScript AST.
pub fn parse<'a>(source: &'a str, arena: &'a Bump) -> ParseResult<'a> {
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

    ParseResult {
        program: if errors.is_empty() { program } else { None },
        errors,
    }
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
    pos: usize,    prev: Token<'a>,
    errors: Vec<Diagnostic>,
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
        }
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

    fn bump_str(&self, s: &str) -> &'a str {
        self.arena.alloc_str(s)
    }

    fn error(&mut self, message: impl Into<String>) {
        let pos = self.current_span().start;
        self.errors
            .push(Diagnostic::error(pos.line, pos.column, message));
    }

    /// Rejects `fn f() T` -- the colon before a return type.
    ///
    /// DekaScript writes the return type directly after the parameter list:
    /// `fn f() T`. The colon form is TypeScript's and was silently tolerated
    /// here, so both spellings parsed to the same AST and the corpus drifted
    /// into a mix of the two. Reporting it keeps one spelling (deka#511).
    fn reject_return_type_colon(&mut self) {
        if self.at(TokenKind::Colon) {
            self.error(
                "unexpected `:` before the return type -- remove it. DekaScript writes `fn f() T`, not `fn f()` followed by `:`",
            );
            self.advance();
        }
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

    // deka#511: the return-type colon is TypeScript's, not DekaScript's. Both
    // spellings used to parse to the same AST, so the corpus drifted into a mix.

    #[test]
    fn return_type_colon_is_rejected() {
        let arena = Bump::new();
        let result = parse("fn f(): boolean { return true }", &arena);
        assert!(!result.errors.is_empty(), "colon form must not parse");
        assert!(
            result.errors[0].message.contains("remove it"),
            "diagnostic must say what to do: {}",
            result.errors[0].message
        );
    }

    #[test]
    fn return_type_colon_rejected_on_receiver_method() {
        let arena = Bump::new();
        let result = parse("fn (p P) get(): number { return p.x }", &arena);
        assert!(!result.errors.is_empty(), "colon form must not parse on methods");
    }

    #[test]
    fn return_type_without_colon_parses() {
        let arena = Bump::new();
        let result = parse("fn f() boolean { return true }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
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
        let result = parse("for (let i = 0; i < 3; i = i + 1) {\n  match (i) {\n    2 => { break },\n    _ => {}\n  }\n}", &arena);
        assert!(
            result.errors.iter().any(|e| e.message.contains("expected object key")),
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
                assert_eq!(params[0].name.to_string(), "a");
                assert_eq!(params[1].name.to_string(), "b");
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
        let result = parse(
            "const x: Option<Option<Option<number>>> = None;",
            &arena,
        );
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
                params, return_type, ..
            } => {
                assert!(matches!(&params[0].ty, Some(Type::Union { members, .. }) if members.len() == 2));
                assert!(matches!(return_type, Some(Type::Union { members, .. }) if members.len() == 2));
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
                    assert!(matches!(&members[1], Type::Option { inner, .. } if matches!(&**inner, Type::Named { name, .. } if name == &"number")));
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
                Expr::EnumConstructor { enum_name, case_name, .. } => {
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
        let result = parse(
            "struct Point {\n  x: number\n  y: number\n}",
            &arena,
        );
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
                Expr::EnumConstructor { enum_name, case_name, .. } => {
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
                    assert!(matches!(arms[0].pattern, Pattern::Constructor { name: "Red", .. }));
                    assert!(matches!(arms[1].pattern, Pattern::Constructor { name: "Green", .. }));
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
    fn parse_generic_function_banned() {
        // User code cannot declare type parameters (deka#561): every
        // declaration form is rejected with the diagnostic that teaches
        // the two replacement patterns.
        for source in [
            "fn id<T>(x: T) T { return x; }",
            "fn (p Point) distance<T>(x: T) T { return x; }",
            "struct Box<T> { value: T }",
            "enum Maybe<T> { Some(T), None }",
            "alias Pair<A, B> = Array<number>",
            "type Pair<T> number",
            "interface Container<T> { fn get() T }",
        ] {
            let arena = Bump::new();
            let result = parse(source, &arena);
            assert!(
                result
                    .errors
                    .iter()
                    .any(|e| e.message.contains("user code cannot declare type parameters")),
                "{source:?} must be rejected, got: {:?}",
                result.errors
            );
        }

        // The type_params AST field remains (always empty) for builtin and
        // PR B use.
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
    fn parse_import_named() {
        let arena = Bump::new();
        let result = parse("import { add } from \"./math.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import { specifiers, source, .. } => {
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
    fn parse_import_side_effect() {
        let arena = Bump::new();
        let result = parse("import \"./side-effects.ds\";", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Import { specifiers, source, .. } => {
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
        let result = parse("export fn add(a: number, b: number) number { return a + b; }", &arena);
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
    fn parse_export_default_rejected() {
        let arena = Bump::new();
        let result = parse("export default 42;", &arena);
        assert!(!result.errors.is_empty());
        assert!(
            result.errors.iter().any(|e| e.message.contains("default exports")),
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
                Expr::Binary { op: BinOp::Pipe, left, right, .. } => {
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
        let arena = Bump::new();
        let result = parse("const r = unsafe { function f() { return 1; } f() };", &arena);
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
        assert!(result.program.is_none(), "member expression should fail to parse");
        assert!(
            result.errors.iter().any(|e| e.message.contains("member expressions")),
            "expected member expression error, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn parse_template_literal() {
        let arena = Bump::new();
        let result = parse("const s = `hello ${x}`;", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::TemplateLiteral { parts, .. } => {
                    assert_eq!(parts.len(), 1);
                    assert!(matches!(parts[0], TemplatePart::Text(text) if text == "hello ${x}"));
                }
                _ => panic!("expected template literal, got {:?}", value),
            },
            _ => panic!("expected const declaration"),
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
        let result = parse("fn add(a: number, b: number) number {\n  const c = a + b\n  return c\n}", &arena);
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
                Expr::Binary { op, left, right, .. } => {
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
                assert_eq!(params[0].name.to_string(), "a");
                assert_eq!(params[1].name.to_string(), "b");
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
        assert!(result.program.is_none(), "expected parse failure without newline or semicolon");
    }

    #[test]
    fn parse_fn_expression_literal() {
        let arena = Bump::new();
        let result = parse("const double = fn (x: number) number { return x * 2 }", &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        match &program.statements[0] {
            Stmt::Const { value, .. } => match value {
                Expr::Function { params, return_type, body, .. } => {
                    assert_eq!(params.len(), 1);
                    assert_eq!(params[0].name, "x");
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
            Stmt::For { init, condition, step, body, .. } => {
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
                    assert!(matches!(callee, Expr::Identifier { name, .. } if name.to_string() == "echo"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0], Expr::String { value, .. } if value.to_string() == "hello"));
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
                    assert!(matches!(callee, Expr::Identifier { name, .. } if name.to_string() == "echo"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0], Expr::Identifier { name, .. } if name.to_string() == "message"));
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
                Expr::Binary { op: BinOp::Add, left, right, .. } => {
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
        assert!(matches!(program.statements[0], Stmt::Expr { expr: Expr::Identifier { name, .. }, .. } if name.to_string() == "echo"));
        assert!(matches!(program.statements[1], Stmt::Expr { expr: Expr::String { .. }, .. }));
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
                    assert!(matches!(callee, Expr::Identifier { name, .. } if name.to_string() == "echo"));
                    assert_eq!(args.len(), 1);
                }
                _ => panic!("expected call, got {:?}", expr),
            },
            _ => panic!("expected expr statement"),
        }
    }
}
