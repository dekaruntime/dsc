//! Pattern parsing for `match` arms.

use crate::ast::{MatchArm, Pattern, PatternField};
use crate::lexer::TokenKind;

use super::util::token_name;
use super::Parser;

impl<'a> Parser<'a> {
    pub(super) fn parse_match_arms(&mut self) -> Option<&'a [MatchArm<'a>]> {
        self.expect(TokenKind::LBrace)?;
        let mut arms = Vec::new();
        self.skip_newlines();

        while !self.at(TokenKind::RBrace) && !self.at_end() {
            arms.push(self.parse_match_arm()?);

            if self.at(TokenKind::RBrace) {
                break;
            }
            if self.eat(TokenKind::Comma) {
                self.skip_newlines();
                if self.at(TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            if self.eat(TokenKind::Semicolon) {
                self.skip_newlines();
                if self.at(TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            if self.at(TokenKind::Newline) {
                self.skip_newlines();
                if self.at(TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            self.error("expected `,` or newline between match arms");
            break;
        }

        self.expect(TokenKind::RBrace)?;
        Some(crate::ast::alloc_slice(self.arena, arms))
    }

    fn parse_match_arm(&mut self) -> Option<MatchArm<'a>> {
        let (start, start_byte) = self.span_start();
        let first = self.parse_pattern()?;

        // `A | B | C => …` (deka#446).
        let pattern = if self.at(TokenKind::Bar) {
            let mut alternatives = vec![first];
            while self.eat(TokenKind::Bar) {
                self.skip_newlines();
                alternatives.push(self.parse_pattern()?);
            }
            Pattern::Or {
                alternatives: crate::ast::alloc_slice(self.arena, alternatives),
                span: self.span_from(start, start_byte),
            }
        } else {
            first
        };

        self.expect(TokenKind::FatArrow)?;
        let body = self.parse_expression()?;
        Some(MatchArm {
            pattern,
            guard: None,
            body,
            span: self.span_from(start, start_byte),
        })
    }

    pub(super) fn parse_pattern(&mut self) -> Option<Pattern<'a>> {
        self.skip_newlines();
        let (start, start_byte) = self.span_start();

        match self.current_kind() {
            TokenKind::Identifier => {
                let name = self.bump_str(self.current_text());
                self.advance();

                if name == "_" {
                    return Some(Pattern::Wildcard {
                        span: self.span_from(start, start_byte),
                    });
                }

                // Enum-qualified constructor: `Color.Red` or `Color.Red(p)`.
                if self.at(TokenKind::Dot) {
                    self.advance();
                    let case_name = self.expect_field_name()?;
                    if self.at(TokenKind::LParen) {
                        self.advance();
                        let payload = if self.at(TokenKind::RParen) {
                            None
                        } else {
                            Some(crate::ast::alloc(self.arena, self.parse_pattern()?))
                        };
                        self.expect(TokenKind::RParen)?;
                        return Some(Pattern::Constructor {
                            name: case_name,
                            payload,
                            span: self.span_from(start, start_byte),
                        });
                    }
                    return Some(Pattern::Constructor {
                        name: case_name,
                        payload: None,
                        span: self.span_from(start, start_byte),
                    });
                }

                if self.at(TokenKind::LBrace) {
                    // Struct pattern: `Name { field, field: p }`
                    self.advance();
                    let mut fields = Vec::new();
                    if !self.at(TokenKind::RBrace) {
                        loop {
                            let (field_start, field_start_byte) = self.span_start();
                            let field_name = self.expect_identifier()?;
                            let pattern = if self.eat(TokenKind::Colon) {
                                self.parse_pattern()?
                            } else {
                                Pattern::Identifier {
                                    name: field_name,
                                    span: self.span_from(field_start, field_start_byte),
                                }
                            };
                            fields.push(PatternField {
                                name: field_name,
                                pattern,
                                span: self.span_from(field_start, field_start_byte),
                            });
                            if !self.eat(TokenKind::Comma) {
                                break;
                            }
                            self.skip_newlines();
                        }
                    }
                    self.expect(TokenKind::RBrace)?;
                    return Some(Pattern::Struct {
                        name,
                        fields: crate::ast::alloc_slice(self.arena, fields),
                        span: self.span_from(start, start_byte),
                    });
                }

                if self.at(TokenKind::LParen) {
                    // Constructor pattern: `Name(p)` or `Name()`
                    self.advance();
                    let payload = if self.at(TokenKind::RParen) {
                        None
                    } else {
                        Some(crate::ast::alloc(self.arena, self.parse_pattern()?))
                    };
                    self.expect(TokenKind::RParen)?;
                    return Some(Pattern::Constructor {
                        name,
                        payload,
                        span: self.span_from(start, start_byte),
                    });
                }

                Some(Pattern::Identifier {
                    name,
                    span: self.span_from(start, start_byte),
                })
            }

            TokenKind::None => {
                self.advance();
                Some(Pattern::Constructor {
                    name: "None",
                    payload: None,
                    span: self.span_from(start, start_byte),
                })
            }

            TokenKind::Number | TokenKind::String | TokenKind::True | TokenKind::False => {
                let expr = self.parse_expression()?;
                Some(Pattern::Literal {
                    expr,
                    span: self.span_from(start, start_byte),
                })
            }

            TokenKind::LParen => {
                // Tuple pattern: `(a, b)` or grouped pattern `(p)`.
                self.advance();
                if self.at(TokenKind::RParen) {
                    self.error("empty tuple pattern is not allowed".to_string());
                    return None;
                }
                let first = self.parse_pattern()?;
                if self.eat(TokenKind::RParen) {
                    return Some(first);
                }
                let mut elements = vec![first];
                while self.eat(TokenKind::Comma) {
                    self.skip_newlines();
                    if self.at(TokenKind::RParen) {
                        break;
                    }
                    elements.push(self.parse_pattern()?);
                }
                self.expect(TokenKind::RParen)?;
                Some(Pattern::Tuple {
                    elements: crate::ast::alloc_slice(self.arena, elements),
                    span: self.span_from(start, start_byte),
                })
            }

            _ => {
                self.error(format!(
                    "expected pattern, found `{}`",
                    token_name(self.current_kind())
                ));
                None
            }
        }
    }
}
