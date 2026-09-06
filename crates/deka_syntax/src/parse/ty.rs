//! Type parsing.

use crate::ast::{alloc, alloc_slice, Pos, Span, Type};
use crate::lexer::{Token, TokenKind};

use super::util::token_name;
use super::Parser;

impl<'a> Parser<'a> {
    pub(super) fn parse_type(&mut self) -> Option<Type<'a>> {
        let first = self.parse_type_postfix()?;
        if !self.at(TokenKind::Bar) {
            return Some(first);
        }
        // Union type: `A | B`. `|` binds looser than postfix `?`, so
        // `A | B?` parses as `A | (B?)` (rfd#42, deka#530).
        let mut members = vec![first];
        while self.eat(TokenKind::Bar) {
            self.skip_newlines();
            members.push(self.parse_type_postfix()?);
        }
        let first_span = members.first().unwrap().span();
        let last_span = members.last().unwrap().span();
        let span = Span {
            start: first_span.start,
            end: last_span.end,
            byte_start: first_span.byte_start,
            byte_end: last_span.byte_end,
        };
        Some(Type::Union {
            members: alloc_slice(self.arena, members),
            span,
        })
    }

    /// Primary type plus an optional postfix `?`.
    fn parse_type_postfix(&mut self) -> Option<Type<'a>> {
        let ty = self.parse_type_primary()?;
        if self.eat(TokenKind::Question) {
            let span = ty.span();
            return Some(Type::Option {
                inner: alloc(self.arena, ty),
                span,
            });
        }
        Some(ty)
    }

    /// Expect the `>` closing a generic argument list.
    ///
    /// The lexer emits `>>` as a single `Shr` token, so a nested type like
    /// `Option<Option<number>>` presents `Shr` where the inner generic's
    /// closing `>` is expected (deka#465). Split it: consume the first half
    /// as this generic's `>`, leaving a `Gt` covering the second half for the
    /// enclosing generic to consume. The expression parser is unaffected —
    /// `8 >> 1` still sees the whole `Shr`.
    fn expect_gt(&mut self) -> Option<()> {
        if self.eat(TokenKind::Gt) {
            return Some(());
        }
        if self.at(TokenKind::Shr) {
            let span = self.current_span();
            let first = Token {
                kind: TokenKind::Gt,
                text: &self.source[span.byte_start..span.byte_start + 1],
                span: Span {
                    start: span.start,
                    end: Pos {
                        line: span.start.line,
                        column: span.start.column + 1,
                    },
                    byte_start: span.byte_start,
                    byte_end: span.byte_start + 1,
                },
            };
            self.tokens[self.pos] = Token {
                kind: TokenKind::Gt,
                text: &self.source[span.byte_start + 1..span.byte_end],
                span: Span {
                    start: Pos {
                        line: span.start.line,
                        column: span.start.column + 1,
                    },
                    end: span.end,
                    byte_start: span.byte_start + 1,
                    byte_end: span.byte_end,
                },
            };
            // Like advance(), but the position stays: the second half of the
            // split token is the new current token.
            self.prev = first;
            return Some(());
        }
        self.error(format!(
            "expected `{}`, found `{}`",
            token_name(TokenKind::Gt),
            token_name(self.current_kind())
        ));
        None
    }

    fn parse_type_primary(&mut self) -> Option<Type<'a>> {
        self.skip_newlines();
        let (start, start_byte) = self.span_start();

        if self.eat(TokenKind::Fn) {
            // Function type: `fn(T, U) R`.
            self.expect(TokenKind::LParen)?;
            let mut params = Vec::new();
            if !self.at(TokenKind::RParen) {
                loop {
                    params.push(self.parse_type()?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                    self.skip_newlines();
                }
                self.skip_newlines();
            }
            self.skip_newlines();
            self.expect(TokenKind::RParen)?;
            let ret = self.parse_type()?;
            Some(Type::Function {
                params: alloc_slice(self.arena, params),
                ret: alloc(self.arena, ret),
                span: self.span_from(start, start_byte),
            })
        } else if self.eat(TokenKind::LParen) {
            // Grouped type `(T)`.
            let ty = self.parse_type()?;
            self.expect(TokenKind::RParen)?;
            Some(ty)
        } else if self.at(TokenKind::Identifier) {
            let name = self.bump_str(self.current_text());
            let span = self.current_span();
            self.advance();

            if self.eat(TokenKind::Lt) {
                let mut args = Vec::new();
                loop {
                    args.push(self.parse_type()?);
                    if !self.eat(TokenKind::Comma) {
                        break;
                    }
                    self.skip_newlines();
                }
                self.skip_newlines();
                self.expect_gt()?;
                Some(Type::Generic {
                    base: name,
                    args: alloc_slice(self.arena, args),
                    span: self.span_from(start, start_byte),
                })
            } else {
                Some(Type::Named { name, span })
            }
        } else {
            self.error(format!(
                "expected type, found `{}`",
                token_name(self.current_kind())
            ));
            None
        }
    }
}
