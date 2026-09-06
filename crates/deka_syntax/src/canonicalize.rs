//! AST resolution passes.
//!
//! This module runs between parsing and typechecking to resolve syntactic
//! ambiguities that cannot be decided by the parser alone.

use std::collections::{HashMap, HashSet};

use bumpalo::Bump;

use crate::ast;
use crate::ast::{Expr, MethodTarget, Program, Stmt};
use crate::typeck::ModuleExports;

/// Rewrite enum member access into explicit enum constructor expressions.
///
/// The parser cannot distinguish `Color.Red` (enum constructor) from
/// `value.field` (field access), because it does not know which identifiers
/// name enums. This pass runs after the whole program is parsed, collects
/// enum declarations, and resugars:
///
/// - `EnumName.Case` -> `EnumConstructor { enum_name, case_name, payload: None }`
/// - `EnumName.Case(payload)` -> `EnumConstructor { enum_name, case_name, payload: Some(payload) }`
///
/// Prelude enums (`Option`, `Result`) are included so `Option.Some(5)` and
/// `Result.Ok(x)` work as well as the bare `Some(5)` / `Ok(x)` shorthand.
pub fn resolve_enum_constructors<'a>(program: &mut Program<'a>, arena: &'a Bump) {
    let mut enums: HashMap<&'a str, HashSet<&'a str>> = HashMap::new();

    // Collect user-defined enums.
    for stmt in program.statements.iter() {
        if let Stmt::Enum { name, cases, .. } = stmt {
            let set: HashSet<&'a str> = cases.iter().map(|c| c.name).collect();
            enums.insert(name, set);
        }
    }

    // Seed prelude enums.
    let mut option = HashSet::new();
    option.insert("Some");
    option.insert("None");
    enums.insert("Option", option);

    let mut result = HashSet::new();
    result.insert("Ok");
    result.insert("Err");
    enums.insert("Result", result);

    let transformed: Vec<Stmt<'a>> = program
        .statements
        .iter()
        .map(|stmt| transform_stmt(stmt, arena, &enums))
        .collect();

    program.statements = ast::alloc_slice(arena, transformed);
}

/// Re-resolve enum constructors after imports are known.
///
/// The parser's initial canonicalization only sees enums declared in the same
/// file, so `Color.Red` where `Color` is imported parses as a field access.
/// Call this after parsing and after resolving the import graph to resugar
/// imported enum constructors before typechecking.
pub fn resolve_imported_enum_constructors<'a>(
    program: &mut Program<'a>,
    arena: &'a Bump,
    imports: &HashMap<&str, &ModuleExports<'a>>,
) {
    let mut enums: HashMap<&'a str, HashSet<&'a str>> = HashMap::new();

    // Collect user-defined enums in the current file.
    for stmt in program.statements.iter() {
        if let Stmt::Enum { name, cases, .. } = stmt {
            let set: HashSet<&'a str> = cases.iter().map(|c| c.name).collect();
            enums.insert(name, set);
        }
    }

    // Seed prelude enums.
    let mut option = HashSet::new();
    option.insert("Some");
    option.insert("None");
    enums.insert("Option", option);

    let mut result = HashSet::new();
    result.insert("Ok");
    result.insert("Err");
    enums.insert("Result", result);

    // Add enum names/cases re-exported by imported modules.
    for exports in imports.values() {
        for (name, info) in exports.enums.iter() {
            let set: HashSet<&'a str> = info.cases.iter().map(|c| c.name).collect();
            enums.insert(name, set);
        }
    }

    let transformed: Vec<Stmt<'a>> = program
        .statements
        .iter()
        .map(|stmt| transform_stmt(stmt, arena, &enums))
        .collect();

    program.statements = ast::alloc_slice(arena, transformed);
}

fn transform_stmt<'a>(
    stmt: &'a Stmt<'a>,
    arena: &'a Bump,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> Stmt<'a> {
    match stmt {
        Stmt::UnwrapLet {
            name,
            ty,
            is_const,
            scrutinee,
            alternative,
            span,
        } => Stmt::UnwrapLet {
            name,
            ty: ty.clone(),
            is_const: *is_const,
            scrutinee: transform_expr(scrutinee, arena, enums).clone(),
            alternative: match alternative {
                ast::UnwrapAlternative::Block(stmts) => ast::UnwrapAlternative::Block(
                    ast::alloc_slice(
                        arena,
                        stmts
                            .iter()
                            .map(|inner| transform_stmt(inner, arena, enums))
                            .collect::<Vec<_>>(),
                    ),
                ),
                ast::UnwrapAlternative::Match(arms) => ast::UnwrapAlternative::Match(
                    ast::alloc_slice(
                        arena,
                        arms.iter()
                            .map(|arm| ast::MatchArm {
                                pattern: arm.pattern.clone(),
                                guard: arm.guard.clone(),
                                body: transform_expr(&arm.body, arena, enums).clone(),
                                span: arm.span,
                            })
                            .collect::<Vec<_>>(),
                    ),
                ),
            },
            span: *span,
        },
        Stmt::Export { decl, span } => {
            let new_decl = match decl {
                ast::ExportDecl::Const { name, ty, value } => {
                    let new_value = transform_expr(value, arena, enums);
                    ast::ExportDecl::Const {
                        name,
                        ty: ty.clone(),
                        value: new_value.clone(),
                    }
                }
                ast::ExportDecl::Function {
                    name,
                    type_params,
                    params,
                    return_type,
                    body,
                    is_async,
                } => {
                    let new_body: Vec<Stmt<'a>> = body
                        .iter()
                        .map(|s| transform_stmt(s, arena, enums))
                        .collect();
                    ast::ExportDecl::Function {
                        name,
                        type_params,
                        params,
                        return_type: return_type.clone(),
                        body: ast::alloc_slice(arena, new_body),
                        is_async: *is_async,
                    }
                }
                ast::ExportDecl::NamedGroup { .. } => decl.clone(),
            };
            Stmt::Export {
                decl: new_decl,
                span: *span,
            }
        }
        Stmt::Import { .. } => return stmt.clone(),
        Stmt::Const {
            name,
            ty,
            value,
            span,
        } => Stmt::Const {
            name,
            ty: ty.clone(),
            value: transform_expr(value, arena, enums).clone(),
            span: *span,
        },
        Stmt::Let {
            name,
            ty,
            value,
            span,
        } => Stmt::Let {
            name,
            ty: ty.clone(),
            value: transform_expr(value, arena, enums).clone(),
            span: *span,
        },
        Stmt::Function {
            name,
            type_params,
            params,
            return_type,
            body,
            is_async,
            span,
        } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| transform_stmt(s, arena, enums))
                .collect();
            Stmt::Function {
                name,
                type_params,
                params,
                return_type: return_type.clone(),
                body: ast::alloc_slice(arena, new_body),
                is_async: *is_async,
                span: *span,
            }
        }
        Stmt::ReceiverMethod {
            receiver_type,
            receiver_name,
            receiver_mutable,
            name,
            type_params,
            params,
            return_type,
            body,
            is_async,
            span,
        } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| transform_stmt(s, arena, enums))
                .collect();
            Stmt::ReceiverMethod {
                receiver_type,
                receiver_name,
                receiver_mutable: *receiver_mutable,
                name,
                type_params,
                params,
                return_type: return_type.clone(),
                body: ast::alloc_slice(arena, new_body),
                is_async: *is_async,
                span: *span,
            }
        }
        Stmt::Struct {
            name,
            type_params,
            fields,
            embeds,
            is_super,
            span,
        } => {
            let new_fields: Vec<ast::StructField<'a>> = fields
                .iter()
                .map(|f| ast::StructField {
                    name: f.name,
                    ty: f.ty.clone(),
                    default_value: f
                        .default_value
                        .as_ref()
                        .map(|v| transform_expr(v, arena, enums).clone()),
                    optional: f.optional,
                    span: f.span,
                })
                .collect();
            Stmt::Struct {
                name,
                type_params,
                fields: ast::alloc_slice(arena, new_fields),
                embeds,
                is_super: *is_super,
                span: *span,
            }
        }
        Stmt::Empty { .. }
        | Stmt::Enum { .. }
        | Stmt::TypeAlias { .. }
        | Stmt::Interface { .. } => return stmt.clone(),
        Stmt::Break { .. } | Stmt::Continue { .. } => return stmt.clone(),
        Stmt::Expr { expr, span } => Stmt::Expr {
            expr: transform_expr(expr, arena, enums).clone(),
            span: *span,
        },
        Stmt::Return { value, span } => Stmt::Return {
            value: value.as_ref().map(|v| transform_expr(v, arena, enums).clone()),
            span: *span,
        },
        Stmt::If {
            condition,
            then_body,
            else_body,
            span,
        } => {
            let new_then: Vec<Stmt<'a>> = then_body
                .iter()
                .map(|s| transform_stmt(s, arena, enums))
                .collect();
            let new_else: Vec<Stmt<'a>> = else_body
                .iter()
                .map(|s| transform_stmt(s, arena, enums))
                .collect();
            Stmt::If {
                condition: transform_expr(condition, arena, enums).clone(),
                then_body: ast::alloc_slice(arena, new_then),
                else_body: ast::alloc_slice(arena, new_else),
                span: *span,
            }
        }
        Stmt::Block { body, span } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| transform_stmt(s, arena, enums))
                .collect();
            Stmt::Block {
                body: ast::alloc_slice(arena, new_body),
                span: *span,
            }
        }
        Stmt::ForOf {
            name,
            is_const,
            iterable,
            body,
            span,
        } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| transform_stmt(s, arena, enums))
                .collect();
            Stmt::ForOf {
                name,
                is_const: *is_const,
                iterable: transform_expr(iterable, arena, enums).clone(),
                body: ast::alloc_slice(arena, new_body),
                span: *span,
            }
        }
        Stmt::For {
            init,
            condition,
            step,
            body,
            span,
        } => {
            let new_init = init.as_ref().map(|i| match i {
                ast::ForInit::Const { name, value } => ast::ForInit::Const {
                    name,
                    value: transform_expr(value, arena, enums).clone(),
                },
                ast::ForInit::Let { name, value } => ast::ForInit::Let {
                    name,
                    value: transform_expr(value, arena, enums).clone(),
                },
                ast::ForInit::Expr(expr) => {
                    ast::ForInit::Expr(transform_expr(expr, arena, enums).clone())
                }
            });
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| transform_stmt(s, arena, enums))
                .collect();
            Stmt::For {
                init: new_init,
                condition: condition
                    .as_ref()
                    .map(|c| transform_expr(c, arena, enums).clone()),
                step: step.as_ref().map(|s| transform_expr(s, arena, enums).clone()),
                body: ast::alloc_slice(arena, new_body),
                span: *span,
            }
        }
        Stmt::Newtype { name, repr, span } => Stmt::Newtype {
            name,
            repr: *repr,
            span: *span,
        },
    }
}

fn transform_expr<'a>(
    expr: &'a Expr<'a>,
    arena: &'a Bump,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> &'a Expr<'a> {
    if let Some(resolved) = try_resolve_enum_expr(expr, enums) {
        return alloc_expr(arena, resolved);
    }

    let new_expr = match expr {
        Expr::Number { .. }
        | Expr::BigInt { .. }
        | Expr::String { .. }
        | Expr::Boolean { .. }
        | Expr::None { .. }
        | Expr::Identifier { .. } => return expr,

        Expr::Binary { op, left, right, span } => Expr::Binary {
            op: *op,
            left: transform_expr(left, arena, enums),
            right: transform_expr(right, arena, enums),
            span: *span,
        },
        Expr::Unary { op, operand, span } => Expr::Unary {
            op: *op,
            operand: transform_expr(operand, arena, enums),
            span: *span,
        },
        Expr::Call {
            callee,
            type_args,
            args,
            span,
        } => Expr::Call {
            callee: transform_expr(callee, arena, enums),
            type_args,
            args: transform_exprs(args, arena, enums),
            span: *span,
        },
        Expr::FieldAccess { object, field, span } => Expr::FieldAccess {
            object: transform_expr(object, arena, enums),
            field,
            span: *span,
        },
        Expr::IndexAccess { object, index, span } => Expr::IndexAccess {
            object: transform_expr(object, arena, enums),
            index: transform_expr(index, arena, enums),
            span: *span,
        },
        Expr::StructLiteral { name, fields, span } => Expr::StructLiteral {
            name,
            fields: transform_struct_fields(fields, arena, enums),
            span: *span,
        },
        Expr::EnumConstructor {
            enum_name,
            case_name,
            payload,
            span,
        } => Expr::EnumConstructor {
            enum_name,
            case_name,
            payload: payload
                .as_ref()
                .map(|p| transform_expr(p, arena, enums) as &'a Expr<'a>),
            span: *span,
        },
        Expr::Match {
            scrutinee,
            arms,
            span,
        } => Expr::Match {
            scrutinee: transform_expr(scrutinee, arena, enums),
            arms: transform_match_arms(arms, arena, enums),
            span: *span,
        },
        Expr::Unsafe {
            source,
            result_type,
            span,
        } => Expr::Unsafe {
            source,
            result_type: result_type.clone(),
            span: *span,
        },
        Expr::Bridge {
            kind,
            action,
            args,
            span,
        } => Expr::Bridge {
            kind,
            action,
            args: transform_exprs(args, arena, enums),
            span: *span,
        },
        Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            span,
        } => Expr::Ternary {
            condition: transform_expr(condition, arena, enums),
            then_branch: transform_expr(then_branch, arena, enums),
            else_branch: transform_expr(else_branch, arena, enums),
            span: *span,
        },
        Expr::Await { expr, span } => Expr::Await {
            expr: transform_expr(expr, arena, enums),
            span: *span,
        },
        Expr::JsxElement { element, span } => Expr::JsxElement {
            element: transform_jsx_element(element, arena, enums),
            span: *span,
        },
        Expr::JsxFragment { children, span } => Expr::JsxFragment {
            children: transform_exprs(children, arena, enums),
            span: *span,
        },
        Expr::JsxText { value, span } => Expr::JsxText { value, span: *span },
        Expr::Array { elements, span } => Expr::Array {
            elements: transform_exprs(elements, arena, enums),
            span: *span,
        },
        Expr::Object { fields, span } => Expr::Object {
            fields: transform_object_fields(fields, arena, enums),
            span: *span,
        },
        Expr::Spread { expr, span } => Expr::Spread {
            expr: transform_expr(expr, arena, enums),
            span: *span,
        },
        Expr::Paren { expr, span } => Expr::Paren {
            expr: transform_expr(expr, arena, enums),
            span: *span,
        },
        Expr::TemplateLiteral { parts, span } => Expr::TemplateLiteral {
            parts: transform_template_parts(parts, arena, enums),
            span: *span,
        },
        Expr::Function {
            params,
            return_type,
            body,
            is_async,
            span,
        } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| transform_stmt(s, arena, enums))
                .collect();
            Expr::Function {
                params,
                return_type: return_type.clone(),
                body: ast::alloc_slice(arena, new_body),
                is_async: *is_async,
                span: *span,
            }
        }
    };

    alloc_expr(arena, new_expr)
}

fn transform_template_parts<'a>(
    parts: &'a [ast::TemplatePart<'a>],
    arena: &'a Bump,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> &'a [ast::TemplatePart<'a>] {
    let transformed: Vec<ast::TemplatePart<'a>> = parts
        .iter()
        .map(|part| match part {
            ast::TemplatePart::Text(text) => ast::TemplatePart::Text(text),
            ast::TemplatePart::Expr(expr) => {
                ast::TemplatePart::Expr(transform_expr(expr, arena, enums))
            }
        })
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn transform_exprs<'a>(
    exprs: &'a [Expr<'a>],
    arena: &'a Bump,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> &'a [Expr<'a>] {
    let transformed: Vec<Expr<'a>> = exprs
        .iter()
        .map(|e| transform_expr(e, arena, enums).clone())
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn transform_struct_fields<'a>(
    fields: &'a [ast::StructLiteralField<'a>],
    arena: &'a Bump,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> &'a [ast::StructLiteralField<'a>] {
    let transformed: Vec<ast::StructLiteralField<'a>> = fields
        .iter()
        .map(|f| ast::StructLiteralField {
            name: f.name,
            value: transform_expr(&f.value, arena, enums).clone(),
            span: f.span,
        })
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn transform_object_fields<'a>(
    fields: &'a [ast::ObjectField<'a>],
    arena: &'a Bump,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> &'a [ast::ObjectField<'a>] {
    let transformed: Vec<ast::ObjectField<'a>> = fields
        .iter()
        .map(|f| ast::ObjectField {
            key: f.key,
            value: transform_expr(&f.value, arena, enums).clone(),
            span: f.span,
        })
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn transform_match_arms<'a>(
    arms: &'a [ast::MatchArm<'a>],
    arena: &'a Bump,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> &'a [ast::MatchArm<'a>] {
    let transformed: Vec<ast::MatchArm<'a>> = arms
        .iter()
        .map(|arm| ast::MatchArm {
            pattern: arm.pattern.clone(),
            guard: arm
                .guard
                .as_ref()
                .map(|g| transform_expr(g, arena, enums).clone()),
            body: transform_expr(&arm.body, arena, enums).clone(),
            span: arm.span,
        })
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn transform_jsx_element<'a>(
    element: &ast::JsxElement<'a>,
    arena: &'a Bump,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> ast::JsxElement<'a> {
    let new_attrs: Vec<ast::JsxAttribute<'a>> = element
        .attributes
        .iter()
        .map(|attr| ast::JsxAttribute {
            name: attr.name,
            value: attr
                .value
                .as_ref()
                .map(|v| transform_expr(v, arena, enums).clone()),
            span: attr.span,
        })
        .collect();
    ast::JsxElement {
        tag: element.tag,
        attributes: ast::alloc_slice(arena, new_attrs),
        children: transform_exprs(element.children, arena, enums),
        span: element.span,
    }
}

/// If `expr` is `EnumName.Case` or `EnumName.Case(payload)`, return the
/// corresponding `EnumConstructor` expression. Otherwise return `None`.
fn try_resolve_enum_expr<'a>(
    expr: &Expr<'a>,
    enums: &HashMap<&'a str, HashSet<&'a str>>,
) -> Option<Expr<'a>> {
    match expr {
        Expr::FieldAccess { object, field, span } => {
            let enum_name = match object {
                Expr::Identifier { name, .. } => *name,
                _ => return None,
            };
            let cases = enums.get(enum_name)?;
            if !cases.contains(field) {
                return None;
            }
            Some(Expr::EnumConstructor {
                enum_name,
                case_name: field,
                payload: None,
                span: *span,
            })
        }
        Expr::Call {
            callee,
            args,
            span,
            ..
        } => {
            let (object, field) = match callee {
                Expr::FieldAccess { object, field, .. } => (object, *field),
                _ => return None,
            };
            let enum_name = match object {
                Expr::Identifier { name, .. } => *name,
                _ => return None,
            };
            let cases = enums.get(enum_name)?;
            if !cases.contains(field) {
                return None;
            }
            // Enum cases in this AST hold at most one payload.
            let payload = if args.is_empty() {
                None
            } else {
                Some(args.first().unwrap() as &'a Expr<'a>)
            };
            Some(Expr::EnumConstructor {
                enum_name,
                case_name: field,
                payload,
                span: *span,
            })
        }
        _ => None,
    }
}

fn alloc_expr<'a>(arena: &'a Bump, expr: Expr<'a>) -> &'a Expr<'a> {
    ast::alloc(arena, expr)
}

/// Lower recorded method calls into calls to mangled top-level functions.
///
/// After typechecking, any call `obj.method(args)` where `obj` is a struct with
/// a receiver method `method` has been recorded in `method_calls`. This pass
/// rewrites those call sites to `StructName_method(obj, args)` so the emitter
/// can treat them as ordinary function calls.
pub fn lower_method_calls<'a>(
    program: &mut Program<'a>,
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) {
    let transformed: Vec<Stmt<'a>> = program
        .statements
        .iter()
        .map(|stmt| lower_stmt(stmt, arena, method_calls))
        .collect();

    program.statements = ast::alloc_slice(arena, transformed);
}

fn lower_stmt<'a>(
    stmt: &'a Stmt<'a>,
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) -> Stmt<'a> {
    match stmt {
        Stmt::UnwrapLet {
            name,
            ty,
            is_const,
            scrutinee,
            alternative,
            span,
        } => Stmt::UnwrapLet {
            name,
            ty: ty.clone(),
            is_const: *is_const,
            scrutinee: lower_expr(scrutinee, arena, method_calls).clone(),
            alternative: match alternative {
                ast::UnwrapAlternative::Block(stmts) => ast::UnwrapAlternative::Block(
                    ast::alloc_slice(
                        arena,
                        stmts
                            .iter()
                            .map(|inner| lower_stmt(inner, arena, method_calls))
                            .collect::<Vec<_>>(),
                    ),
                ),
                ast::UnwrapAlternative::Match(arms) => ast::UnwrapAlternative::Match(
                    ast::alloc_slice(
                        arena,
                        arms.iter()
                            .map(|arm| ast::MatchArm {
                                pattern: arm.pattern.clone(),
                                guard: arm.guard.clone(),
                                body: lower_expr(&arm.body, arena, method_calls).clone(),
                                span: arm.span,
                            })
                            .collect::<Vec<_>>(),
                    ),
                ),
            },
            span: *span,
        },
        Stmt::Export { decl, span } => {
            let new_decl = match decl {
                ast::ExportDecl::Const { name, ty, value } => ast::ExportDecl::Const {
                    name,
                    ty: ty.clone(),
                    value: lower_expr(value, arena, method_calls).clone(),
                },
                ast::ExportDecl::Function {
                    name,
                    type_params,
                    params,
                    return_type,
                    body,
                    is_async,
                } => {
                    let new_body: Vec<Stmt<'a>> = body
                        .iter()
                        .map(|s| lower_stmt(s, arena, method_calls))
                        .collect();
                    ast::ExportDecl::Function {
                        name,
                        type_params,
                        params,
                        return_type: return_type.clone(),
                        body: ast::alloc_slice(arena, new_body),
                        is_async: *is_async,
                    }
                }
                ast::ExportDecl::NamedGroup { .. } => decl.clone(),
            };
            Stmt::Export {
                decl: new_decl,
                span: *span,
            }
        }
        Stmt::Import { .. } => stmt.clone(),
        Stmt::Const {
            name,
            ty,
            value,
            span,
        } => Stmt::Const {
            name,
            ty: ty.clone(),
            value: lower_expr(value, arena, method_calls).clone(),
            span: *span,
        },
        Stmt::Let {
            name,
            ty,
            value,
            span,
        } => Stmt::Let {
            name,
            ty: ty.clone(),
            value: lower_expr(value, arena, method_calls).clone(),
            span: *span,
        },
        Stmt::Function {
            name,
            type_params,
            params,
            return_type,
            body,
            is_async,
            span,
        } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| lower_stmt(s, arena, method_calls))
                .collect();
            Stmt::Function {
                name,
                type_params,
                params,
                return_type: return_type.clone(),
                body: ast::alloc_slice(arena, new_body),
                is_async: *is_async,
                span: *span,
            }
        }
        Stmt::ReceiverMethod {
            receiver_type,
            receiver_name,
            receiver_mutable,
            name,
            type_params,
            params,
            return_type,
            body,
            is_async,
            span,
        } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| lower_stmt(s, arena, method_calls))
                .collect();
            Stmt::ReceiverMethod {
                receiver_type,
                receiver_name,
                receiver_mutable: *receiver_mutable,
                name,
                type_params,
                params,
                return_type: return_type.clone(),
                body: ast::alloc_slice(arena, new_body),
                is_async: *is_async,
                span: *span,
            }
        }
        Stmt::Struct {
            name,
            type_params,
            fields,
            embeds,
            is_super,
            span,
        } => {
            let new_fields: Vec<ast::StructField<'a>> = fields
                .iter()
                .map(|f| ast::StructField {
                    name: f.name,
                    ty: f.ty.clone(),
                    default_value: f
                        .default_value
                        .as_ref()
                        .map(|v| lower_expr(v, arena, method_calls).clone()),
                    optional: f.optional,
                    span: f.span,
                })
                .collect();
            Stmt::Struct {
                name,
                type_params,
                fields: ast::alloc_slice(arena, new_fields),
                embeds,
                is_super: *is_super,
                span: *span,
            }
        }
        Stmt::Empty { .. }
        | Stmt::Enum { .. }
        | Stmt::TypeAlias { .. }
        | Stmt::Interface { .. } => stmt.clone(),
        Stmt::Break { .. } | Stmt::Continue { .. } => stmt.clone(),
        Stmt::Expr { expr, span } => Stmt::Expr {
            expr: lower_expr(expr, arena, method_calls).clone(),
            span: *span,
        },
        Stmt::Return { value, span } => Stmt::Return {
            value: value
                .as_ref()
                .map(|v| lower_expr(v, arena, method_calls).clone()),
            span: *span,
        },
        Stmt::If {
            condition,
            then_body,
            else_body,
            span,
        } => {
            let new_then: Vec<Stmt<'a>> = then_body
                .iter()
                .map(|s| lower_stmt(s, arena, method_calls))
                .collect();
            let new_else: Vec<Stmt<'a>> = else_body
                .iter()
                .map(|s| lower_stmt(s, arena, method_calls))
                .collect();
            Stmt::If {
                condition: lower_expr(condition, arena, method_calls).clone(),
                then_body: ast::alloc_slice(arena, new_then),
                else_body: ast::alloc_slice(arena, new_else),
                span: *span,
            }
        }
        Stmt::Block { body, span } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| lower_stmt(s, arena, method_calls))
                .collect();
            Stmt::Block {
                body: ast::alloc_slice(arena, new_body),
                span: *span,
            }
        }
        Stmt::ForOf {
            name,
            is_const,
            iterable,
            body,
            span,
        } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| lower_stmt(s, arena, method_calls))
                .collect();
            Stmt::ForOf {
                name,
                is_const: *is_const,
                iterable: lower_expr(iterable, arena, method_calls).clone(),
                body: ast::alloc_slice(arena, new_body),
                span: *span,
            }
        }
        Stmt::For {
            init,
            condition,
            step,
            body,
            span,
        } => {
            let new_init = init.as_ref().map(|i| match i {
                ast::ForInit::Const { name, value } => ast::ForInit::Const {
                    name,
                    value: lower_expr(value, arena, method_calls).clone(),
                },
                ast::ForInit::Let { name, value } => ast::ForInit::Let {
                    name,
                    value: lower_expr(value, arena, method_calls).clone(),
                },
                ast::ForInit::Expr(expr) => {
                    ast::ForInit::Expr(lower_expr(expr, arena, method_calls).clone())
                }
            });
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| lower_stmt(s, arena, method_calls))
                .collect();
            Stmt::For {
                init: new_init,
                condition: condition
                    .as_ref()
                    .map(|c| lower_expr(c, arena, method_calls).clone()),
                step: step.as_ref().map(|s| lower_expr(s, arena, method_calls).clone()),
                body: ast::alloc_slice(arena, new_body),
                span: *span,
            }
        }
        Stmt::Newtype { name, repr, span } => Stmt::Newtype {
            name,
            repr: *repr,
            span: *span,
        },
    }
}

fn lower_expr<'a>(
    expr: &'a Expr<'a>,
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) -> &'a Expr<'a> {
    // If this expression is a recorded method call, rewrite it.
    if let Some(target) = method_calls.get(&(expr as *const Expr<'a>)) {
        if let Expr::Call {
            callee: ast::Expr::FieldAccess { object, span, .. },
            args,
            ..
        } = expr
        {
            let mangled_name = ast::alloc_str(arena, &target.mangled);

            // For embedded methods, traverse the embed chain to reach the owner.
            let mut receiver_expr: Expr<'a> = (*object).clone();
            for embed_name in target.embed_path.iter() {
                receiver_expr = Expr::FieldAccess {
                    object: ast::alloc(arena, receiver_expr),
                    field: embed_name,
                    span: *span,
                };
            }

            let mut new_args: Vec<Expr<'a>> = vec![receiver_expr];
            new_args.extend(args.iter().cloned());
            let new_expr = Expr::Call {
                callee: ast::alloc(
                    arena,
                    Expr::Identifier {
                        name: mangled_name,
                        span: *span,
                    },
                ),
                type_args: &[],
                args: ast::alloc_slice(arena, new_args),
                span: *span,
            };
            return ast::alloc(arena, new_expr);
        }
    }

    let new_expr = match expr {
        Expr::Number { .. }
        | Expr::BigInt { .. }
        | Expr::String { .. }
        | Expr::Boolean { .. }
        | Expr::None { .. }
        | Expr::Identifier { .. } => return expr,

        Expr::Binary { op, left, right, span } => Expr::Binary {
            op: *op,
            left: lower_expr(left, arena, method_calls),
            right: lower_expr(right, arena, method_calls),
            span: *span,
        },
        Expr::Unary { op, operand, span } => Expr::Unary {
            op: *op,
            operand: lower_expr(operand, arena, method_calls),
            span: *span,
        },
        Expr::Call {
            callee,
            type_args,
            args,
            span,
        } => Expr::Call {
            callee: lower_expr(callee, arena, method_calls),
            type_args,
            args: lower_exprs(args, arena, method_calls),
            span: *span,
        },
        Expr::FieldAccess { object, field, span } => Expr::FieldAccess {
            object: lower_expr(object, arena, method_calls),
            field,
            span: *span,
        },
        Expr::IndexAccess { object, index, span } => Expr::IndexAccess {
            object: lower_expr(object, arena, method_calls),
            index: lower_expr(index, arena, method_calls),
            span: *span,
        },
        Expr::StructLiteral { name, fields, span } => Expr::StructLiteral {
            name,
            fields: lower_struct_fields(fields, arena, method_calls),
            span: *span,
        },
        Expr::EnumConstructor {
            enum_name,
            case_name,
            payload,
            span,
        } => Expr::EnumConstructor {
            enum_name,
            case_name,
            payload: payload
                .as_ref()
                .map(|p| lower_expr(p, arena, method_calls) as &'a Expr<'a>),
            span: *span,
        },
        Expr::Match {
            scrutinee,
            arms,
            span,
        } => Expr::Match {
            scrutinee: lower_expr(scrutinee, arena, method_calls),
            arms: lower_match_arms(arms, arena, method_calls),
            span: *span,
        },
        Expr::Unsafe {
            source,
            result_type,
            span,
        } => Expr::Unsafe {
            source,
            result_type: result_type.clone(),
            span: *span,
        },
        Expr::Bridge {
            kind,
            action,
            args,
            span,
        } => Expr::Bridge {
            kind,
            action,
            args: lower_exprs(args, arena, method_calls),
            span: *span,
        },
        Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            span,
        } => Expr::Ternary {
            condition: lower_expr(condition, arena, method_calls),
            then_branch: lower_expr(then_branch, arena, method_calls),
            else_branch: lower_expr(else_branch, arena, method_calls),
            span: *span,
        },
        Expr::Await { expr, span } => Expr::Await {
            expr: lower_expr(expr, arena, method_calls),
            span: *span,
        },
        Expr::JsxElement { element, span } => Expr::JsxElement {
            element: lower_jsx_element(element, arena, method_calls),
            span: *span,
        },
        Expr::JsxFragment { children, span } => Expr::JsxFragment {
            children: lower_exprs(children, arena, method_calls),
            span: *span,
        },
        Expr::JsxText { value, span } => Expr::JsxText { value, span: *span },
        Expr::Array { elements, span } => Expr::Array {
            elements: lower_exprs(elements, arena, method_calls),
            span: *span,
        },
        Expr::Object { fields, span } => Expr::Object {
            fields: lower_object_fields(fields, arena, method_calls),
            span: *span,
        },
        Expr::Spread { expr, span } => Expr::Spread {
            expr: lower_expr(expr, arena, method_calls),
            span: *span,
        },
        Expr::Paren { expr, span } => Expr::Paren {
            expr: lower_expr(expr, arena, method_calls),
            span: *span,
        },
        Expr::TemplateLiteral { parts, span } => Expr::TemplateLiteral {
            parts: lower_template_parts(parts, arena, method_calls),
            span: *span,
        },
        Expr::Function {
            params,
            return_type,
            body,
            is_async,
            span,
        } => {
            let new_body: Vec<Stmt<'a>> = body
                .iter()
                .map(|s| lower_stmt(s, arena, method_calls))
                .collect();
            Expr::Function {
                params,
                return_type: return_type.clone(),
                body: ast::alloc_slice(arena, new_body),
                is_async: *is_async,
                span: *span,
            }
        }
    };

    ast::alloc(arena, new_expr)
}

fn lower_template_parts<'a>(
    parts: &'a [ast::TemplatePart<'a>],
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) -> &'a [ast::TemplatePart<'a>] {
    let transformed: Vec<ast::TemplatePart<'a>> = parts
        .iter()
        .map(|part| match part {
            ast::TemplatePart::Text(text) => ast::TemplatePart::Text(text),
            ast::TemplatePart::Expr(expr) => {
                ast::TemplatePart::Expr(lower_expr(expr, arena, method_calls))
            }
        })
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn lower_exprs<'a>(
    exprs: &'a [Expr<'a>],
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) -> &'a [Expr<'a>] {
    let transformed: Vec<Expr<'a>> = exprs
        .iter()
        .map(|e| lower_expr(e, arena, method_calls).clone())
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn lower_struct_fields<'a>(
    fields: &'a [ast::StructLiteralField<'a>],
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) -> &'a [ast::StructLiteralField<'a>] {
    let transformed: Vec<ast::StructLiteralField<'a>> = fields
        .iter()
        .map(|f| ast::StructLiteralField {
            name: f.name,
            value: lower_expr(&f.value, arena, method_calls).clone(),
            span: f.span,
        })
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn lower_object_fields<'a>(
    fields: &'a [ast::ObjectField<'a>],
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) -> &'a [ast::ObjectField<'a>] {
    let transformed: Vec<ast::ObjectField<'a>> = fields
        .iter()
        .map(|f| ast::ObjectField {
            key: f.key,
            value: lower_expr(&f.value, arena, method_calls).clone(),
            span: f.span,
        })
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn lower_match_arms<'a>(
    arms: &'a [ast::MatchArm<'a>],
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) -> &'a [ast::MatchArm<'a>] {
    let transformed: Vec<ast::MatchArm<'a>> = arms
        .iter()
        .map(|arm| ast::MatchArm {
            pattern: arm.pattern.clone(),
            guard: arm
                .guard
                .as_ref()
                .map(|g| lower_expr(g, arena, method_calls).clone()),
            body: lower_expr(&arm.body, arena, method_calls).clone(),
            span: arm.span,
        })
        .collect();
    ast::alloc_slice(arena, transformed)
}

fn lower_jsx_element<'a>(
    element: &ast::JsxElement<'a>,
    arena: &'a Bump,
    method_calls: &HashMap<*const Expr<'a>, MethodTarget<'a>>,
) -> ast::JsxElement<'a> {
    let new_attrs: Vec<ast::JsxAttribute<'a>> = element
        .attributes
        .iter()
        .map(|attr| ast::JsxAttribute {
            name: attr.name,
            value: attr
                .value
                .as_ref()
                .map(|v| lower_expr(v, arena, method_calls).clone()),
            span: attr.span,
        })
        .collect();
    ast::JsxElement {
        tag: element.tag,
        attributes: ast::alloc_slice(arena, new_attrs),
        children: lower_exprs(element.children, arena, method_calls),
        span: element.span,
    }
}
