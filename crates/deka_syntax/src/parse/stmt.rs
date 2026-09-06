//! Statement parsing.

use crate::ast::{
    alloc, alloc_slice, EnumCase, ExportDecl, Expr, ForInit, InterfaceMember, NewtypeRepr, Param,
    Pos, Program, StructField, Stmt, TemplatePart, Type, TypeParam,
};
use crate::diagnostics::Diagnostic;
use crate::lexer::TokenKind;

use super::util::token_name;
use super::Parser;

/// User code cannot declare type parameters (deka#561). Generics stay in the
/// compiler, reserved for the builtin containers. The diagnostic teaches the
/// two patterns that cover the need: structural typing for capability, and
/// builtin containers for carrying a value's identity across a boundary.
const USER_TYPE_PARAMS_BANNED: &str = "user code cannot declare type parameters — generics are reserved for the language's own types (Array<T>, Option<T>, Result<T, E>, Promise<T>). For capability, define an interface and take it as a parameter, e.g. `interface Named { name: string }` then `fn greet(x: Named)`. To carry a value's identity, use a builtin container.";

impl<'a> Parser<'a> {
    pub(super) fn parse_program(&mut self) -> Option<Program<'a>> {
        self.skip_newlines();
        let (start, start_byte) = self.span_start();
        let mut statements = Vec::new();

        while !self.at_end() {
            let before = self.pos;
            match self.parse_statement(false) {
                Some(stmt) => statements.push(stmt),
                None => {
                    self.synchronize();
                    // If synchronize() made no progress, force advancement
                    // so we don't loop forever on unexpected tokens.
                    if self.pos == before && !self.at_end() {
                        self.advance();
                    }
                }
            }
            self.skip_newlines();
        }

        let statements = alloc_slice(self.arena, statements);
        let has_top_level_await = program_has_top_level_await(statements);
        Some(Program {
            statements,
            span: self.span_from(start, start_byte),
            has_top_level_await,
        })
    }

    pub(super) fn parse_statement(&mut self, in_block: bool) -> Option<Stmt<'a>> {
        self.skip_newlines();
        let (start, start_byte) = self.span_start();

        if self.eat(TokenKind::Semicolon) {
            return Some(Stmt::Empty {
                span: self.span_from(start, start_byte),
            });
        }

        match self.current_kind() {
            TokenKind::Const | TokenKind::Let => {
                let is_const = self.current_kind() == TokenKind::Const;
                self.advance();

                let name = self.expect_identifier()?;
                let ty = if self.eat(TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.expect(TokenKind::Eq)?;

                // `unwrap(x) or { … }` (deka#445). Recognised positionally so
                // `unwrap` and `or` stay ordinary identifiers everywhere else.
                if self.at_unwrap_binding() {
                    return self.parse_unwrap_binding(name, ty, is_const, start, start_byte, in_block);
                }

                let value = self.parse_expression()?;
                self.expect_statement_end(in_block)?;

                let span = self.span_from(start, start_byte);
                if is_const {
                    Some(Stmt::Const {
                        name,
                        ty,
                        value,
                        span,
                    })
                } else {
                    Some(Stmt::Let {
                        name,
                        ty,
                        value,
                        span,
                    })
                }
            }

            TokenKind::Fn => {
                if in_block {
                    self.error("function declarations are only allowed at the top level in DekaScript");
                    return None;
                }
                self.parse_fn_statement(start, start_byte)
            }

            TokenKind::Async => {
                let next_is_fn = self.tokens.get(self.pos + 1).map(|t| t.kind) == Some(TokenKind::Fn);
                if next_is_fn {
                    if in_block {
                        self.error("function declarations are only allowed at the top level in DekaScript");
                        return None;
                    }
                    self.parse_fn_statement(start, start_byte)
                } else {
                    self.error("expected `fn` after `async`");
                    None
                }
            }

            // `super struct` / `super enum` (rfd#41, deka#561 PR B): marks a
            // declaration whose type information survives to runtime, so
            // `Name.type()` is legal. Anything else after `super` is an error.
            TokenKind::Super => {
                if in_block {
                    self.error("struct and enum declarations are only allowed at the top level in DekaScript");
                    return None;
                }
                let next = self.tokens.get(self.pos + 1).map(|t| t.kind);
                match next {
                    Some(TokenKind::Struct) => {
                        self.advance(); // `super`
                        self.parse_struct_statement(start, start_byte, true)
                    }
                    Some(TokenKind::Enum) => {
                        self.advance(); // `super`
                        self.parse_enum_statement(start, start_byte, true)
                    }
                    _ => {
                        self.error("`super` is only available on `struct` and `enum` declarations (e.g. `super struct User { ... }`)");
                        None
                    }
                }
            }

            TokenKind::For => self.parse_for_statement(start, start_byte),

            TokenKind::If => self.parse_if_statement(start, start_byte),

            TokenKind::LBrace => {
                let body = self.parse_block()?;
                Some(Stmt::Block {
                    body,
                    span: self.span_from(start, start_byte),
                })
            }

            TokenKind::Break => {
                self.advance();
                self.expect_statement_end(in_block)?;
                Some(Stmt::Break {
                    span: self.span_from(start, start_byte),
                })
            }

            TokenKind::Continue => {
                self.advance();
                self.expect_statement_end(in_block)?;
                Some(Stmt::Continue {
                    span: self.span_from(start, start_byte),
                })
            }

            TokenKind::Struct => {
                if in_block {
                    self.error("struct declarations are only allowed at the top level in DekaScript");
                    return None;
                }
                self.parse_struct_statement(start, start_byte, false)
            }

            TokenKind::Enum => {
                if in_block {
                    self.error("enum declarations are only allowed at the top level in DekaScript");
                    return None;
                }
                self.parse_enum_statement(start, start_byte, false)
            }

            TokenKind::Interface => {
                if in_block {
                    self.error("interface declarations are only allowed at the top level in DekaScript");
                    return None;
                }
                self.parse_interface_statement(start, start_byte)
            }

            TokenKind::Type | TokenKind::Alias => {
                self.parse_type_alias_statement(start, start_byte)
            }

            TokenKind::Import => self.parse_import_statement(start, start_byte),

            TokenKind::Export => self.parse_export_statement(start, start_byte),

            TokenKind::Return => {
                self.advance();
                let value =
                    if self.at(TokenKind::Semicolon) || (in_block && self.at(TokenKind::RBrace)) {
                        None
                    } else {
                        Some(self.parse_expression()?)
                    };
                self.expect_statement_end(in_block)?;
                Some(Stmt::Return {
                    value,
                    span: self.span_from(start, start_byte),
                })
            }

            _ => {
                let expr = self.parse_expression()?;
                self.expect_statement_end(in_block)?;
                Some(Stmt::Expr {
                    expr,
                    span: self.span_from(start, start_byte),
                })
            }
        }
    }

    fn parse_fn_statement(&mut self, start: Pos, start_byte: usize) -> Option<Stmt<'a>> {
        let is_async = self.eat(TokenKind::Async);
        self.advance(); // `fn`

        // Receiver method: `fn (p Point) distance<T>(...): Ret { ... }`
        if self.at(TokenKind::LParen) {
            self.advance(); // `(`
            let receiver_name = self.expect_identifier()?;
            let receiver_mutable = self.eat(TokenKind::Mut);
            let receiver_type = self.expect_identifier()?;
            self.expect(TokenKind::RParen)?;

            let name = self.expect_identifier()?;

            if self.at(TokenKind::Lt) {
                self.error(USER_TYPE_PARAMS_BANNED);
                return None;
            }
            let type_params: &[TypeParam] = &[];

            self.expect(TokenKind::LParen)?;
            let params = self.parse_params()?;
            self.expect(TokenKind::RParen)?;

            let return_type = if self.at(TokenKind::LBrace) {
                None
            } else {
                self.reject_return_type_colon();
                Some(self.parse_type()?)
            };

            let body = self.parse_block()?;

            return Some(Stmt::ReceiverMethod {
                receiver_type,
                receiver_name,
                receiver_mutable,
                name,
                type_params,
                params,
                return_type,
                body,
                is_async,
                span: self.span_from(start, start_byte),
            });
        }

        // Regular function: `fn add<T>(...) Ret { ... }`
        let name = self.expect_identifier()?;
        if self.at(TokenKind::Lt) {
            self.error(USER_TYPE_PARAMS_BANNED);
            return None;
        }
        let type_params: &[TypeParam] = &[];

        self.expect(TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(TokenKind::RParen)?;

        let return_type = if self.at(TokenKind::LBrace) {
            None
        } else {
            self.reject_return_type_colon();
            Some(self.parse_type()?)
        };

        let body = self.parse_block()?;

        Some(Stmt::Function {
            name,
            type_params,
            params,
            return_type,
            body,
            is_async,
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_for_statement(&mut self, start: Pos, start_byte: usize) -> Option<Stmt<'a>> {
        self.advance(); // `for`
        self.expect(TokenKind::LParen)?;

        // for-of: `for (const x of iterable) { ... }` or `for (let x of iterable) { ... }`
        if self.at(TokenKind::Const) || self.at(TokenKind::Let) {
            let is_const = self.at(TokenKind::Const);
            self.advance();
            let name = self.expect_identifier()?;
            if self.eat(TokenKind::Of) {
                let iterable = self.parse_expression()?;
                self.expect(TokenKind::RParen)?;
                let body = self.parse_block()?;
                return Some(Stmt::ForOf {
                    name,
                    is_const,
                    iterable,
                    body,
                    span: self.span_from(start, start_byte),
                });
            }
            // Otherwise fall back to C-style for with const/let init.
            self.expect(TokenKind::Eq)?;
            let value = self.parse_expression()?;
            let init = if is_const {
                ForInit::Const { name, value }
            } else {
                ForInit::Let { name, value }
            };
            self.expect(TokenKind::Semicolon)?;
            let condition = if self.at(TokenKind::Semicolon) {
                None
            } else {
                Some(self.parse_expression()?)
            };
            self.expect(TokenKind::Semicolon)?;
            let step = if self.at(TokenKind::RParen) {
                None
            } else {
                Some(self.parse_expression()?)
            };
            self.expect(TokenKind::RParen)?;
            let body = self.parse_block()?;
            return Some(Stmt::For {
                init: Some(init),
                condition,
                step,
                body,
                span: self.span_from(start, start_byte),
            });
        }

        let init = if self.at(TokenKind::Semicolon) {
            None
        } else {
            Some(ForInit::Expr(self.parse_expression()?))
        };

        self.expect(TokenKind::Semicolon)?;

        let condition = if self.at(TokenKind::Semicolon) {
            None
        } else {
            Some(self.parse_expression()?)
        };

        self.expect(TokenKind::Semicolon)?;

        let step = if self.at(TokenKind::RParen) {
            None
        } else {
            Some(self.parse_expression()?)
        };

        self.expect(TokenKind::RParen)?;
        let body = self.parse_block()?;

        Some(Stmt::For {
            init,
            condition,
            step,
            body,
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_if_statement(&mut self, start: Pos, start_byte: usize) -> Option<Stmt<'a>> {
        self.advance(); // `if`
        self.expect(TokenKind::LParen)?;
        let condition = self.parse_expression()?;
        self.expect(TokenKind::RParen)?;
        let then_body = self.parse_block()?;

        let else_body = if self.eat(TokenKind::Else) {
            if self.at(TokenKind::If) {
                let else_start = self.span_start();
                let else_if = self.parse_if_statement(else_start.0, else_start.1)?;
                alloc_slice(self.arena, vec![else_if])
            } else {
                self.parse_block()?
            }
        } else {
            &[]
        };

        Some(Stmt::If {
            condition,
            then_body,
            else_body,
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_struct_statement(&mut self, start: Pos, start_byte: usize, is_super: bool) -> Option<Stmt<'a>> {
        self.advance(); // `struct`

        let name = self.expect_identifier()?;
        if self.at(TokenKind::Lt) {
            self.error(USER_TYPE_PARAMS_BANNED);
            return None;
        }
        let type_params: &[TypeParam] = &[];

        self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        let mut embeds = Vec::new();

        while !self.at(TokenKind::RBrace) && !self.at_end() {
            let (field_start, field_start_byte) = self.span_start();
            let field_name = self.expect_identifier()?;

            // If the identifier is followed by `:` or `?:`, this is a regular field.
            // Otherwise it names an embedded struct (e.g. `struct Outer { Inner }`).
            if self.at(TokenKind::Colon) || self.at(TokenKind::Question) {
                let is_optional = if self.eat(TokenKind::Question) {
                    self.expect(TokenKind::Colon)?;
                    true
                } else {
                    self.expect(TokenKind::Colon)?;
                    false
                };
                let field_type = self.parse_type()?;
                let field_span = field_type.span();
                let field_type = if is_optional {
                    Type::Option {
                        inner: alloc(self.arena, field_type),
                        span: field_span,
                    }
                } else {
                    field_type
                };
                let default_value = if self.eat(TokenKind::Eq) {
                    Some(self.parse_expression()?)
                } else {
                    None
                };
                fields.push(StructField {
                    name: field_name,
                    ty: field_type,
                    default_value,
                    optional: is_optional,
                    span: self.span_from(field_start, field_start_byte),
                });
            } else {
                embeds.push(crate::ast::Embed {
                    name: field_name,
                    span: self.span_from(field_start, field_start_byte),
                });
            }

            if self.at(TokenKind::RBrace) {
                break;
            }
            if self.at(TokenKind::Comma) {
                self.error("Missing semicolon: struct fields must be separated by ';' or a newline");
                return None;
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
            self.error("expected `;` or newline between struct fields");
            break;
        }

        self.skip_newlines();
        self.expect(TokenKind::RBrace)?;

        Some(Stmt::Struct {
            name,
            type_params,
            fields: alloc_slice(self.arena, fields),
            embeds: alloc_slice(self.arena, embeds),
            is_super,
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_enum_statement(&mut self, start: Pos, start_byte: usize, is_super: bool) -> Option<Stmt<'a>> {
        self.advance(); // `enum`

        let name = self.expect_identifier()?;
        if self.at(TokenKind::Lt) {
            self.error(USER_TYPE_PARAMS_BANNED);
            return None;
        }
        let type_params: &[TypeParam] = &[];

        self.expect(TokenKind::LBrace)?;
        let mut cases = Vec::new();

        while !self.at(TokenKind::RBrace) && !self.at_end() {
            let (case_start, case_start_byte) = self.span_start();
            let case_name = self.expect_identifier()?;
            let payload = if self.eat(TokenKind::LParen) {
                let ty = self.parse_type()?;
                self.expect(TokenKind::RParen)?;
                Some(ty)
            } else {
                None
            };
            cases.push(EnumCase {
                name: case_name,
                payload,
                span: self.span_from(case_start, case_start_byte),
            });

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
            // Allow space-separated enum cases for v1 parity:
            // enum Color { Red Green Blue }
            if self.at(TokenKind::Identifier) {
                continue;
            }
            self.error("expected `,` or newline between enum cases");
            break;
        }

        self.skip_newlines();
        self.expect(TokenKind::RBrace)?;

        Some(Stmt::Enum {
            name,
            type_params,
            cases: alloc_slice(self.arena, cases),
            is_super,
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_interface_statement(&mut self, start: Pos, start_byte: usize) -> Option<Stmt<'a>> {
        self.advance(); // `interface`

        let name = self.expect_identifier()?;
        if self.at(TokenKind::Lt) {
            self.error(USER_TYPE_PARAMS_BANNED);
            return None;
        }
        let type_params: &[TypeParam] = &[];

        self.expect(TokenKind::LBrace)?;
        let mut members = Vec::new();

        while !self.at(TokenKind::RBrace) && !self.at_end() {
            self.skip_newlines();
            if self.at(TokenKind::RBrace) {
                break;
            }

            let (member_start, member_start_byte) = self.span_start();

            // Optional `mut` for mutable fields.
            let mutable = self.eat(TokenKind::Mut);

            if self.at(TokenKind::Fn) {
                // Method signature: fn name(params) Ret
                self.advance(); // `fn`
                let method_name = self.expect_identifier()?;
                self.expect(TokenKind::LParen)?;
                let params = self.parse_params()?;
                self.expect(TokenKind::RParen)?;
                let return_type = if !self.at(TokenKind::Semicolon)
                    && !self.at(TokenKind::Newline)
                    && !self.at(TokenKind::RBrace)
                    && !self.at(TokenKind::Comma)
                {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                members.push(InterfaceMember::Method {
                    name: method_name,
                    params: alloc_slice(self.arena, params.to_vec()),
                    return_type,
                    mutable,
                    span: self.span_from(member_start, member_start_byte),
                });
            } else {
                // Field declaration.
                let field_name = self.expect_identifier()?;
                let optional = self.eat(TokenKind::Question);
                self.expect(TokenKind::Colon)?;
                let field_type = self.parse_type()?;
                members.push(InterfaceMember::Field {
                    name: field_name,
                    ty: field_type,
                    mutable,
                    optional,
                    span: self.span_from(member_start, member_start_byte),
                });
            }

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
            self.error("expected `,` or newline between interface members");
            break;
        }

        self.skip_newlines();
        self.expect(TokenKind::RBrace)?;

        Some(Stmt::Interface {
            name,
            type_params,
            members: alloc_slice(self.arena, members),
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_type_alias_statement(&mut self, start: Pos, start_byte: usize) -> Option<Stmt<'a>> {
        let keyword = self.current_kind();
        self.advance(); // `type` or `alias`

        let name = self.expect_identifier()?;

        if self.at(TokenKind::Lt) {
            self.error(USER_TYPE_PARAMS_BANNED);
            return None;
        }

        // Newtype: `type Name Repr` (no `=`).
        if keyword == TokenKind::Type && !self.at(TokenKind::Eq) {
            let repr = self.parse_newtype_repr()?;
            self.expect_statement_end(false)?;
            return Some(Stmt::Newtype {
                name,
                repr,
                span: self.span_from(start, start_byte),
            });
        }

        if keyword == TokenKind::Type {
            self.errors.push(
                Diagnostic::warning(
                    start.line,
                    start.column,
                    "`type X = Y` is deprecated; use `alias X = Y` instead",
                )
                .with_help("replace `type` with `alias`"),
            );
        }

        if self.at(TokenKind::Lt) {
            self.error(USER_TYPE_PARAMS_BANNED);
            return None;
        }
        let type_params: &[TypeParam] = &[];

        self.expect(TokenKind::Eq)?;
        let value = self.parse_type()?;
        self.expect_statement_end(false)?;

        Some(Stmt::TypeAlias {
            name,
            type_params,
            value,
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_newtype_repr(&mut self) -> Option<NewtypeRepr> {
        let repr_name = self.expect_identifier()?;
        Some(match repr_name {
            "number" => NewtypeRepr::Number,
            "string" => NewtypeRepr::String,
            "bool" => NewtypeRepr::Bool,
            _ => {
                self.error(format!(
                    "newtype representation must be `number`, `string`, or `bool`, found `{repr_name}`"
                ));
                NewtypeRepr::Number
            }
        })
    }

    fn parse_import_statement(&mut self, start: Pos, start_byte: usize) -> Option<Stmt<'a>> {
        self.advance(); // `import`

        // Side-effect import: `import "./mod.ds";`
        if self.at(TokenKind::String) {
            let source = self.bump_str(self.current_text());
            self.advance();
            self.expect_statement_end(false)?;
            return Some(Stmt::Import {
                specifiers: alloc_slice(self.arena, Vec::new()),
                source,
                span: self.span_from(start, start_byte),
            });
        }

        self.expect(TokenKind::LBrace)?;
        let mut specs = Vec::new();
        if !self.at(TokenKind::RBrace) {
            loop {
                let (spec_start, spec_start_byte) = self.span_start();
                let imported = self.expect_identifier()?;
                let local = if self.eat(TokenKind::As) {
                    self.expect_identifier()?
                } else {
                    imported
                };
                specs.push(crate::ast::ImportSpec {
                    imported,
                    local,
                    span: self.span_from(spec_start, spec_start_byte),
                });
                if !self.eat(TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBrace)?;
        self.expect(TokenKind::From)?;

        if !self.at(TokenKind::String) {
            self.error(format!(
                "expected module path string, found `{}`",
                token_name(self.current_kind())
            ));
            return None;
        }
        let source = self.bump_str(self.current_text());
        self.advance();
        self.expect_statement_end(false)?;

        Some(Stmt::Import {
            specifiers: alloc_slice(self.arena, specs),
            source,
            span: self.span_from(start, start_byte),
        })
    }

    fn parse_export_statement(&mut self, start: Pos, start_byte: usize) -> Option<Stmt<'a>> {
        self.advance(); // `export`

        match self.current_kind() {
            TokenKind::Const => {
                self.advance();

                let name = self.expect_identifier()?;
                let ty = if self.eat(TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.expect(TokenKind::Eq)?;
                let value = self.parse_expression()?;
                self.expect_statement_end(false)?;

                let span = self.span_from(start, start_byte);
                let decl = crate::ast::ExportDecl::Const { name, ty, value };
                Some(Stmt::Export { decl, span })
            }
            // `parse_fn_statement` already consumes an optional `async`, so the
            // async form needs no separate parse — only a way to reach it from
            // here. `export async fn` was rejected outright before (deka#410);
            // `@deka/fs` is written that way and could not be compiled at all.
            TokenKind::Fn | TokenKind::Async => {
                if self.current_kind() == TokenKind::Async {
                    let next_is_fn =
                        self.tokens.get(self.pos + 1).map(|t| t.kind) == Some(TokenKind::Fn);
                    if !next_is_fn {
                        self.error("expected `fn` after `async`");
                        return None;
                    }
                }
                let fn_stmt = self.parse_fn_statement(start, start_byte)?;
                let span = self.span_from(start, start_byte);
                let decl = match fn_stmt {
                    Stmt::Function {
                        name,
                        type_params,
                        params,
                        return_type,
                        body,
                        is_async,
                        ..
                    } => crate::ast::ExportDecl::Function {
                        name,
                        type_params,
                        params,
                        return_type,
                        body,
                        is_async,
                    },
                    Stmt::ReceiverMethod { .. } => {
                        self.error("cannot export a receiver method");
                        return None;
                    }
                    _ => unreachable!(),
                };
                Some(Stmt::Export { decl, span })
            }
            TokenKind::Identifier if self.current_text() == "default" => {
                self.error("unsupported export syntax: default exports are not allowed");
                None
            }
            TokenKind::LBrace => {
                self.advance(); // `{`
                let mut names = Vec::new();
                if !self.at(TokenKind::RBrace) {
                    loop {
                        let (name_start, name_start_byte) = self.span_start();
                        let name = self.expect_identifier()?;
                        let alias = if self.eat(TokenKind::As) {
                            Some(self.expect_identifier()?)
                        } else {
                            None
                        };
                        names.push(crate::ast::ExportName {
                            name,
                            alias,
                            span: self.span_from(name_start, name_start_byte),
                        });
                        if !self.eat(TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RBrace)?;
                let source = if self.eat(TokenKind::From) {
                    if !self.at(TokenKind::String) {
                        self.error(format!("expected module path string, found `{}`", token_name(self.current_kind())));
                        return None;
                    }
                    let source = self.bump_str(self.current_text());
                    self.advance();
                    Some(source)
                } else { None };
                self.expect_statement_end(false)?;
                Some(Stmt::Export {
                    decl: crate::ast::ExportDecl::NamedGroup {
                        names: alloc_slice(self.arena, names),
                        source,
                    },
                    span: self.span_from(start, start_byte),
                })
            }
            _ => {
                self.error(format!(
                    "expected `const`, `fn`, `async fn`, `{{` or `default` after `export`, found `{}`",
                    token_name(self.current_kind())
                ));
                None
            }
        }
    }

    /// `unwrap` `(` … `)` `or` — the start of an unwrap binding.
    ///
    /// The `or` is part of the recognition, not just the grammar that follows.
    /// `unwrap` stays an ordinary identifier, so a program that defines its own
    /// `unwrap` function keeps working: `const x = unwrap(b)` with no `or` is
    /// that call, and this returns false for it.
    fn at_unwrap_binding(&self) -> bool {
        if self.current_kind() != TokenKind::Identifier
            || self.current_text() != "unwrap"
            || self.peek_kind(1) != Some(TokenKind::LParen)
        {
            return false;
        }
        // Scan to the `(`'s partner, then look one past it.
        let mut depth = 0usize;
        let mut offset = 1usize;
        loop {
            match self.peek_kind(offset) {
                Some(TokenKind::LParen) => depth += 1,
                Some(TokenKind::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return self.peek_kind(offset + 1) == Some(TokenKind::Identifier)
                            && self.peek_text(offset + 1) == Some("or");
                    }
                }
                Some(TokenKind::Eof) | None => return false,
                _ => {}
            }
            offset += 1;
        }
    }

    /// `let name = unwrap(scrutinee) or { … }`.
    fn parse_unwrap_binding(
        &mut self,
        name: &'a str,
        ty: Option<crate::ast::Type<'a>>,
        is_const: bool,
        start: crate::ast::Pos,
        start_byte: usize,
        in_block: bool,
    ) -> Option<Stmt<'a>> {
        self.advance(); // `unwrap`
        self.expect(TokenKind::LParen)?;
        let scrutinee = self.parse_expression()?;
        self.expect(TokenKind::RParen)?;

        // `at_unwrap_binding` already established this.
        self.advance(); // `or`

        let alternative = if self.at(TokenKind::Match) {
            self.advance();
            crate::ast::UnwrapAlternative::Match(self.parse_match_arms()?)
        } else {
            crate::ast::UnwrapAlternative::Block(self.parse_block()?)
        };
        self.expect_statement_end(in_block)?;

        Some(Stmt::UnwrapLet {
            name,
            ty,
            is_const,
            scrutinee,
            alternative,
            span: self.span_from(start, start_byte),
        })
    }

    pub(super) fn parse_block(&mut self) -> Option<&'a [Stmt<'a>]> {
        self.expect(TokenKind::LBrace)?;
        let mut statements = Vec::new();

        self.skip_newlines();
        while !self.at(TokenKind::RBrace) && !self.at_end() {
            let before = self.pos;
            match self.parse_statement(true) {
                Some(stmt) => statements.push(stmt),
                None => {
                    self.synchronize();
                    if self.pos == before && !self.at_end() {
                        self.advance();
                    }
                }
            }
            self.skip_newlines();
        }

        self.expect(TokenKind::RBrace)?;
        Some(alloc_slice(self.arena, statements))
    }

    pub(super) fn parse_params(&mut self) -> Option<&'a [Param<'a>]> {
        let mut params = Vec::new();

        if !self.at(TokenKind::RParen) {
            loop {
                let (param_start, param_start_byte) = self.span_start();
                let name = self.expect_identifier()?;
                let ty = if self.eat(TokenKind::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let default_value = if self.eat(TokenKind::Eq) {
                    Some(self.parse_expression()?)
                } else {
                    None
                };
                params.push(Param {
                    name,
                    ty,
                    default_value,
                    span: self.span_from(param_start, param_start_byte),
                });

                if !self.eat(TokenKind::Comma) {
                    break;
                }
                self.skip_newlines();
            }
        }

        self.skip_newlines();
        Some(alloc_slice(self.arena, params))
    }

    fn expect_statement_end(&mut self, in_block: bool) -> Option<()> {
        if self.eat(TokenKind::Semicolon) {
            Some(())
        } else if in_block && self.at(TokenKind::RBrace) {
            // Optional semicolon before a closing brace.
            Some(())
        } else if self.at(TokenKind::Newline) || self.at(TokenKind::Eof) {
            // Optional semicolon: a newline or end-of-file terminates the
            // statement. Consume any following newlines as well.
            self.skip_newlines();
            Some(())
        } else if self.at(TokenKind::Bar) {
            // `|` only separates or-pattern alternatives (deka#446).
            // DekaScript has no bitwise or, and a bare `|` used to be a lex
            // error carrying this hint -- keep it now that the token is real.
            self.error("unexpected `|`; did you mean `||` or `|>`?");
            None
        } else {
            self.error(format!(
                "expected `;` or newline, found `{}`",
                token_name(self.current_kind())
            ));
            None
        }
    }
}

/// True when any statement at the top level of the program contains an
/// `await` expression outside of a function or closure body.
pub(crate) fn program_has_top_level_await(statements: &[Stmt<'_>]) -> bool {
    statements.iter().any(|stmt| stmt_has_top_level_await(stmt))
}

fn stmt_has_top_level_await(stmt: &Stmt<'_>) -> bool {
    match stmt {
        Stmt::Const { value, .. }
        | Stmt::Let { value, .. }
        | Stmt::Expr { expr: value, .. } => expr_has_top_level_await(value),
        Stmt::UnwrapLet {
            scrutinee,
            alternative,
            ..
        } => {
            expr_has_top_level_await(scrutinee)
                || match alternative {
                    crate::ast::UnwrapAlternative::Block(stmts) => {
                        stmts.iter().any(stmt_has_top_level_await)
                    }
                    crate::ast::UnwrapAlternative::Match(arms) => {
                        arms.iter().any(|arm| expr_has_top_level_await(&arm.body))
                    }
                }
        }
        Stmt::Return { value: Some(value), .. } => expr_has_top_level_await(value),
        Stmt::Return { value: None, .. } => false,
        // Top-level function declarations are boundaries: await inside them is
        // not top-level await.
        Stmt::Function { .. } | Stmt::ReceiverMethod { .. } => false,
        Stmt::Export { decl, .. } => match decl {
            ExportDecl::Const { value, .. } => expr_has_top_level_await(value),
            ExportDecl::Function { .. } | ExportDecl::NamedGroup { .. } => false,
        },
        Stmt::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            expr_has_top_level_await(condition)
                || then_body.iter().any(|s| stmt_has_top_level_await(s))
                || else_body.iter().any(|s| stmt_has_top_level_await(s))
        }
        Stmt::Block { body, .. } => body.iter().any(|s| stmt_has_top_level_await(s)),
        Stmt::For {
            init,
            condition,
            step,
            body,
            ..
        } => {
            init.as_ref().map_or(false, |i| match i {
                ForInit::Const { value, .. }
                | ForInit::Let { value, .. }
                | ForInit::Expr(value) => expr_has_top_level_await(value),
            })
                || condition.as_ref().map_or(false, |e| expr_has_top_level_await(e))
                || step.as_ref().map_or(false, |e| expr_has_top_level_await(e))
                || body.iter().any(|s| stmt_has_top_level_await(s))
        }
        Stmt::ForOf { iterable, body, .. } => {
            expr_has_top_level_await(iterable) || body.iter().any(|s| stmt_has_top_level_await(s))
        }
        Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Empty { .. } => false,
        Stmt::Struct { .. }
        | Stmt::Enum { .. }
        | Stmt::TypeAlias { .. }
        | Stmt::Newtype { .. }
        | Stmt::Interface { .. }
        | Stmt::Import { .. } => false,
    }
}

fn expr_has_top_level_await(expr: &Expr<'_>) -> bool {
    match expr {
        Expr::Await { .. } => true,
        // Closures are function boundaries.
        Expr::Function { .. } => false,
        Expr::Binary { left, right, .. } => {
            expr_has_top_level_await(left) || expr_has_top_level_await(right)
        }
        Expr::Unary { operand, .. } => expr_has_top_level_await(operand),
        Expr::Call { callee, args, .. } => {
            expr_has_top_level_await(callee)
                || args.iter().any(|a| expr_has_top_level_await(a))
        }
        Expr::FieldAccess { object, .. }
        | Expr::IndexAccess { object, .. }
        | Expr::Paren { expr: object, .. }
        | Expr::Spread { expr: object, .. } => expr_has_top_level_await(object),
        Expr::StructLiteral { fields, .. } => {
            fields.iter().any(|f| expr_has_top_level_await(&f.value))
        }
        Expr::EnumConstructor { payload, .. } => {
            payload.as_ref().map_or(false, |p| expr_has_top_level_await(p))
        }
        Expr::Match { scrutinee, arms, .. } => {
            expr_has_top_level_await(scrutinee)
                || arms.iter().any(|arm| {
                    arm.guard
                        .as_ref()
                        .map_or(false, |g| expr_has_top_level_await(g))
                        || expr_has_top_level_await(&arm.body)
                })
        }
        Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_has_top_level_await(condition)
                || expr_has_top_level_await(then_branch)
                || expr_has_top_level_await(else_branch)
        }
        Expr::Array { elements, .. } => {
            elements.iter().any(|e| expr_has_top_level_await(e))
        }
        Expr::Object { fields, .. } => {
            fields.iter().any(|f| expr_has_top_level_await(&f.value))
        }
        Expr::TemplateLiteral { parts, .. } => parts.iter().any(|p| match p {
            TemplatePart::Text(_) => false,
            TemplatePart::Expr(e) => expr_has_top_level_await(e),
        }),
        Expr::Unsafe { .. } | Expr::Bridge { .. } => false,
        Expr::JsxElement { element, .. } => {
            element.attributes.iter().any(|attr| {
                attr.value
                    .as_ref()
                    .map_or(false, |v| expr_has_top_level_await(v))
            }) || element.children.iter().any(|c| expr_has_top_level_await(c))
        }
        Expr::JsxFragment { children, .. } => {
            children.iter().any(|c| expr_has_top_level_await(c))
        }
        Expr::JsxText { .. }
        | Expr::Number { .. }
        | Expr::BigInt { .. }
        | Expr::String { .. }
        | Expr::Boolean { .. }
        | Expr::None { .. }
        | Expr::Identifier { .. } => false,
    }
}
