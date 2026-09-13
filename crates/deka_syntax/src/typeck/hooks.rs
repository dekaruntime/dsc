//! Hook coloring and the Rules of React as type-system facts (rfd#64 amendment 3).
//!
//! Coloring reuses the Exception pattern: a first pass discovers which functions
//! call a hook builtin, then call sites are judged against that color. A function
//! that calls `useState`/`useRef` is a hook function (a custom hook; the `use`
//! prefix is convention, not the check). Hook functions are callable only from
//! Component-typed functions (ReactNode return) or other hook functions.
//! Module-level code is a plain context.
//!
//! Straight-line: hook calls must be unconditional, un-looped, and before any
//! early return in the immediate body.

use std::collections::HashSet;

use super::{types::Type, Checker};
use crate::ast;

pub(super) const STRAIGHT_LINE: &str =
    "hooks run in a fixed order every render; move the condition inside the hook";

pub(super) fn is_hook_builtin(name: &str) -> bool {
    matches!(name, "useState" | "useRef")
}

/// Named functions whose immediate body calls `useState` or `useRef`. Nested
/// function expressions are collected under their binding name, not attributed
/// to the enclosing function.
pub(super) fn collect_hook_functions<'a>(program: &'a ast::Program<'a>) -> HashSet<&'a str> {
    let mut hooks = HashSet::new();
    collect_in_stmts(program.statements, &mut hooks);
    hooks
}

fn collect_in_stmts<'a>(stmts: &'a [ast::Stmt<'a>], hooks: &mut HashSet<&'a str>) {
    for stmt in stmts {
        match stmt {
            ast::Stmt::Function { name, body, .. }
            | ast::Stmt::Export {
                decl: ast::ExportDecl::Function { name, body, .. },
                ..
            } => {
                if stmts_call_hook_builtin(body) {
                    hooks.insert(*name);
                }
                collect_in_stmts(body, hooks);
            }
            ast::Stmt::ReceiverMethod { name, body, .. } => {
                if stmts_call_hook_builtin(body) {
                    hooks.insert(*name);
                }
                collect_in_stmts(body, hooks);
            }
            ast::Stmt::Const {
                name,
                value: ast::Expr::Function { body, .. },
                ..
            }
            | ast::Stmt::Let {
                name,
                value: ast::Expr::Function { body, .. },
                ..
            }
            | ast::Stmt::Export {
                decl:
                    ast::ExportDecl::Const {
                        name,
                        value: ast::Expr::Function { body, .. },
                        ..
                    },
                ..
            } => {
                if stmts_call_hook_builtin(body) {
                    hooks.insert(*name);
                }
                collect_in_stmts(body, hooks);
            }
            ast::Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                collect_in_stmts(then_body, hooks);
                collect_in_stmts(else_body, hooks);
            }
            ast::Stmt::Block { body, .. } => collect_in_stmts(body, hooks),
            ast::Stmt::Try {
                body, catch_body, ..
            } => {
                collect_in_stmts(body, hooks);
                collect_in_stmts(catch_body, hooks);
            }
            ast::Stmt::For { body, .. } | ast::Stmt::ForOf { body, .. } => {
                collect_in_stmts(body, hooks);
            }
            ast::Stmt::UnwrapLet { alternative, .. } => match alternative {
                ast::UnwrapAlternative::Block(body) => collect_in_stmts(body, hooks),
                ast::UnwrapAlternative::Match(_) => {}
            },
            _ => {}
        }
    }
}

fn stmts_call_hook_builtin(stmts: &[ast::Stmt<'_>]) -> bool {
    stmts.iter().any(stmt_calls_hook_builtin)
}

fn stmt_calls_hook_builtin(stmt: &ast::Stmt<'_>) -> bool {
    match stmt {
        ast::Stmt::Function { .. } | ast::Stmt::ReceiverMethod { .. } => false,
        ast::Stmt::Const { value, .. }
        | ast::Stmt::Let { value, .. }
        | ast::Stmt::Export {
            decl: ast::ExportDecl::Const { value, .. },
            ..
        } => !matches!(value, ast::Expr::Function { .. }) && expr_calls_hook_builtin(value),
        ast::Stmt::TupleBinding { value, .. } | ast::Stmt::Expr { expr: value, .. } => {
            expr_calls_hook_builtin(value)
        }
        ast::Stmt::Return {
            value: Some(value), ..
        } => expr_calls_hook_builtin(value),
        ast::Stmt::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            expr_calls_hook_builtin(condition)
                || stmts_call_hook_builtin(then_body)
                || stmts_call_hook_builtin(else_body)
        }
        ast::Stmt::Block { body, .. } => stmts_call_hook_builtin(body),
        ast::Stmt::Try {
            body, catch_body, ..
        } => stmts_call_hook_builtin(body) || stmts_call_hook_builtin(catch_body),
        ast::Stmt::For {
            init,
            condition,
            step,
            body,
            ..
        } => {
            let init_hit = match init {
                Some(ast::ForInit::Const { value, .. } | ast::ForInit::Let { value, .. }) => {
                    expr_calls_hook_builtin(value)
                }
                Some(ast::ForInit::Expr(value)) => expr_calls_hook_builtin(value),
                None => false,
            };
            init_hit
                || condition.as_ref().is_some_and(expr_calls_hook_builtin)
                || step.as_ref().is_some_and(expr_calls_hook_builtin)
                || stmts_call_hook_builtin(body)
        }
        ast::Stmt::ForOf { iterable, body, .. } => {
            expr_calls_hook_builtin(iterable) || stmts_call_hook_builtin(body)
        }
        ast::Stmt::UnwrapLet {
            scrutinee,
            alternative,
            ..
        } => {
            expr_calls_hook_builtin(scrutinee)
                || match alternative {
                    ast::UnwrapAlternative::Block(body) => stmts_call_hook_builtin(body),
                    ast::UnwrapAlternative::Match(arms) => arms.iter().any(|arm| {
                        arm.guard.as_ref().is_some_and(expr_calls_hook_builtin)
                            || expr_calls_hook_builtin(&arm.body)
                    }),
                }
        }
        ast::Stmt::Export {
            decl: ast::ExportDecl::Function { body, .. },
            ..
        } => stmts_call_hook_builtin(body),
        _ => false,
    }
}

fn expr_calls_hook_builtin(expr: &ast::Expr<'_>) -> bool {
    match expr {
        ast::Expr::Function { .. } => false,
        ast::Expr::Call { callee, args, .. } => {
            matches!(callee, ast::Expr::Identifier { name, .. } if is_hook_builtin(name))
                || expr_calls_hook_builtin(callee)
                || args.iter().any(expr_calls_hook_builtin)
        }
        ast::Expr::Binary { left, right, .. } => {
            expr_calls_hook_builtin(left) || expr_calls_hook_builtin(right)
        }
        ast::Expr::Unary { operand, .. }
        | ast::Expr::Await { expr: operand, .. }
        | ast::Expr::Spread { expr: operand, .. }
        | ast::Expr::Safe { expr: operand, .. }
        | ast::Expr::Paren { expr: operand, .. } => expr_calls_hook_builtin(operand),
        ast::Expr::FieldAccess { object, .. } => expr_calls_hook_builtin(object),
        ast::Expr::IndexAccess { object, index, .. } => {
            expr_calls_hook_builtin(object) || expr_calls_hook_builtin(index)
        }
        ast::Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            expr_calls_hook_builtin(condition)
                || expr_calls_hook_builtin(then_branch)
                || expr_calls_hook_builtin(else_branch)
        }
        ast::Expr::Array { elements, .. } => elements.iter().any(expr_calls_hook_builtin),
        ast::Expr::Object { fields, .. } => fields
            .iter()
            .any(|field| expr_calls_hook_builtin(&field.value)),
        ast::Expr::StructLiteral { fields, .. } => fields
            .iter()
            .any(|field| expr_calls_hook_builtin(&field.value)),
        ast::Expr::EnumConstructor {
            payload: Some(payload),
            ..
        } => expr_calls_hook_builtin(payload),
        ast::Expr::Match {
            scrutinee, arms, ..
        } => {
            expr_calls_hook_builtin(scrutinee)
                || arms.iter().any(|arm| {
                    arm.guard.as_ref().is_some_and(expr_calls_hook_builtin)
                        || expr_calls_hook_builtin(&arm.body)
                })
        }
        ast::Expr::JsxElement { element, .. } => {
            element
                .attributes
                .iter()
                .any(|attr| attr.value.as_ref().is_some_and(expr_calls_hook_builtin))
                || element.children.iter().any(expr_calls_hook_builtin)
        }
        ast::Expr::JsxFragment { children, .. } => children.iter().any(expr_calls_hook_builtin),
        ast::Expr::TemplateLiteral { parts, .. } => parts.iter().any(|part| match part {
            ast::TemplatePart::Expr(expr) => expr_calls_hook_builtin(expr),
            ast::TemplatePart::Text(_) => false,
        }),
        ast::Expr::Bridge { args, .. } => args.iter().any(expr_calls_hook_builtin),
        _ => false,
    }
}

pub(super) struct HookFrame<'a> {
    pub name: Option<&'a str>,
    pub is_component: bool,
    pub seen_return: bool,
    pub conditional_depth: usize,
}

impl<'a> Checker<'a> {
    pub(super) fn is_component_return(ty: &Option<Type<'a>>) -> bool {
        ty.as_ref().is_some_and(Type::is_react_node)
    }

    pub(super) fn push_hook_frame(
        &mut self,
        name: Option<&'a str>,
        is_component: bool,
    ) -> HookFrame<'a> {
        let saved = HookFrame {
            name: self.hook_fn_name,
            is_component: self.hook_is_component,
            seen_return: self.hook_seen_return,
            conditional_depth: self.hook_conditional_depth,
        };
        self.hook_fn_name = name;
        self.hook_is_component = is_component;
        self.hook_seen_return = false;
        self.hook_conditional_depth = 0;
        self.last_fn_expr_is_hook = false;
        saved
    }

    pub(super) fn pop_hook_frame(&mut self, saved: HookFrame<'a>, expr_was_hook: bool) {
        self.hook_fn_name = saved.name;
        self.hook_is_component = saved.is_component;
        self.hook_seen_return = saved.seen_return;
        self.hook_conditional_depth = saved.conditional_depth;
        self.last_fn_expr_is_hook = expr_was_hook;
    }

    pub(super) fn with_hook_conditional<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.hook_conditional_depth += 1;
        let result = f(self);
        self.hook_conditional_depth -= 1;
        result
    }

    pub(super) fn note_hook_call(&mut self, name: &str, span: ast::Span, builtin: bool) {
        if self.hook_conditional_depth > 0 || self.loop_depth > 0 || self.hook_seen_return {
            self.error_span(span, STRAIGHT_LINE);
        }

        let allowed = self.hook_is_component
            || self
                .hook_fn_name
                .is_some_and(|fn_name| self.hook_functions.contains(fn_name))
            || self.hook_fn_name.is_none() && self.in_function;

        if !self.in_function || !allowed {
            let kind = if builtin { "hook" } else { "hook function" };
            self.error_span(
                span,
                format!(
                    "cannot call {kind} `{name}` from a plain function; hook functions are only callable from Component functions or other hook functions"
                ),
            );
        }

        if self.hook_fn_name.is_none() && self.in_function {
            self.last_fn_expr_is_hook = true;
        }
        if let Some(fn_name) = self.hook_fn_name {
            if builtin {
                self.hook_functions.insert(fn_name);
            }
        }
    }

    pub(super) fn check_use_state(
        &mut self,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        self.note_hook_call("useState", span, true);
        self.check_state_or_ref("useState", type_args, args, span, true)
    }

    pub(super) fn check_use_ref(
        &mut self,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        self.note_hook_call("useRef", span, true);
        self.check_state_or_ref("useRef", type_args, args, span, false)
    }

    fn check_state_or_ref(
        &mut self,
        name: &str,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
        state: bool,
    ) -> Type<'a> {
        if type_args.len() > 1 {
            self.error_span(span, format!("`{name}` takes at most one type argument"));
        }
        if args.len() != 1 {
            self.error_span(span, format!("`{name}` takes one initial value"));
        }
        for extra in args.iter().skip(1) {
            self.check_expr(extra);
        }
        let arg_type = if let Some(arg) = args.first() {
            self.check_expr(arg)
        } else {
            Type::Error
        };
        let t = if let Some(ty) = type_args.first() {
            let expected = self.resolve_ast_type(ty);
            if !args.is_empty() && !self.is_assignable(&expected, &arg_type) {
                self.error_at_expr(
                    &args[0],
                    super::with_union_narrowing_hint(
                        format!("expected type `{expected}`, found type `{arg_type}`"),
                        &expected,
                        &arg_type,
                    ),
                );
            }
            expected
        } else {
            arg_type
        };
        if state {
            Type::Tuple {
                elements: vec![
                    t.clone(),
                    Type::Generic {
                        base: "Setter",
                        args: vec![t],
                    },
                ],
            }
        } else {
            Type::Generic {
                base: "Ref",
                args: vec![t],
            }
        }
    }

    pub(super) fn check_setter_call(
        &mut self,
        payload: Type<'a>,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        if !type_args.is_empty() {
            self.error_span(span, "`Setter` calls do not take type arguments");
        }
        if args.len() != 1 {
            self.error_span(
                span,
                format!(
                    "`Setter<{payload}>` takes one argument: a `{payload}` or a `fn({payload}) {payload}` updater"
                ),
            );
            for arg in args {
                self.check_expr(arg);
            }
            return Type::Named { name: "void" };
        }
        let arg_type = self.check_expr(&args[0]);
        let updater = Type::Function {
            params: vec![payload.clone()],
            ret: Box::new(payload.clone()),
            optional: 0,
        };
        if !self.is_assignable(&payload, &arg_type) && !self.is_assignable(&updater, &arg_type) {
            self.error_at_expr(
                &args[0],
                format!(
                    "expected type `{payload}` or `fn({payload}) {payload}`, found type `{arg_type}`"
                ),
            );
        }
        Type::Named { name: "void" }
    }

    pub(super) fn remember_hook_binding(&mut self, name: &'a str) {
        if self.last_fn_expr_is_hook {
            self.hook_functions.insert(name);
        }
        self.last_fn_expr_is_hook = false;
    }

    pub(super) fn reject_hook_shadow(&mut self, name: &str, span: ast::Span) {
        if is_hook_builtin(name) {
            self.error_span(span, format!("cannot shadow compiler-known hook `{name}`"));
        }
    }
}
