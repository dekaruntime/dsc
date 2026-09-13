//! Hook coloring and the Rules of React as type-system facts (rfd#64 amendment 3).
//!
//! Hook-ness is part of the function TYPE (`Hook<fn(...) T>`), the same
//! Generic-wrapper shape as `Exception<>` on a signature. A function whose
//! body calls a hook-typed function becomes hook-typed (fixed-point,
//! transitive). Aliasing preserves the color because the type flows. A
//! hook-typed function is not assignable to a plain `fn(...)` parameter.
//!
//! Hook functions are callable only from Component-typed functions
//! (`ReactNode` return) or other hook functions. Module-level code is a
//! plain context. The `use` prefix is convention, not the check.
//!
//! Straight-line: hook calls must be unconditional, un-looped, and before any
//! early return in the immediate body.

use std::collections::HashSet;

use super::{types::Type, Checker};
use crate::ast;

pub(super) const STRAIGHT_LINE: &str =
    "hooks run in a fixed order every render; move the condition inside the hook";

pub(super) const HOOK_ASSIGN: &str = "this closure calls a hook; hooks run only during render — accept a hook-typed parameter or lift the hook to the component";

/// `None` is not a state type: it is the empty Option payload, and without an
/// annotation `useState(None)` infers `T = none` rather than `Option<T>`.
pub(super) const NONE_INIT: &str =
    "`None` is not a state type; pass an explicit Option, e.g. `useState<Option<number>>(None)`, or a concrete initial value";

pub(super) fn hook_builtin_name(name: &str) -> Option<&'static str> {
    match name {
        "useState" => Some("useState"),
        "useRef" => Some("useRef"),
        "useEffect" => Some("useEffect"),
        _ => None,
    }
}

pub(super) fn is_hook_builtin(name: &str) -> bool {
    hook_builtin_name(name).is_some()
}

/// How a component-scope binding participates in `useEffect` dependency
/// inference. Stable identities are omitted; everything else that is not a
/// known reactive binding is a diagnostic, not a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaptureClass {
    /// useState values, props, and other hook results.
    Reactive,
    /// Setter and Ref identities, which React treats as stable.
    Stable,
    /// The `useEffect` builtin (and aliases). Stable, and the call is the
    /// inference site.
    UseEffect,
    /// Visible in the component, but neither reactive nor a stable identity.
    Other,
}

pub(super) const UNCLASSIFIED_CAPTURE: &str =
    "the compiler infers effect dependencies from useState values, props, and hook results — it will not guess";

pub(super) const EFFECT_INLINE: &str =
    "`useEffect` needs an inline function so the compiler can infer its dependencies from captures";

pub(super) const EFFECT_ARITY: &str =
    "`useEffect` takes one effect; the compiler infers the dependency array from the effect's captures — do not write one";

pub(super) struct HookFrame<'a> {
    pub name: Option<&'a str>,
    pub is_component: bool,
    pub seen_return: bool,
    pub conditional_depth: usize,
    pub body_called: bool,
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
            body_called: self.hook_body_called,
        };
        self.hook_fn_name = name;
        self.hook_is_component = is_component;
        self.hook_seen_return = false;
        self.hook_conditional_depth = 0;
        self.hook_body_called = false;
        saved
    }

    pub(super) fn pop_hook_frame(&mut self, saved: HookFrame<'a>) -> bool {
        let body_called = self.hook_body_called;
        self.hook_fn_name = saved.name;
        self.hook_is_component = saved.is_component;
        self.hook_seen_return = saved.seen_return;
        self.hook_conditional_depth = saved.conditional_depth;
        self.hook_body_called = saved.body_called;
        body_called
    }

    pub(super) fn with_hook_conditional<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.hook_conditional_depth += 1;
        let result = f(self);
        self.hook_conditional_depth -= 1;
        result
    }

    pub(super) fn paint_current_fn_hook(&mut self) {
        if let Some(name) = self.hook_fn_name {
            if let Some(ty) = self.globals.get(name).cloned() {
                if !ty.is_hook_fn() {
                    self.globals.insert(name, ty.as_hook());
                }
            }
        }
    }

    pub(super) fn note_hook_call(&mut self, name: &str, span: ast::Span, builtin: bool) {
        if self.hook_conditional_depth > 0 || self.loop_depth > 0 || self.hook_seen_return {
            self.error_span(span, STRAIGHT_LINE);
        }

        if !self.in_function {
            let kind = if builtin { "hook" } else { "hook function" };
            self.error_span(
                span,
                format!(
                    "cannot call {kind} `{name}` from a plain function; hook functions are only callable from Component functions or other hook functions"
                ),
            );
        }

        // Any function that calls a hook-typed function becomes hook-typed.
        self.hook_body_called = true;
        self.paint_current_fn_hook();
    }

    pub(super) fn note_hook_callee(
        &mut self,
        callee: &ast::Expr<'a>,
        ty: &Type<'a>,
        span: ast::Span,
    ) {
        if !ty.is_hook_fn() {
            return;
        }
        let (name, builtin) = match callee {
            ast::Expr::Identifier { name, .. } => (*name, is_hook_builtin(name)),
            _ => ("hook", false),
        };
        self.note_hook_call(name, span, builtin);
    }

    pub(super) fn reject_none_hook_init(
        &mut self,
        type_args: &'a [ast::Type<'a>],
        inferred: &Type<'a>,
        span: ast::Span,
    ) {
        if type_args.is_empty() && matches!(inferred, Type::None) {
            self.error_span(span, NONE_INIT);
        }
    }

    pub(super) fn check_use_state(
        &mut self,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        self.note_hook_builtin_ref("useState");
        self.note_hook_call("useState", span, true);
        self.check_state_or_ref("useState", type_args, args, span, true)
    }

    pub(super) fn check_use_ref(
        &mut self,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        self.note_hook_builtin_ref("useRef");
        self.note_hook_call("useRef", span, true);
        self.check_state_or_ref("useRef", type_args, args, span, false)
    }

    pub(super) fn check_use_effect(
        &mut self,
        call: &ast::Expr<'a>,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        self.note_hook_builtin_ref("useEffect");
        self.note_hook_call("useEffect", span, true);
        if !type_args.is_empty() {
            self.error_span(span, "`useEffect` does not take type arguments");
        }
        if args.len() != 1 {
            self.error_span(span, EFFECT_ARITY);
            for extra in args.iter().skip(1) {
                self.check_expr(extra);
            }
        }
        let expected = Self::effect_fn_type();
        let arg_type = if let Some(arg) = args.first() {
            self.check_exception_use(arg, super::exceptions::Use::Value, Some(expected.clone()))
        } else {
            Type::Error
        };
        if let Some(arg) = args.first() {
            if !arg_type.is_error() && !self.is_assignable(&expected, &arg_type) {
                self.error_at_expr(
                    arg,
                    format!("expected type `{expected}`, found type `{arg_type}`"),
                );
            }
            self.infer_effect_deps(call, arg);
        } else {
            self.effect_deps.insert(call as *const _, Vec::new());
        }
        Type::Named { name: "void" }
    }

    pub(super) fn note_hook_builtin_ref(&mut self, name: &'static str) {
        self.hook_builtin_refs.insert(name);
    }

    pub(super) fn is_use_effect_binding(&self, name: &str) -> bool {
        name == "useEffect" || self.lookup_capture(name) == Some(CaptureClass::UseEffect)
    }

    pub(super) fn param_capture_class(ty: &Type<'a>) -> CaptureClass {
        if is_stable_identity(ty) {
            CaptureClass::Stable
        } else {
            CaptureClass::Reactive
        }
    }

    pub(super) fn classify_initializer(
        &self,
        value: &ast::Expr<'a>,
        ty: &Type<'a>,
    ) -> CaptureClass {
        if self.is_use_effect_expr(value) {
            return CaptureClass::UseEffect;
        }
        if is_stable_identity(ty) {
            return CaptureClass::Stable;
        }
        if self.is_hook_call_expr(value) {
            return CaptureClass::Reactive;
        }
        CaptureClass::Other
    }

    fn is_use_effect_expr(&self, expr: &ast::Expr<'a>) -> bool {
        match expr {
            ast::Expr::Paren { expr, .. } | ast::Expr::Safe { expr, .. } => {
                self.is_use_effect_expr(expr)
            }
            ast::Expr::Identifier { name, .. } => self.is_use_effect_binding(name),
            _ => false,
        }
    }

    fn is_hook_call_expr(&self, expr: &ast::Expr<'a>) -> bool {
        match peel(expr) {
            ast::Expr::Call { callee, .. } => match peel(callee) {
                ast::Expr::Identifier { name, .. } => {
                    self.lookup_var(name).is_some_and(|ty| ty.is_hook_fn())
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn infer_effect_deps(&mut self, call: &ast::Expr<'a>, arg: &ast::Expr<'a>) {
        let ast::Expr::Function { params, body, .. } = arg else {
            self.error_at_expr(arg, EFFECT_INLINE);
            self.effect_deps.insert(call as *const _, Vec::new());
            return;
        };
        let captures = collect_free_idents(params, body);
        let mut deps = Vec::new();
        for (name, span) in captures {
            match self.classify_capture(name) {
                CaptureDecision::Skip => {}
                CaptureDecision::Dep => {
                    if !deps.contains(&name) {
                        deps.push(name);
                    }
                }
                CaptureDecision::Unclassified => {
                    self.error_span(
                        span,
                        format!("`{name}` is captured by this effect, but {UNCLASSIFIED_CAPTURE}"),
                    );
                }
            }
        }
        self.effect_deps.insert(call as *const _, deps);
    }

    fn classify_capture(&self, name: &str) -> CaptureDecision {
        match self.lookup_capture_depth(name) {
            None => CaptureDecision::Skip,
            Some((0, _)) => CaptureDecision::Skip,
            Some((_, CaptureClass::Reactive)) => CaptureDecision::Dep,
            Some((_, CaptureClass::Stable | CaptureClass::UseEffect)) => CaptureDecision::Skip,
            Some((_, CaptureClass::Other)) => CaptureDecision::Unclassified,
        }
    }

    fn effect_fn_type() -> Type<'a> {
        let cleanup = Type::Function {
            params: vec![],
            ret: Box::new(Type::Named { name: "void" }),
            optional: 0,
        };
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Option {
                inner: Box::new(cleanup),
            }),
            optional: 0,
        }
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
        self.reject_none_hook_init(type_args, &t, span);
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

    pub(super) fn reject_hook_shadow(&mut self, name: &str, span: ast::Span) {
        if is_hook_builtin(name) {
            self.error_span(span, format!("cannot shadow compiler-known hook `{name}`"));
        }
    }

    pub(super) fn builtin_use_effect_type() -> Type<'a> {
        Type::Function {
            params: vec![Self::effect_fn_type()],
            ret: Box::new(Type::Named { name: "void" }),
            optional: 0,
        }
        .as_hook()
    }

    pub(super) fn builtin_hook_type(state: bool) -> Type<'a> {
        let t = Type::Param { name: "T" };
        let inner = if state {
            Type::Function {
                params: vec![t.clone()],
                ret: Box::new(Type::Tuple {
                    elements: vec![
                        t.clone(),
                        Type::Generic {
                            base: "Setter",
                            args: vec![t],
                        },
                    ],
                }),
                optional: 0,
            }
        } else {
            Type::Function {
                params: vec![t.clone()],
                ret: Box::new(Type::Generic {
                    base: "Ref",
                    args: vec![t],
                }),
                optional: 0,
            }
        };
        inner.as_hook()
    }
}

fn is_stable_identity(ty: &Type<'_>) -> bool {
    matches!(
        ty,
        Type::Generic {
            base: "Setter" | "Ref",
            args
        } if args.len() == 1
    )
}

fn peel<'ast, 'src>(expr: &'src ast::Expr<'ast>) -> &'src ast::Expr<'ast> {
    match expr {
        ast::Expr::Paren { expr, .. } | ast::Expr::Safe { expr, .. } => peel(expr),
        other => other,
    }
}

enum CaptureDecision {
    Skip,
    Dep,
    Unclassified,
}

fn collect_free_idents<'a>(
    params: &'a [ast::Param<'a>],
    body: &'a [ast::Stmt<'a>],
) -> Vec<(&'a str, ast::Span)> {
    let mut walker = CaptureWalker {
        bound: vec![params.iter().map(|p| p.name).collect()],
        seen: HashSet::new(),
        frees: Vec::new(),
    };
    walker.stmts(body);
    walker.frees
}

struct CaptureWalker<'a> {
    bound: Vec<HashSet<&'a str>>,
    seen: HashSet<&'a str>,
    frees: Vec<(&'a str, ast::Span)>,
}

impl<'a> CaptureWalker<'a> {
    fn is_bound(&self, name: &str) -> bool {
        self.bound.iter().rev().any(|scope| scope.contains(name))
    }

    fn bind(&mut self, name: &'a str) {
        if let Some(scope) = self.bound.last_mut() {
            scope.insert(name);
        }
    }

    fn push(&mut self) {
        self.bound.push(HashSet::new());
    }

    fn pop(&mut self) {
        self.bound.pop();
    }

    fn ident(&mut self, name: &'a str, span: ast::Span) {
        if name == "_" || self.is_bound(name) || !self.seen.insert(name) {
            return;
        }
        self.frees.push((name, span));
    }

    fn stmts(&mut self, stmts: &'a [ast::Stmt<'a>]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &'a ast::Stmt<'a>) {
        match stmt {
            ast::Stmt::Const { name, value, .. } | ast::Stmt::Let { name, value, .. } => {
                self.expr(value);
                self.bind(name);
            }
            ast::Stmt::TupleBinding { names, value, .. } => {
                self.expr(value);
                for name in names.iter() {
                    self.bind(name);
                }
            }
            ast::Stmt::UnwrapLet {
                name,
                scrutinee,
                alternative,
                ..
            } => {
                self.expr(scrutinee);
                match alternative {
                    ast::UnwrapAlternative::Block(body) => {
                        self.push();
                        self.stmts(body);
                        self.pop();
                    }
                    ast::UnwrapAlternative::Match(arms) => {
                        for arm in arms.iter() {
                            self.match_arm(arm);
                        }
                    }
                }
                self.bind(name);
            }
            ast::Stmt::Function {
                name, params, body, ..
            } => {
                self.bind(name);
                self.push();
                for param in params.iter() {
                    if let Some(default) = &param.default_value {
                        self.expr(default);
                    }
                    self.bind(param.name);
                }
                self.stmts(body);
                self.pop();
            }
            ast::Stmt::Export { decl, .. } => match decl {
                ast::ExportDecl::Const { name, value, .. } => {
                    self.expr(value);
                    self.bind(name);
                }
                ast::ExportDecl::Function {
                    name, params, body, ..
                } => {
                    self.bind(name);
                    self.push();
                    for param in params.iter() {
                        if let Some(default) = &param.default_value {
                            self.expr(default);
                        }
                        self.bind(param.name);
                    }
                    self.stmts(body);
                    self.pop();
                }
                ast::ExportDecl::NamedGroup { .. } => {}
            },
            ast::Stmt::Expr { expr, .. } => self.expr(expr),
            ast::Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            ast::Stmt::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.expr(condition);
                self.push();
                self.stmts(then_body);
                self.pop();
                self.push();
                self.stmts(else_body);
                self.pop();
            }
            ast::Stmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => {
                self.push();
                self.stmts(body);
                self.pop();
                self.push();
                self.bind(catch_name);
                self.stmts(catch_body);
                self.pop();
            }
            ast::Stmt::Block { body, .. } => {
                self.push();
                self.stmts(body);
                self.pop();
            }
            ast::Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                self.push();
                if let Some(init) = init {
                    match init {
                        ast::ForInit::Const { name, value } | ast::ForInit::Let { name, value } => {
                            self.expr(value);
                            self.bind(name);
                        }
                        ast::ForInit::Expr(value) => self.expr(value),
                    }
                }
                if let Some(condition) = condition {
                    self.expr(condition);
                }
                if let Some(step) = step {
                    self.expr(step);
                }
                self.stmts(body);
                self.pop();
            }
            ast::Stmt::ForOf {
                name,
                iterable,
                body,
                ..
            } => {
                self.expr(iterable);
                self.push();
                self.bind(name);
                self.stmts(body);
                self.pop();
            }
            ast::Stmt::ReceiverMethod {
                receiver_name,
                params,
                body,
                ..
            } => {
                self.push();
                self.bind(receiver_name);
                for param in params.iter() {
                    if let Some(default) = &param.default_value {
                        self.expr(default);
                    }
                    self.bind(param.name);
                }
                self.stmts(body);
                self.pop();
            }
            ast::Stmt::Struct { fields, .. } => {
                for field in fields.iter() {
                    if let Some(default) = &field.default_value {
                        self.expr(default);
                    }
                }
            }
            ast::Stmt::Opaque { .. }
            | ast::Stmt::Summon { .. }
            | ast::Stmt::Import { .. }
            | ast::Stmt::Enum { .. }
            | ast::Stmt::TypeAlias { .. }
            | ast::Stmt::Newtype { .. }
            | ast::Stmt::Interface { .. }
            | ast::Stmt::Empty { .. }
            | ast::Stmt::Break { .. }
            | ast::Stmt::Continue { .. } => {}
        }
    }

    fn expr(&mut self, expr: &'a ast::Expr<'a>) {
        match expr {
            ast::Expr::Identifier { name, span } => self.ident(name, *span),
            ast::Expr::Binary { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            ast::Expr::Unary { operand, .. } => self.expr(operand),
            ast::Expr::Call { callee, args, .. } => {
                self.expr(callee);
                for arg in args.iter() {
                    self.expr(arg);
                }
            }
            ast::Expr::FieldAccess { object, .. }
            | ast::Expr::Await { expr: object, .. }
            | ast::Expr::Safe { expr: object, .. }
            | ast::Expr::Paren { expr: object, .. }
            | ast::Expr::Spread { expr: object, .. } => self.expr(object),
            ast::Expr::IndexAccess { object, index, .. } => {
                self.expr(object);
                self.expr(index);
            }
            ast::Expr::StructLiteral { fields, .. } => {
                for field in fields.iter() {
                    self.expr(&field.value);
                }
            }
            ast::Expr::EnumConstructor { payload, .. } => {
                if let Some(payload) = payload {
                    self.expr(payload);
                }
            }
            ast::Expr::Match {
                scrutinee, arms, ..
            } => {
                self.expr(scrutinee);
                for arm in arms.iter() {
                    self.match_arm(arm);
                }
            }
            ast::Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.expr(condition);
                self.expr(then_branch);
                self.expr(else_branch);
            }
            ast::Expr::Bridge { args, .. } | ast::Expr::Array { elements: args, .. } => {
                for arg in args.iter() {
                    self.expr(arg);
                }
            }
            ast::Expr::Object { fields, .. } => {
                for field in fields.iter() {
                    self.expr(&field.value);
                }
            }
            ast::Expr::TemplateLiteral { parts, .. } => {
                for part in parts.iter() {
                    if let ast::TemplatePart::Expr(inner) = part {
                        self.expr(inner);
                    }
                }
            }
            ast::Expr::Build { body, .. } => self.stmts(body),
            ast::Expr::Function { params, body, .. } => {
                self.push();
                for param in params.iter() {
                    if let Some(default) = &param.default_value {
                        self.expr(default);
                    }
                    self.bind(param.name);
                }
                self.stmts(body);
                self.pop();
            }
            ast::Expr::JsxElement { element, .. } => {
                for attr in element.attributes.iter() {
                    if let Some(value) = &attr.value {
                        self.expr(value);
                    }
                }
                for child in element.children.iter() {
                    self.expr(child);
                }
            }
            ast::Expr::JsxFragment { children, .. } => {
                for child in children.iter() {
                    self.expr(child);
                }
            }
            ast::Expr::Number { .. }
            | ast::Expr::BigInt { .. }
            | ast::Expr::String { .. }
            | ast::Expr::Boolean { .. }
            | ast::Expr::None { .. }
            | ast::Expr::Unsafe { .. }
            | ast::Expr::JsxText { .. } => {}
        }
    }

    fn match_arm(&mut self, arm: &'a ast::MatchArm<'a>) {
        self.push();
        self.pattern(&arm.pattern);
        if let Some(guard) = &arm.guard {
            self.expr(guard);
        }
        self.expr(&arm.body);
        self.pop();
    }

    fn pattern(&mut self, pattern: &'a ast::Pattern<'a>) {
        match pattern {
            ast::Pattern::Wildcard { .. } | ast::Pattern::Literal { .. } => {}
            ast::Pattern::Identifier { name, .. } => self.bind(name),
            ast::Pattern::Constructor { payload, .. } => {
                if let Some(payload) = payload {
                    self.pattern(payload);
                }
            }
            ast::Pattern::Struct { fields, .. } => {
                for field in fields.iter() {
                    self.pattern(&field.pattern);
                }
            }
            ast::Pattern::Tuple { elements, .. } => {
                for element in elements.iter() {
                    self.pattern(element);
                }
            }
            ast::Pattern::Or { alternatives, .. } => {
                for alt in alternatives.iter() {
                    self.pattern(alt);
                }
            }
        }
    }
}
