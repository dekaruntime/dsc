//! Exhaustive expression traversal for compiler validation and prelude demand.
use crate::{ExportDecl, Expr, Stmt};

pub fn walk_stmt(stmt: &Stmt<'_>, visit: &mut dyn FnMut(&Expr<'_>)) {
    match stmt {
        Stmt::Const { value, .. } | Stmt::Let { value, .. } | Stmt::Expr { expr: value, .. } => {
            walk_expr(value, visit);
        }
        Stmt::UnwrapLet {
            scrutinee,
            alternative,
            ..
        } => {
            walk_expr(scrutinee, visit);
            match alternative {
                crate::UnwrapAlternative::Block(body) => {
                    for stmt in *body {
                        walk_stmt(stmt, visit);
                    }
                }
                crate::UnwrapAlternative::Match(arms) => {
                    for arm in *arms {
                        if let Some(guard) = &arm.guard {
                            walk_expr(guard, visit);
                        }
                        walk_expr(&arm.body, visit);
                    }
                }
            }
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                walk_expr(value, visit);
            }
        }
        Stmt::Export { decl, .. } => match decl {
            ExportDecl::Const { value, .. } => walk_expr(value, visit),
            ExportDecl::Function { body, params, .. } => {
                for param in params.iter() {
                    if let Some(default) = &param.default_value {
                        walk_expr(default, visit);
                    }
                }
                for s in body.iter() {
                    walk_stmt(s, visit);
                }
            }
            ExportDecl::NamedGroup { .. } => {}
        },
        Stmt::Function { body, params, .. } | Stmt::ReceiverMethod { body, params, .. } => {
            for param in params.iter() {
                if let Some(default) = &param.default_value {
                    walk_expr(default, visit);
                }
            }
            for s in body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            walk_expr(condition, visit);
            for s in then_body.iter() {
                walk_stmt(s, visit);
            }
            for s in else_body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::Block { body, .. } => {
            for s in body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::For {
            init,
            condition,
            step,
            body,
            ..
        } => {
            match init {
                Some(crate::ForInit::Const { value, .. })
                | Some(crate::ForInit::Let { value, .. }) => walk_expr(value, visit),
                Some(crate::ForInit::Expr(value)) => walk_expr(value, visit),
                None => {}
            }
            if let Some(condition) = condition {
                walk_expr(condition, visit);
            }
            if let Some(step) = step {
                walk_expr(step, visit);
            }
            for s in body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::ForOf { iterable, body, .. } => {
            walk_expr(iterable, visit);
            for s in body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::Struct { fields, .. } => {
            for field in fields.iter() {
                if let Some(default) = &field.default_value {
                    walk_expr(default, visit);
                }
            }
        }
        Stmt::Enum { .. }
        | Stmt::TypeAlias { .. }
        | Stmt::Newtype { .. }
        | Stmt::Interface { .. }
        | Stmt::Import { .. }
        | Stmt::Empty { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. } => {}
    }
}

pub fn walk_expr(expr: &Expr<'_>, visit: &mut dyn FnMut(&Expr<'_>)) {
    visit(expr);
    match expr {
        Expr::Binary { left, right, .. } => {
            walk_expr(left, visit);
            walk_expr(right, visit);
        }
        Expr::Unary { operand, .. } => walk_expr(operand, visit),
        Expr::Call { callee, args, .. } => {
            walk_expr(callee, visit);
            for arg in args.iter() {
                walk_expr(arg, visit);
            }
        }
        Expr::FieldAccess { object, .. }
        | Expr::Await { expr: object, .. }
        | Expr::Safe { expr: object, .. }
        | Expr::Paren { expr: object, .. }
        | Expr::Spread { expr: object, .. } => walk_expr(object, visit),
        Expr::IndexAccess { object, index, .. } => {
            walk_expr(object, visit);
            walk_expr(index, visit);
        }
        Expr::StructLiteral { fields, .. } => {
            for field in fields.iter() {
                walk_expr(&field.value, visit);
            }
        }
        Expr::EnumConstructor { payload, .. } => {
            if let Some(payload) = payload {
                walk_expr(payload, visit);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            walk_expr(scrutinee, visit);
            for arm in arms.iter() {
                if let Some(guard) = &arm.guard {
                    walk_expr(guard, visit);
                }
                walk_expr(&arm.body, visit);
            }
        }
        Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            walk_expr(condition, visit);
            walk_expr(then_branch, visit);
            walk_expr(else_branch, visit);
        }
        Expr::Bridge { args: elements, .. } | Expr::Array { elements, .. } => {
            for element in elements.iter() {
                walk_expr(element, visit);
            }
        }
        Expr::Object { fields, .. } => {
            for field in fields.iter() {
                walk_expr(&field.value, visit);
            }
        }
        Expr::TemplateLiteral { parts, .. } => {
            for part in parts.iter() {
                if let crate::TemplatePart::Expr(inner) = part {
                    walk_expr(inner, visit);
                }
            }
        }
        Expr::Build { body, .. } => {
            for stmt in *body {
                walk_stmt(stmt, visit);
            }
        }
        Expr::Function { body, params, .. } => {
            for param in params.iter() {
                if let Some(default) = &param.default_value {
                    walk_expr(default, visit);
                }
            }
            for stmt in body.iter() {
                walk_stmt(stmt, visit);
            }
        }
        Expr::JsxElement { element, .. } => {
            for attr in element.attributes.iter() {
                if let Some(value) = &attr.value {
                    walk_expr(value, visit);
                }
            }
            for child in element.children.iter() {
                walk_expr(child, visit);
            }
        }
        Expr::JsxFragment { children, .. } => {
            for child in children.iter() {
                walk_expr(child, visit);
            }
        }
        Expr::Number { .. }
        | Expr::BigInt { .. }
        | Expr::String { .. }
        | Expr::Boolean { .. }
        | Expr::None { .. }
        | Expr::Identifier { .. }
        | Expr::Unsafe { .. }
        | Expr::JsxText { .. } => {}
    }
}
