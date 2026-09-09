//! JSX parsing for DekaScript (Compiler v2).
//!
//! JSX is parsed lazily when the main parser sees `<` in prefix position.
//! The lexer tokenises the structural characters (`<`, `>`, `/`, `{`, `}`,
//! `=`, identifiers, strings) and emits the text between them as a single
//! verbatim `JsxText` token per run (deka#67, deka#68); this module reads
//! those tokens and builds the element tree.

use crate::ast::{self, Expr, JsxAttribute, JsxElement};
use crate::lexer::TokenKind;

use super::{kind_is_name_capable, util::token_name, Parser};

impl<'a> Parser<'a> {
    /// Entry point called from the expression parser when it sees `<`.
    pub(super) fn parse_jsx(&mut self, start: crate::ast::Pos, start_byte: usize) -> Option<Expr<'a>> {
        // Current token must be `<`.
        if !self.at(TokenKind::Lt) {
            return None;
        }

        let element = self.parse_jsx_element_or_fragment(start, start_byte)?;
        Some(element)
    }

    fn parse_jsx_element_or_fragment(
        &mut self,
        start: crate::ast::Pos,
        start_byte: usize,
    ) -> Option<Expr<'a>> {
        let _guard = self.enter_recursion()?;
        self.expect(TokenKind::Lt)?;

        // Fragment: `<>`
        if self.at(TokenKind::Gt) {
            self.advance();
            let children = self.parse_jsx_children()?;
            self.expect_jsx_closing_fragment()?;
            return Some(Expr::JsxFragment {
                children: ast::alloc_slice(self.arena, children),
                span: self.span_from(start, start_byte),
            });
        }

        let tag = self.expect_identifier()?;

        // RFD 8: JSX member expressions (`<My.Component />`) are disallowed.
        if self.at(TokenKind::Dot) {
            self.error("JSX member expressions are not allowed; use a single identifier for the tag (RFD 8)");
            return None;
        }

        let attributes = self.parse_jsx_attributes()?;

        // Self-closing: `<tag ... />`
        if self.eat(TokenKind::Slash) {
            self.expect(TokenKind::Gt)?;
            return Some(Expr::JsxElement {
                element: JsxElement {
                    tag,
                    attributes: ast::alloc_slice(self.arena, attributes),
                    children: &[],
                    span: self.span_from(start, start_byte),
                },
                span: self.span_from(start, start_byte),
            });
        }

        self.expect(TokenKind::Gt)?;
        let children = self.parse_jsx_children()?;
        self.expect_jsx_closing_tag(tag)?;

        Some(Expr::JsxElement {
            element: JsxElement {
                tag,
                attributes: ast::alloc_slice(self.arena, attributes),
                children: ast::alloc_slice(self.arena, children),
                span: self.span_from(start, start_byte),
            },
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_jsx_attributes(&mut self) -> Option<Vec<JsxAttribute<'a>>> {
        let mut attrs = Vec::new();
        while !self.at(TokenKind::Gt)
            && !self.at(TokenKind::Slash)
            && !self.at(TokenKind::Eof)
        {
            self.skip_newlines();
            // Spread attribute: `{...expr}`
            if self.at(TokenKind::LBrace) {
                self.advance();
                self.expect(TokenKind::Spread)?;
                // Consume the expression and closing brace for error recovery.
                let _ = self.parse_expression();
                let _ = self.expect(TokenKind::RBrace);
                self.error("Invalid JSX attribute name '...'.");
                return None;
            }

            let first = self.parse_jsx_attr_name_part()?;
            let name = if self.eat(TokenKind::Colon) {
                let second = self.parse_jsx_attr_name_part()?;
                self.bump_str(&format!("{first}:{second}"))
            } else {
                first
            };
            let attr_start_byte = self.prev.span.byte_start;
            let value = if self.eat(TokenKind::Eq) {
                if self.at(TokenKind::String) {
                    let s = self.bump_str(self.current_text());
                    self.advance();
                    Some(Expr::String {
                        value: s,
                        span: self.current_span(),
                    })
                } else if self.at(TokenKind::LBrace) {
                    self.advance();
                    let expr = self.parse_expression()?;
                    self.expect(TokenKind::RBrace)?;
                    Some(expr)
                } else {
                    self.error("expected string or `{expr}` after JSX attribute `=`");
                    return None;
                }
            } else {
                // Boolean attribute: `<input disabled />` => disabled={true}
                Some(Expr::Boolean {
                    value: true,
                    span: self.prev.span,
                })
            };
            attrs.push(JsxAttribute {
                name,
                value,
                span: self.span_from(self.prev.span.start, attr_start_byte),
            });
        }
        Some(attrs)
    }

    /// One segment of an attribute name: a name plus any number of
    /// `-name` continuations, so every valid HTML attribute name
    /// (`data-*`, `aria-*`, `http-equiv`, ...) is writable (dsc#69). `-` is
    /// not an operator in attribute-name position — subtraction only occurs
    /// inside `{ ... }` attribute values, which parse as ordinary
    /// expressions — so joining here cannot swallow a real operator.
    ///
    /// The leading segment (and each continuation) may be any keyword, not
    /// just an identifier (dsc#77): attribute-name position is neither
    /// statement nor expression position, so a keyword there can only be a
    /// name. This is the same contextual-name rule already used for field
    /// access (`o.type`, dsc#69's precedent), extended to every keyword —
    /// enumerating `type`/`for`/`class` would just miss the next collision.
    /// `super` is included even though `expect_field_name` excludes it:
    /// there it guards a future declaration form, but no declaration can
    /// start after `<tag` or `-`, so the reservation does not apply here.
    /// Keyword-led hyphenation (`for-action="x"`) falls out naturally.
    fn parse_jsx_attr_name_part(&mut self) -> Option<&'a str> {
        let mut name = self.parse_jsx_attr_name_segment()?;
        while self.at(TokenKind::Minus) {
            self.advance();
            let part = self.parse_jsx_attr_name_segment()?;
            name = self.bump_str(&format!("{name}-{part}"));
        }
        Some(name)
    }

    /// A single `-`-free segment of an attribute name: an identifier or any
    /// keyword token (see `parse_jsx_attr_name_part`).
    fn parse_jsx_attr_name_segment(&mut self) -> Option<&'a str> {
        self.skip_newlines();
        let kind = self.current_kind();
        if kind_is_name_capable(kind) || kind == TokenKind::Super {
            let name = self.bump_str(self.current_text());
            self.advance();
            Some(name)
        } else {
            self.error(format!(
                "expected identifier, found `{}`",
                token_name(kind)
            ));
            None
        }
    }

    fn parse_jsx_children(&mut self) -> Option<Vec<Expr<'a>>> {
        let mut children = Vec::new();
        loop {
            if self.at(TokenKind::Lt) {
                // Could be a closing tag `</` or a child element/fragment.
                if self.peek_kind(1) == Some(TokenKind::Slash) {
                    break;
                }
                let child = self.parse_jsx_element_or_fragment(
                    self.current_span().start,
                    self.current_span().byte_start,
                )?;
                children.push(child);
            } else if self.at(TokenKind::LBrace) {
                let child = self.parse_jsx_expression_child()?;
                if let Some(child) = child {
                    children.push(child);
                }
            } else if self.at(TokenKind::JsxText) {
                children.push(Expr::JsxText {
                    value: self.bump_str(self.current_text()),
                    span: self.current_span(),
                });
                self.advance();
            } else if self.at(TokenKind::Eof) {
                break;
            } else {
                // Stray token from malformed input: stop and let the missing
                // closing-tag diagnostic fire rather than looping forever.
                break;
            }
        }
        Some(children)
    }

    fn parse_jsx_expression_child(&mut self) -> Option<Option<Expr<'a>>> {
        self.expect(TokenKind::LBrace)?;
        if self.eat(TokenKind::RBrace) {
            // Empty expression child is ignored.
            return Some(None);
        }
        let expr = self.parse_expression()?;
        self.expect(TokenKind::RBrace)?;
        Some(Some(expr))
    }

    fn expect_jsx_closing_tag(&mut self, expected: &'a str) -> Option<()> {
        self.expect(TokenKind::Lt)?;
        self.expect(TokenKind::Slash)?;
        let name = self.expect_identifier()?;
        if name != expected {
            self.error(format!(
                "expected closing tag `</{}>` but found `</{}>`",
                expected, name
            ));
            return None;
        }
        self.expect(TokenKind::Gt)
    }

    fn expect_jsx_closing_fragment(&mut self) -> Option<()> {
        self.expect(TokenKind::Lt)?;
        self.expect(TokenKind::Slash)?;
        self.expect(TokenKind::Gt)
    }

    pub(super) fn peek_kind(&self, offset: usize) -> Option<TokenKind> {
        self.tokens.get(self.pos + offset).map(|t| t.kind)
    }

    pub(super) fn peek_text(&self, offset: usize) -> Option<&'a str> {
        self.tokens.get(self.pos + offset).map(|t| t.text)
    }
}
