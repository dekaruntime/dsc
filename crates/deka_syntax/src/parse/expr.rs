//! Expression parsing (Pratt parser).

use crate::ast::{alloc, alloc_slice, Expr, StructLiteralField, Type, UnOp};
use crate::lexer::TokenKind;

use super::util::{infix_info, token_name};
use super::Parser;

impl<'a> Parser<'a> {
    pub(super) fn parse_expression(&mut self) -> Option<Expr<'a>> {
        self.skip_newlines();
        self.parse_expr(0)
    }

    fn parse_expr(&mut self, min_prec: u8) -> Option<Expr<'a>> {
        self.skip_newlines();
        let (start, start_byte) = self.span_start();
        let mut left = self.parse_prefix()?;

        loop {
            // Optional-semicolon rule: do not eagerly skip newlines here.
            // A newline terminates the expression unless the following token
            // clearly continues it (a leading `.` for method chaining, or a
            // binary operator). We peek past newlines to make that decision
            // without consuming a statement terminator.
            let next_kind = self.peek_after_newlines();

            // Juxtaposition call syntax: `fn arg` is sugar for `fn(arg)`.
            // The callee must be a bare identifier, the argument must start on
            // the same line, and it cannot be an operator that would conflict
            // with infix parsing (`-`, `+`, `|`).
            if let Expr::Identifier {
                span: callee_span, ..
            } = &left
            {
                let current = self.current();
                if callee_span.end.line == current.span.start.line
                    && can_start_juxtaposition_arg(current.kind)
                {
                    let arg = self.parse_expr(JUXTAPOSITION_ARG_PREC)?;
                    let span = self.span_from(start, start_byte);
                    left = Expr::Call {
                        callee: alloc(self.arena, left),
                        type_args: &[],
                        args: alloc_slice(self.arena, vec![arg]),
                        span,
                    };
                    continue;
                }
            }

            // Postfix member access: allow newline before `.` for chaining.
            if next_kind == TokenKind::Dot {
                self.skip_newlines();
                self.advance(); // `.`
                let field = self.expect_field_name()?;
                let span = self.span_from(start, start_byte);
                left = Expr::FieldAccess {
                    object: alloc(self.arena, left),
                    field,
                    span,
                };
                continue;
            }

            // Explicit type arguments: `id<number>(args)`. Do not span newlines
            // before `<`; a newline should terminate the statement instead.
            let type_args = if self.at(TokenKind::Lt) && self.next_lt_is_type_args() {
                self.try_parse_type_args().unwrap_or(&[])
            } else {
                &[]
            };

            if self.at(TokenKind::LParen) {
                self.advance();
                // Newlines are insignificant inside call parens: a multi-line
                // argument list must not terminate the expression (optional
                // semicolons make the newline significant only outside).
                self.skip_newlines();
                let mut args = Vec::new();
                if !self.at(TokenKind::RParen) {
                    loop {
                        args.push(self.parse_expression()?);
                        self.skip_newlines();
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                    }
                }
                self.skip_newlines();
                self.expect(TokenKind::RParen)?;
                let span = self.span_from(start, start_byte);

                // Built-in prelude enum constructors: Some/Ok/Err take one payload.
                if type_args.is_empty() {
                    if let Expr::Identifier { name, .. } = &left {
                        if let Some((enum_name, _requires_payload)) = builtin_enum_constructor(name) {
                            if args.len() == 1 {
                                left = Expr::EnumConstructor {
                                    enum_name: self.bump_str(enum_name),
                                    case_name: name,
                                    payload: Some(alloc(self.arena, args.into_iter().next().unwrap())),
                                    span,
                                };
                                continue;
                            }
                        }
                    }
                }

                left = Expr::Call {
                    callee: alloc(self.arena, left),
                    type_args,
                    args: alloc_slice(self.arena, args),
                    span,
                };
                continue;
            }

            // Index access: `arr[0]` or `obj["key"]`. Newlines are
            // insignificant inside the brackets, as in call parens.
            if self.at(TokenKind::LBracket) {
                self.advance();
                self.skip_newlines();
                let index = self.parse_expression()?;
                self.skip_newlines();
                self.expect(TokenKind::RBracket)?;
                let span = self.span_from(start, start_byte);
                left = Expr::IndexAccess {
                    object: alloc(self.arena, left),
                    index: alloc(self.arena, index),
                    span,
                };
                continue;
            }

            // Struct literal: `Name { field: expr, ... }`.
            // We peek ahead to confirm this is really a struct literal and not
            // a block/record-like construct (e.g. a match body after the
            // scrutinee). It must be empty `{}` or start with `field: expr`.
            if self.at(TokenKind::LBrace) && self.looks_like_struct_literal() {
                if let Expr::Identifier { name, .. } = &left {
                    let struct_name = *name;
                    self.advance();
                    let mut fields = Vec::new();
                    if !self.at(TokenKind::RBrace) {
                        loop {
                            let (field_start, field_start_byte) = self.span_start();
                            let field_name = self.expect_identifier()?;
                            self.expect(TokenKind::Colon)?;
                            let value = self.parse_expression()?;
                            fields.push(StructLiteralField {
                                name: field_name,
                                value,
                                span: self.span_from(field_start, field_start_byte),
                            });
                            if !self.eat(TokenKind::Comma) {
                                break;
                            }
                            self.skip_newlines();
                        }
                    }
                    self.expect(TokenKind::RBrace)?;
                    let span = self.span_from(start, start_byte);
                    left = Expr::StructLiteral {
                        name: struct_name,
                        fields: alloc_slice(self.arena, fields),
                        span,
                    };
                    continue;
                }
            }

            // Ternary conditional: `cond ? then : else`. Low precedence,
            // right-associative, and binds looser than `||`.
            if next_kind == TokenKind::Question && min_prec <= 2 {
                self.skip_newlines();
                self.advance(); // `?`
                let then_branch = alloc(self.arena, self.parse_expr(2)?);
                self.skip_newlines();
                self.expect(TokenKind::Colon)?;
                self.skip_newlines();
                let else_branch = alloc(self.arena, self.parse_expr(2)?);
                let span = self.span_from(start, start_byte);
                left = Expr::Ternary {
                    condition: alloc(self.arena, left),
                    then_branch,
                    else_branch,
                    span,
                };
                continue;
            }

            let (lbp, rbp, op) = match infix_info(next_kind) {
                Some(info) => info,
                None => break,
            };

            if lbp < min_prec {
                break;
            }

            // Commit to the binary operator: skip any newlines before it, then
            // the operator itself, then any newlines after it, then the RHS.
            self.skip_newlines();
            self.advance();
            self.skip_newlines();
            let right = self.parse_expr(rbp)?;
            let span = self.span_from(start, start_byte);

            left = Expr::Binary {
                op,
                left: alloc(self.arena, left),
                right: alloc(self.arena, right),
                span,
            };
        }

        Some(left)
    }

    fn parse_prefix(&mut self) -> Option<Expr<'a>> {
        self.skip_newlines();
        let (start, start_byte) = self.span_start();

        match self.current_kind() {
            TokenKind::Number => {
                let text = self.current_text();
                let without_underscores: String = text.chars().filter(|&c| c != '_').collect();
                let value = match without_underscores.parse::<f64>() {
                    Ok(v) => v,
                    Err(_) => {
                        self.error(format!("invalid number literal `{}`", text));
                        return None;
                    }
                };
                self.advance();
                Some(Expr::Number {
                    value,
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::String => {
                let text = self.current_text();
                let unescaped = unescape_string(text);
                let value = self.bump_str(&unescaped);
                self.advance();
                Some(Expr::String {
                    value,
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::BacktickString => {
                let value = self.bump_str(self.current_text());
                self.advance();
                Some(Expr::TemplateLiteral {
                    parts: alloc_slice(self.arena, vec![crate::ast::TemplatePart::Text(value)]),
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::True => {
                self.advance();
                Some(Expr::Boolean {
                    value: true,
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::False => {
                self.advance();
                Some(Expr::Boolean {
                    value: false,
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::None => {
                self.advance();
                Some(Expr::None {
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::Identifier => {
                let name = self.bump_str(self.current_text());
                self.advance();
                Some(Expr::Identifier {
                    name,
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::LParen => {
                self.advance();
                let expr = self.parse_expression()?;
                self.expect(TokenKind::RParen)?;
                Some(Expr::Paren {
                    expr: alloc(self.arena, expr),
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::Minus => {
                self.advance();
                let operand = self.parse_expr(12)?;
                Some(Expr::Unary {
                    op: UnOp::Neg,
                    operand: alloc(self.arena, operand),
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::Plus => {
                self.advance();
                let operand = self.parse_expr(12)?;
                Some(Expr::Unary {
                    op: UnOp::Plus,
                    operand: alloc(self.arena, operand),
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::Not => {
                self.advance();
                let operand = self.parse_expr(12)?;
                Some(Expr::Unary {
                    op: UnOp::Not,
                    operand: alloc(self.arena, operand),
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::Match => {
                self.advance();
                let scrutinee = alloc(self.arena, self.parse_expression()?);
                let arms = self.parse_match_arms()?;
                Some(Expr::Match {
                    scrutinee,
                    arms,
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::Await => {
                self.advance();
                let operand = self.parse_expr(12)?;
                Some(Expr::Await {
                    expr: alloc(self.arena, operand),
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::Async => {
                if self.tokens.get(self.pos + 1).map(|t| t.kind) == Some(TokenKind::Fn) {
                    self.parse_fn_expression(start, start_byte)
                } else {
                    self.error("expected `fn` after `async`");
                    None
                }
            }
            TokenKind::Fn => self.parse_fn_expression(start, start_byte),
            TokenKind::Unsafe => self.parse_unsafe_expression(start, start_byte),
            TokenKind::Bridge => self.parse_bridge_expression(start, start_byte),
            TokenKind::Lt => self.parse_jsx(start, start_byte),
            TokenKind::LBracket => {
                self.advance();
                let mut elements = Vec::new();
                if !self.at(TokenKind::RBracket) {
                    loop {
                        elements.push(self.parse_spreadable_expr()?);
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                        if self.at(TokenKind::RBracket) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RBracket)?;
                Some(Expr::Array {
                    elements: alloc_slice(self.arena, elements),
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::LBrace => {
                self.advance();
                let mut fields = Vec::new();
                if !self.at(TokenKind::RBrace) {
                    loop {
                        let (field_start, field_start_byte) = self.span_start();
                        if self.eat(TokenKind::Spread) {
                            let expr = self.parse_expression()?;
                            fields.push(crate::ast::ObjectField {
                                key: "",
                                value: expr,
                                span: self.span_from(field_start, field_start_byte),
                            });
                        } else {
                            let key = self.expect_object_key()?;
                            self.expect(TokenKind::Colon)?;
                            let value = self.parse_expression()?;
                            fields.push(crate::ast::ObjectField {
                                key,
                                value,
                                span: self.span_from(field_start, field_start_byte),
                            });
                        }
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                        self.skip_newlines();
                        if self.at(TokenKind::RBrace) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RBrace)?;
                Some(Expr::Object {
                    fields: alloc_slice(self.arena, fields),
                    span: self.span_from(start, start_byte),
                })
            }
            TokenKind::Super => {
                // `super` is a hard keyword; in expression position there is
                // nothing it can validly start. It is reserved for
                // declarations (super struct, super fn) which are not yet
                // available (deka#561).
                self.error("`super` is reserved and not yet available");
                None
            }
            _ => {
                self.error(format!(
                    "expected expression, found `{}`",
                    token_name(self.current_kind())
                ));
                None
            }
        }
    }
}

/// Returns `(enum_name, requires_payload)` for built-in prelude enum
/// constructors that are parsed specially instead of as function calls.
fn builtin_enum_constructor(name: &str) -> Option<(&'static str, bool)> {
    match name {
        "Some" => Some(("Option", true)),
        "Ok" => Some(("Result", true)),
        "Err" => Some(("Result", true)),
        _ => None,
    }
}

impl<'a> Parser<'a> {
    /// Peek at the tokens after the current `{` to decide whether this is a
    /// struct literal (`Name {}` or `Name { a: 1 }`) or something else.
    fn looks_like_struct_literal(&self) -> bool {
        let next = self.tokens.get(self.pos + 1).map(|t| t.kind);
        match next {
            Some(TokenKind::RBrace) => true,
            Some(TokenKind::Identifier) => {
                let next_next = self.tokens.get(self.pos + 2).map(|t| t.kind);
                matches!(next_next, Some(TokenKind::Colon))
            }
            _ => false,
        }
    }

    /// Parse an `unsafe { ... }` raw JavaScript block.
    ///
    /// The lexer emits the body as a single `RawJs` token, so the parser only
    /// needs to consume the surrounding braces. The contents are not parsed as
    /// DekaScript.
    fn parse_unsafe_expression(
        &mut self,
        start: crate::ast::Pos,
        start_byte: usize,
    ) -> Option<Expr<'a>> {
        self.advance(); // `unsafe`

        // Optional success-type annotation: `unsafe<T> { ... }` (deka#460).
        let result_type = if self.at(TokenKind::Lt) {
            self.advance(); // `<`
            let ty = self.parse_type()?;
            if !self.at(TokenKind::Gt) {
                self.error("expected `>` after `unsafe` result type");
                return None;
            }
            self.advance(); // `>`
            Some(ty)
        } else {
            None
        };

        if !self.at(TokenKind::LBrace) {
            self.error("expected `{` after `unsafe`");
            return None;
        }
        self.advance(); // `{`

        let source = if self.at(TokenKind::RawJs) {
            self.current_text()
        } else {
            ""
        };
        let source = self.bump_str(source);
        if self.at(TokenKind::RawJs) {
            self.advance();
        }

        if !self.at(TokenKind::RBrace) {
            self.error("unterminated `unsafe` block; expected `}`");
            return None;
        }
        self.advance(); // `}`

        Some(Expr::Unsafe {
            source,
            result_type,
            span: self.span_from(start, start_byte),
        })
    }

    /// Parse a host bridge expression: `bridge kind.action(arg1, arg2)`.
    fn parse_bridge_expression(
        &mut self,
        start: crate::ast::Pos,
        start_byte: usize,
    ) -> Option<Expr<'a>> {
        self.advance(); // `bridge`

        let kind = self.expect_identifier()?;

        if !self.at(TokenKind::Dot) {
            self.error("expected `.` between bridge kind and action, e.g. `bridge crypto.random_bytes(...)`");
            return None;
        }
        self.advance(); // `.`

        let action = self.expect_identifier()?;

        if !self.at(TokenKind::LParen) {
            self.error("expected `(` after bridge action");
            return None;
        }
        self.advance(); // `(`

        let mut args = Vec::new();
        if !self.at(TokenKind::RParen) {
            loop {
                args.push(self.parse_expression()?);
                if !self.eat(TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
        }
        self.expect(TokenKind::RParen)?;

        Some(Expr::Bridge {
            kind: self.bump_str(kind),
            action: self.bump_str(action),
            args: alloc_slice(self.arena, args),
            span: self.span_from(start, start_byte),
        })
    }

    /// Parse an anonymous function expression: `fn (x: number) number { ... }`.
    fn parse_fn_expression(
        &mut self,
        start: crate::ast::Pos,
        start_byte: usize,
    ) -> Option<Expr<'a>> {
        let is_async = self.eat(TokenKind::Async);
        self.advance(); // `fn`
        self.expect(TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(TokenKind::RParen)?;

        let return_type = if self.at(TokenKind::LBrace) {
            None
        } else if self.eat(TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            Some(self.parse_type()?)
        };

        let body = self.parse_block()?;
        Some(Expr::Function {
            params,
            return_type,
            body,
            is_async,
            span: self.span_from(start, start_byte),
        })
    }

    /// Parse an expression that may be a spread element (`...expr`).
    fn parse_spreadable_expr(&mut self) -> Option<Expr<'a>> {
        let (start, start_byte) = self.span_start();
        if self.eat(TokenKind::Spread) {
            let expr = self.parse_expression()?;
            Some(Expr::Spread {
                expr: alloc(self.arena, expr),
                span: self.span_from(start, start_byte),
            })
        } else {
            self.parse_expression()
        }
    }

    /// Parse an object literal key: identifier, keyword (contextual in name
    /// position, e.g. `{ type: "text" }`), or string. A keyword is only
    /// treated as a key when a `:` follows, so statement-only keywords in a
    /// block-like position (`match { 2 => { break } }`) still error instead
    /// of being silently reinterpreted as an object key.
    fn expect_object_key(&mut self) -> Option<&'a str> {
        match self.current_kind() {
            kind if super::kind_is_name_capable(kind) => {
                if kind != TokenKind::Identifier && self.tokens.get(self.pos + 1).map(|t| t.kind) != Some(TokenKind::Colon) {
                    self.error(format!(
                        "expected object key, found `{}`",
                        token_name(self.current_kind())
                    ));
                    return None;
                }
                let key = self.bump_str(self.current_text());
                self.advance();
                Some(key)
            }
            TokenKind::String => {
                let key = self.bump_str(self.current_text());
                self.advance();
                Some(key)
            }
            _ => {
                self.error(format!(
                    "expected object key, found `{}`",
                    token_name(self.current_kind())
                ));
                None
            }
        }
    }

    /// Peek at the `<` at the current position and decide whether it opens an
    /// explicit type argument list that is immediately followed by a call `(`.
    /// This prevents `<` in comparison expressions (`a < b`) from being parsed
    /// as (and erroring during) a speculative type argument list.
    fn next_lt_is_type_args(&self) -> bool {
        if !self.at(TokenKind::Lt) {
            return false;
        }
        let mut depth = 1usize;
        let mut i = self.pos + 1;
        while i < self.tokens.len() {
            match self.tokens[i].kind {
                TokenKind::Lt => depth += 1,
                TokenKind::Gt => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return i + 1 < self.tokens.len()
                            && self.tokens[i + 1].kind == TokenKind::LParen;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Try to parse explicit type arguments `<T, U>`.
    /// On success, returns the parsed types and advances the cursor.
    /// On failure, leaves the cursor unchanged.
    fn try_parse_type_args(&mut self) -> Option<&'a [Type<'a>]> {
        let saved_pos = self.pos;
        let saved_prev = self.prev.clone();

        if !self.eat(TokenKind::Lt) {
            return None;
        }

        let mut args = Vec::new();
        if !self.at(TokenKind::Gt) {
            loop {
                match self.parse_type() {
                    Some(ty) => args.push(ty),
                    None => {
                        self.pos = saved_pos;
                        self.prev = saved_prev;
                        return None;
                    }
                }
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }

        if !self.eat(TokenKind::Gt) {
            self.pos = saved_pos;
            self.prev = saved_prev;
            return None;
        }

        Some(alloc_slice(self.arena, args))
    }
}

/// Unescape a string literal body (without surrounding quotes).
/// Recognizes the standard C-style escapes used in DekaScript.
fn unescape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some(c) => {
                // Unknown escape: keep both characters to preserve source meaning.
                out.push('\\');
                out.push(c);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Precedence used when parsing a juxtaposition argument. It is higher than
/// every binary operator so that `fn arg + 1` parses as `(fn arg) + 1`.
const JUXTAPOSITION_ARG_PREC: u8 = 13;

/// True when `kind` can start a primary expression that is valid as a
/// juxtaposition call argument. Only literals and identifiers are allowed,
/// which avoids ambiguity with struct literals (`Point { ... }`), index
/// access (`arr[0]`), normal call syntax (`fn()`), and JSX (`<div />`).
/// `-`, `+`, and `|` are excluded because they would be mistaken for infix
/// operators.
fn can_start_juxtaposition_arg(kind: TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        kind,
        Number | String | BacktickString | True | False | None | Identifier
    )
}
