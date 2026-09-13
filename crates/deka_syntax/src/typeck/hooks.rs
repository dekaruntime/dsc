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

use std::collections::{HashMap, HashSet};

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
        "useContext" => Some("useContext"),
        _ => None,
    }
}

pub(super) fn react_import_name(name: &str) -> Option<&'static str> {
    match name {
        "useState" => Some("useState"),
        "useRef" => Some("useRef"),
        "useEffect" => Some("useEffect"),
        "useContext" => Some("useContext"),
        "createContext" => Some("createContext"),
        _ => None,
    }
}

pub(super) fn is_hook_builtin(name: &str) -> bool {
    hook_builtin_name(name).is_some()
}

pub(super) fn is_react_builtin(name: &str) -> bool {
    react_import_name(name).is_some()
}

/// Names the compiler owns, including auto-inserted memo hooks that never
/// exist in DS source (rfd#64 lane D). Shadowing any of them would collide
/// with the emitted `import { useMemo, useCallback } from "@js/react"`.
pub(super) fn reserved_hook_name(name: &str) -> Option<&'static str> {
    match name {
        "useMemo" => Some("useMemo"),
        "useCallback" => Some("useCallback"),
        other => hook_builtin_name(other),
    }
}

pub(super) fn written_memo_diagnostic(name: &str) -> Option<&'static str> {
    match name {
        "useMemo" => Some(WRITTEN_MEMO),
        "useCallback" => Some(WRITTEN_CALLBACK),
        _ => None,
    }
}

/// Compiler-inserted memoization (rfd#64 lane D). Keyed by the source
/// expression that emission wraps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoKind {
    UseMemo,
    UseCallback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoSite<'a> {
    pub kind: MemoKind,
    pub deps: Vec<&'a str>,
}

pub(super) const WRITTEN_MEMO: &str =
    "`useMemo` is inserted by the compiler from a pure expression over tracked inputs — do not write it";

pub(super) const WRITTEN_CALLBACK: &str =
    "`useCallback` is inserted by the compiler for callbacks over tracked inputs — do not write it";

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
    /// The `createContext` builtin (and aliases). Not a hook.
    CreateContext,
    /// The `useContext` builtin (and aliases). The *call* is a hook; the
    /// function value itself is a stable identity.
    UseContext,
    /// Visible in the component, but neither reactive nor a stable identity.
    Other,
}

/// One `createContext` identity. Provider presence is about this object,
/// not the `Context<T>` type — two `Context<string>` values are distinct.
#[derive(Clone, Debug)]
pub(super) struct ContextInfo<'a> {
    pub name: &'a str,
    pub has_default: bool,
}

pub(super) const UNCLASSIFIED_CAPTURE: &str =
    "the compiler infers effect dependencies from useState values, props, and hook results — it will not guess";

pub(super) const EFFECT_INLINE: &str =
    "`useEffect` needs an inline function so the compiler can infer its dependencies from captures";

pub(super) const EFFECT_ARITY: &str =
    "`useEffect` takes one effect; the compiler infers the dependency array from the effect's captures — do not write one";

pub(super) const CONTEXT_MODULE: &str =
    "`createContext` belongs at module scope so the context identity is stable across renders";

pub(super) const CONTEXT_ARG: &str =
    "`useContext` needs a context from `createContext`, passed by name, so the compiler can prove a Provider";

pub(super) const CONTEXT_DEFAULT: &str =
    "give this context a default (`createContext<T>(default)`) or wrap the render in `<Ctx.Provider>`";

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

    pub(super) fn note_hook_call(&mut self, name: &'a str, span: ast::Span, builtin: bool) {
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
        if !builtin {
            if let Some(caller) = self.hook_fn_name {
                if name != "hook" {
                    self.fn_hook_calls.entry(caller).or_default().insert(name);
                }
            }
        }
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

    pub(super) fn is_use_context_binding(&self, name: &str) -> bool {
        name == "useContext" || self.lookup_capture(name) == Some(CaptureClass::UseContext)
    }

    pub(super) fn is_create_context_binding(&self, name: &str) -> bool {
        name == "createContext" || self.lookup_capture(name) == Some(CaptureClass::CreateContext)
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
        if self.is_create_context_expr(value) {
            return CaptureClass::CreateContext;
        }
        if self.is_use_context_expr(value) {
            return CaptureClass::UseContext;
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

    fn is_create_context_expr(&self, expr: &ast::Expr<'a>) -> bool {
        match peel(expr) {
            ast::Expr::Identifier { name, .. } => self.is_create_context_binding(name),
            _ => false,
        }
    }

    fn is_use_context_expr(&self, expr: &ast::Expr<'a>) -> bool {
        match peel(expr) {
            ast::Expr::Identifier { name, .. } => self.is_use_context_binding(name),
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
            Some((
                _,
                CaptureClass::Stable
                | CaptureClass::UseEffect
                | CaptureClass::CreateContext
                | CaptureClass::UseContext,
            )) => CaptureDecision::Skip,
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
        if reserved_hook_name(name).is_some() || is_react_builtin(name) {
            let kind = if name == "createContext" {
                "compiler-known React builtin"
            } else {
                "compiler-known hook"
            };
            self.error_span(span, format!("cannot shadow {kind} `{name}`"));
        }
    }

    /// Straight-line position where inserting a hook does not change call order.
    fn memo_position_ok(&self) -> bool {
        self.in_function
            && (self.hook_is_component || self.hook_body_called)
            && self.hook_conditional_depth == 0
            && self.loop_depth == 0
            && !self.hook_seen_return
    }

    /// Wrap a component-body const initializer if it is a pure expression
    /// over classified captures, or a function (useCallback).
    pub(super) fn try_auto_memo(&mut self, expr: &ast::Expr<'a>) -> bool {
        if !self.memo_position_ok() {
            return false;
        }
        let key = expr as *const ast::Expr<'a>;
        if self.memo_sites.contains_key(&key) {
            return false;
        }
        let kind = match peel(expr) {
            ast::Expr::Function { is_async: true, .. } => return false,
            ast::Expr::Function { .. } => MemoKind::UseCallback,
            _ => MemoKind::UseMemo,
        };
        if kind == MemoKind::UseMemo {
            if is_trivial_memo_expr(expr) || self.is_hook_call_expr(expr) {
                return false;
            }
            if !self.expr_is_memo_pure(expr) {
                return false;
            }
        }
        let frees = collect_expr_frees(expr);
        let Some(deps) = self.infer_classified_deps(&frees) else {
            return false;
        };
        let builtin = match kind {
            MemoKind::UseMemo => "useMemo",
            MemoKind::UseCallback => "useCallback",
        };
        self.note_hook_builtin_ref(builtin);
        self.note_hook_call(builtin, expr.span(), true);
        self.memo_sites.insert(key, MemoSite { kind, deps });
        true
    }

    /// JSX attribute values wrap only inline function expressions.
    pub(super) fn consider_jsx_callback(&mut self, expr: &ast::Expr<'a>) {
        if matches!(peel(expr), ast::Expr::Function { .. }) {
            self.try_auto_memo(expr);
        }
    }

    /// Same capture classification as `useEffect` deps: unclassified means
    /// "do not guess", so auto-memo skips rather than diagnosing.
    fn infer_classified_deps(&self, frees: &[(&'a str, ast::Span)]) -> Option<Vec<&'a str>> {
        let mut deps = Vec::new();
        for &(name, _) in frees {
            match self.classify_capture(name) {
                CaptureDecision::Skip => {}
                CaptureDecision::Dep => {
                    if !deps.contains(&name) {
                        deps.push(name);
                    }
                }
                CaptureDecision::Unclassified => return None,
            }
        }
        Some(deps)
    }

    fn expr_is_memo_pure(&self, expr: &ast::Expr<'a>) -> bool {
        let mut impure = false;
        crate::visit::walk_expr(expr, &mut |node| {
            if self.node_is_memo_impure(node) {
                impure = true;
            }
        });
        !impure
    }

    fn node_is_memo_impure(&self, expr: &ast::Expr<'a>) -> bool {
        match expr {
            ast::Expr::Unsafe { .. }
            | ast::Expr::Await { .. }
            | ast::Expr::Bridge { .. }
            | ast::Expr::Build { .. }
            | ast::Expr::Match { .. }
            | ast::Expr::JsxElement { .. }
            | ast::Expr::JsxFragment { .. }
            | ast::Expr::Function { .. } => true,
            ast::Expr::Binary { op, .. }
                if matches!(
                    *op,
                    ast::BinOp::Assign
                        | ast::BinOp::AddAssign
                        | ast::BinOp::SubAssign
                        | ast::BinOp::MulAssign
                        | ast::BinOp::DivAssign
                        | ast::BinOp::ModAssign
                        | ast::BinOp::Pipe
                ) =>
            {
                true
            }
            ast::Expr::Call { callee, .. } => {
                self.is_hook_call_expr(expr)
                    || self.callee_is_setter(callee)
                    || self.callee_is_summon(callee)
                    || self.call_is_mutating_method(callee)
                    || !self.call_is_known_pure(expr)
            }
            ast::Expr::FieldAccess {
                object,
                field: "current",
                ..
            } => self.expr_is_ref(object),
            _ => false,
        }
    }

    fn call_is_known_pure(&self, expr: &ast::Expr<'a>) -> bool {
        let key = expr as *const _;
        self.unwrap_calls.contains_key(&key)
            || self.number_math_calls.contains_key(&key)
            || self.array_builtin_calls.get(&key) == Some(&super::types::ArrayAccess::Has)
    }

    fn callee_is_setter(&self, callee: &ast::Expr<'a>) -> bool {
        match peel(callee) {
            ast::Expr::Identifier { name, .. } => {
                matches!(
                    self.lookup_var(name),
                    Some(Type::Generic { base: "Setter", .. })
                )
            }
            _ => false,
        }
    }

    fn callee_is_summon(&self, callee: &ast::Expr<'a>) -> bool {
        match peel(callee) {
            ast::Expr::Identifier { name, .. } => self.program.statements.iter().any(|stmt| {
                matches!(
                    stmt,
                    ast::Stmt::Summon { functions, .. }
                        if functions.iter().any(|f| f.name == *name)
                )
            }),
            _ => false,
        }
    }

    fn call_is_mutating_method(&self, callee: &ast::Expr<'a>) -> bool {
        match peel(callee) {
            ast::Expr::FieldAccess { field, .. } => matches!(
                *field,
                "push" | "pop" | "shift" | "unshift" | "splice" | "sort" | "reverse" | "fill"
            ),
            _ => false,
        }
    }

    fn expr_is_ref(&self, expr: &ast::Expr<'a>) -> bool {
        match peel(expr) {
            ast::Expr::Identifier { name, .. } => {
                matches!(
                    self.lookup_var(name),
                    Some(Type::Generic { base: "Ref", .. })
                )
            }
            _ => false,
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

    pub(super) fn builtin_create_context_type() -> Type<'a> {
        let t = Type::Param { name: "T" };
        Type::Function {
            params: vec![t.clone()],
            ret: Box::new(Type::context(t, true)),
            optional: 1,
        }
    }

    pub(super) fn builtin_use_context_type() -> Type<'a> {
        let t = Type::Param { name: "T" };
        Type::Function {
            params: vec![Type::context(t.clone(), true)],
            ret: Box::new(t),
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

    pub(super) fn check_create_context(
        &mut self,
        expr: &ast::Expr<'a>,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        self.note_hook_builtin_ref("createContext");
        if self.in_function {
            self.error_span(span, CONTEXT_MODULE);
        }
        if type_args.len() > 1 {
            self.error_span(span, "`createContext` takes at most one type argument");
        }
        if args.len() > 1 {
            self.error_span(
                span,
                "`createContext` takes a default value, or none if a Provider must wrap every use",
            );
            for extra in args.iter().skip(1) {
                self.check_expr(extra);
            }
        }
        let has_default = !args.is_empty();
        let arg_type = if let Some(arg) = args.first() {
            self.check_expr(arg)
        } else {
            Type::Var
        };
        let t = if let Some(ty) = type_args.first() {
            let expected = self.resolve_ast_type(ty);
            if has_default && !self.is_assignable(&expected, &arg_type) {
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
        } else if has_default {
            arg_type
        } else {
            self.error_span(
                span,
                "`createContext` without a default needs a type argument, e.g. `createContext<Locale>()`",
            );
            Type::Error
        };
        if has_default {
            self.reject_none_hook_init(type_args, &t, span);
        }
        let ty = Type::context(t, has_default);
        self.intern_create_context(expr, has_default);
        ty
    }

    pub(super) fn check_use_context(
        &mut self,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        self.note_hook_builtin_ref("useContext");
        self.note_hook_call("useContext", span, true);
        if !type_args.is_empty() {
            self.error_span(span, "`useContext` does not take type arguments");
        }
        if args.len() != 1 {
            self.error_span(span, "`useContext` takes one context from `createContext`");
            for extra in args.iter() {
                self.check_expr(extra);
            }
            return Type::Error;
        }
        let arg_type = self.check_expr(&args[0]);
        let Some(value) = arg_type.context_value_type().cloned() else {
            if !arg_type.is_error() {
                self.error_at_expr(
                    &args[0],
                    format!("expected a `Context<T>`, found type `{arg_type}`"),
                );
            }
            return Type::Error;
        };
        match self.context_id_of_expr(&args[0]) {
            Some(id) => self.record_context_use(id, span),
            None if arg_type.context_has_default() => {}
            None => {
                self.error_at_expr(&args[0], CONTEXT_ARG);
            }
        }
        value
    }

    pub(super) fn intern_imported_context(&mut self, name: &'a str, ty: &Type<'a>) {
        let Some(_) = ty.context_value_type() else {
            return;
        };
        let id = self.next_context_id;
        self.next_context_id += 1;
        self.contexts.insert(
            id,
            ContextInfo {
                name,
                has_default: ty.context_has_default(),
            },
        );
        if let Some(scope) = self.context_scopes.last_mut() {
            scope.insert(name, id);
        }
    }

    pub(super) fn bind_context_from_value(&mut self, name: &'a str, value: &ast::Expr<'a>) {
        if let Some(id) = self.context_id_of_expr(value) {
            if let Some(scope) = self.context_scopes.last_mut() {
                scope.insert(name, id);
            }
            if let Some(info) = self.contexts.get_mut(&id) {
                if info.name == "context" {
                    info.name = name;
                }
            }
        }
    }

    pub(super) fn seed_create_context_type(&mut self, value: &ast::Expr<'a>) -> Option<Type<'a>> {
        let ast::Expr::Call {
            callee,
            type_args,
            args,
            span,
            ..
        } = peel(value)
        else {
            return None;
        };
        let ast::Expr::Identifier { name, .. } = peel(callee) else {
            return None;
        };
        if !self.is_create_context_binding(name) {
            return None;
        }
        Some(self.check_create_context(value, type_args, args, *span))
    }

    pub(super) fn note_provider_element(
        &mut self,
        element: &ast::JsxElement<'a>,
        context_name: &'a str,
    ) {
        if let Some(id) = self.lookup_context(context_name) {
            self.provider_elements.insert(element as *const _, id);
        }
    }

    pub(super) fn lookup_context(&self, name: &str) -> Option<usize> {
        for scope in self.context_scopes.iter().rev() {
            if let Some(id) = scope.get(name) {
                return Some(*id);
            }
        }
        None
    }

    fn intern_create_context(&mut self, expr: &ast::Expr<'a>, has_default: bool) -> usize {
        let key = peel(expr) as *const _;
        if let Some(id) = self.context_by_expr.get(&key) {
            return *id;
        }
        let id = self.next_context_id;
        self.next_context_id += 1;
        self.contexts.insert(
            id,
            ContextInfo {
                name: "context",
                has_default,
            },
        );
        self.context_by_expr.insert(key, id);
        id
    }

    fn context_id_of_expr(&self, expr: &ast::Expr<'a>) -> Option<usize> {
        match peel(expr) {
            ast::Expr::Identifier { name, .. } => self.lookup_context(name),
            ast::Expr::Call { .. } => self.context_by_expr.get(&(peel(expr) as *const _)).copied(),
            _ => None,
        }
    }

    fn record_context_use(&mut self, id: usize, span: ast::Span) {
        if let Some(name) = self.hook_fn_name {
            self.fn_uses_context
                .entry(name)
                .or_default()
                .push((id, span));
        }
        if let Some(body) = self.current_body {
            self.body_uses_context
                .entry(body as *const _)
                .or_default()
                .push((id, span));
        }
    }

    pub(super) fn prove_provider_presence(&mut self) {
        if self.infer_only {
            return;
        }
        let consumes = self.saturated_context_consumes();
        let bodies = named_function_bodies(self.program);
        let exported = exported_names(self.program);
        let children_slots = self.children_slot_providers(&bodies);
        let referenced = jsx_component_tags(self.program);
        let mut reached: HashSet<&str> = HashSet::new();
        let mut diagnosed: HashSet<(u32, usize)> = HashSet::new();
        let empty = HashSet::new();

        // Module-level render roots. Providers thread down through inlined
        // same-module components; a callee reached only under a Provider
        // inherits that proof.
        for stmt in self.program.statements {
            match stmt {
                ast::Stmt::Function { .. }
                | ast::Stmt::ReceiverMethod { .. }
                | ast::Stmt::Export {
                    decl: ast::ExportDecl::Function { .. },
                    ..
                } => {}
                _ => {
                    self.walk_stmt_for_providers(
                        stmt,
                        &empty,
                        &consumes,
                        &bodies,
                        &children_slots,
                        &mut reached,
                        &mut diagnosed,
                        &mut Vec::new(),
                    );
                }
            }
        }

        // Exported components are library entries: the export itself is an
        // unproven render path, even when every intra-module site is wrapped.
        let names: Vec<&'a str> = bodies.keys().copied().collect();
        for name in &names {
            if !exported.contains(name) {
                continue;
            }
            let body = bodies.get(name).copied().unwrap_or(&[]);
            self.enter_function_with_providers(
                name,
                body,
                &empty,
                None,
                true,
                &consumes,
                &bodies,
                &children_slots,
                &mut reached,
                &mut diagnosed,
                &mut Vec::new(),
            );
        }

        // Unexported functions with no JSX incoming edges, and not already
        // reached from a module-level root, still seed a render tree so an
        // unexported App without `const root = <App />` remains visible.
        // Their own consumption is not diagnosed (they are not library
        // entries). JSX-referenced components are never re-walked from an
        // empty provided set: they inherit the intersection of incoming
        // proofs, and one unprovided path diagnoses that path by name.
        for name in names {
            if exported.contains(name) || referenced.contains(name) || reached.contains(name) {
                continue;
            }
            let body = bodies.get(name).copied().unwrap_or(&[]);
            self.enter_function_with_providers(
                name,
                body,
                &empty,
                None,
                false,
                &consumes,
                &bodies,
                &children_slots,
                &mut reached,
                &mut diagnosed,
                &mut Vec::new(),
            );
        }
    }

    fn saturated_context_consumes(&self) -> HashMap<&'a str, Vec<(usize, ast::Span)>> {
        let mut consumes = self.fn_uses_context.clone();
        for (name, body) in named_function_bodies(self.program) {
            if let Some(uses) = self.body_uses_context.get(&(body as *const _)) {
                consumes
                    .entry(name)
                    .or_default()
                    .extend(uses.iter().copied());
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            let callers: Vec<_> = self.fn_hook_calls.keys().copied().collect();
            for caller in callers {
                let Some(callees) = self.fn_hook_calls.get(caller) else {
                    continue;
                };
                let mut extra = Vec::new();
                for callee in callees {
                    if let Some(uses) = consumes.get(callee) {
                        extra.extend(uses.iter().copied());
                    }
                }
                if extra.is_empty() {
                    continue;
                }
                let entry = consumes.entry(caller).or_default();
                for item in extra {
                    if !entry
                        .iter()
                        .any(|e| e.0 == item.0 && e.1.byte_start == item.1.byte_start)
                    {
                        entry.push(item);
                        changed = true;
                    }
                }
            }
        }
        consumes
    }

    fn children_slot_providers(
        &self,
        bodies: &HashMap<&'a str, &'a [ast::Stmt<'a>]>,
    ) -> HashMap<&'a str, HashSet<usize>> {
        let mut out = HashMap::new();
        for (name, body) in bodies {
            let params = function_param_names(self.program, name);
            let mut found = Vec::new();
            let mut provided = HashSet::new();
            self.collect_children_slots(body, &params, &mut provided, &mut found);
            if !found.is_empty() {
                let mut inter = found[0].clone();
                for set in found.iter().skip(1) {
                    inter = inter.intersection(set).copied().collect();
                }
                out.insert(*name, inter);
            }
        }
        out
    }

    fn collect_children_slots(
        &self,
        stmts: &'a [ast::Stmt<'a>],
        params: &FunctionParams<'a>,
        provided: &mut HashSet<usize>,
        found: &mut Vec<HashSet<usize>>,
    ) {
        for stmt in stmts {
            self.collect_children_slots_stmt(stmt, params, provided, found);
        }
    }

    fn collect_children_slots_stmt(
        &self,
        stmt: &'a ast::Stmt<'a>,
        params: &FunctionParams<'a>,
        provided: &mut HashSet<usize>,
        found: &mut Vec<HashSet<usize>>,
    ) {
        match stmt {
            ast::Stmt::Const { value, .. }
            | ast::Stmt::Let { value, .. }
            | ast::Stmt::Expr { expr: value, .. }
            | ast::Stmt::Return {
                value: Some(value), ..
            } => self.collect_children_slots_expr(value, params, provided, found),
            ast::Stmt::If {
                then_body,
                else_body,
                condition,
                ..
            } => {
                self.collect_children_slots_expr(condition, params, provided, found);
                self.collect_children_slots(then_body, params, provided, found);
                self.collect_children_slots(else_body, params, provided, found);
            }
            ast::Stmt::Block { body, .. } => {
                self.collect_children_slots(body, params, provided, found)
            }
            ast::Stmt::Function { body, .. } => {
                self.collect_children_slots(body, params, provided, found)
            }
            ast::Stmt::Export {
                decl: ast::ExportDecl::Const { value, .. },
                ..
            } => self.collect_children_slots_expr(value, params, provided, found),
            _ => {}
        }
    }

    fn collect_children_slots_expr(
        &self,
        expr: &'a ast::Expr<'a>,
        params: &FunctionParams<'a>,
        provided: &mut HashSet<usize>,
        found: &mut Vec<HashSet<usize>>,
    ) {
        match expr {
            ast::Expr::JsxElement { element, .. } => {
                let extra = self.provider_elements.get(&(element as *const _)).copied();
                if let Some(id) = extra {
                    provided.insert(id);
                }
                if is_children_slot_expr(element.children, params) {
                    found.push(provided.clone());
                }
                for child in element.children {
                    self.collect_children_slots_expr(child, params, provided, found);
                }
                for attr in element.attributes {
                    if let Some(value) = &attr.value {
                        self.collect_children_slots_expr(value, params, provided, found);
                    }
                }
                if let Some(id) = extra {
                    provided.remove(&id);
                }
            }
            ast::Expr::JsxFragment { children, .. } => {
                for child in *children {
                    self.collect_children_slots_expr(child, params, provided, found);
                }
            }
            ast::Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.collect_children_slots_expr(condition, params, provided, found);
                self.collect_children_slots_expr(then_branch, params, provided, found);
                self.collect_children_slots_expr(else_branch, params, provided, found);
            }
            ast::Expr::Paren { expr, .. }
            | ast::Expr::Safe { expr, .. }
            | ast::Expr::Await { expr, .. } => {
                self.collect_children_slots_expr(expr, params, provided, found)
            }
            ast::Expr::Function { body, .. } => {
                self.collect_children_slots(body, params, provided, found)
            }
            ast::Expr::Call { args, callee, .. } => {
                self.collect_children_slots_expr(callee, params, provided, found);
                for arg in *args {
                    self.collect_children_slots_expr(arg, params, provided, found);
                }
            }
            ast::Expr::Array { elements, .. } => {
                for el in *elements {
                    self.collect_children_slots_expr(el, params, provided, found);
                }
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_stmt_for_providers(
        &mut self,
        stmt: &ast::Stmt<'a>,
        provided: &HashSet<usize>,
        consumes: &HashMap<&'a str, Vec<(usize, ast::Span)>>,
        bodies: &HashMap<&'a str, &'a [ast::Stmt<'a>]>,
        children_slots: &HashMap<&'a str, HashSet<usize>>,
        reached: &mut HashSet<&'a str>,
        diagnosed: &mut HashSet<(u32, usize)>,
        path: &mut Vec<&'a str>,
    ) {
        match stmt {
            ast::Stmt::Const { value, .. }
            | ast::Stmt::Let { value, .. }
            | ast::Stmt::Expr { expr: value, .. }
            | ast::Stmt::Return {
                value: Some(value), ..
            } => self.walk_expr_for_providers(
                value,
                provided,
                consumes,
                bodies,
                children_slots,
                reached,
                diagnosed,
                path,
            ),
            ast::Stmt::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.walk_expr_for_providers(
                    condition,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
                for s in then_body.iter().chain(else_body.iter()) {
                    self.walk_stmt_for_providers(
                        s,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Stmt::Block { body, .. } | ast::Stmt::Function { body, .. } => {
                for s in *body {
                    self.walk_stmt_for_providers(
                        s,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Stmt::Export {
                decl: ast::ExportDecl::Const { value, .. },
                ..
            } => self.walk_expr_for_providers(
                value,
                provided,
                consumes,
                bodies,
                children_slots,
                reached,
                diagnosed,
                path,
            ),
            ast::Stmt::TupleBinding { value, .. } => self.walk_expr_for_providers(
                value,
                provided,
                consumes,
                bodies,
                children_slots,
                reached,
                diagnosed,
                path,
            ),
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_expr_for_providers(
        &mut self,
        expr: &ast::Expr<'a>,
        provided: &HashSet<usize>,
        consumes: &HashMap<&'a str, Vec<(usize, ast::Span)>>,
        bodies: &HashMap<&'a str, &'a [ast::Stmt<'a>]>,
        children_slots: &HashMap<&'a str, HashSet<usize>>,
        reached: &mut HashSet<&'a str>,
        diagnosed: &mut HashSet<(u32, usize)>,
        path: &mut Vec<&'a str>,
    ) {
        match expr {
            ast::Expr::JsxElement { element, .. } => {
                let mut child_provided = provided.clone();
                if let Some(id) = self.provider_elements.get(&(element as *const _)) {
                    child_provided.insert(*id);
                } else if let Some(ctx) = element.context_provider() {
                    if let Some(id) = self.lookup_context(ctx) {
                        child_provided.insert(id);
                    }
                } else if element
                    .tag
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
                {
                    self.enter_function_with_providers(
                        element.tag,
                        bodies.get(element.tag).copied().unwrap_or(&[]),
                        provided,
                        Some(element.span),
                        true,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                    if let Some(extra) = children_slots.get(element.tag) {
                        child_provided.extend(extra.iter().copied());
                    }
                }
                for attr in element.attributes {
                    if let Some(value) = &attr.value {
                        self.walk_expr_for_providers(
                            value,
                            provided,
                            consumes,
                            bodies,
                            children_slots,
                            reached,
                            diagnosed,
                            path,
                        );
                    }
                }
                for child in element.children {
                    self.walk_expr_for_providers(
                        child,
                        &child_provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Expr::JsxFragment { children, .. } => {
                for child in *children {
                    self.walk_expr_for_providers(
                        child,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.walk_expr_for_providers(
                    condition,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
                self.walk_expr_for_providers(
                    then_branch,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
                self.walk_expr_for_providers(
                    else_branch,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
            }
            ast::Expr::Paren { expr, .. }
            | ast::Expr::Safe { expr, .. }
            | ast::Expr::Await { expr, .. }
            | ast::Expr::Spread { expr, .. }
            | ast::Expr::FieldAccess { object: expr, .. } => self.walk_expr_for_providers(
                expr,
                provided,
                consumes,
                bodies,
                children_slots,
                reached,
                diagnosed,
                path,
            ),
            ast::Expr::Call { callee, args, .. } => {
                self.walk_expr_for_providers(
                    callee,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
                for arg in *args {
                    self.walk_expr_for_providers(
                        arg,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Expr::Array { elements, .. } => {
                for el in *elements {
                    self.walk_expr_for_providers(
                        el,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Expr::Function { body, .. } => {
                for stmt in *body {
                    self.walk_stmt_for_providers(
                        stmt,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Expr::Binary { left, right, .. } => {
                self.walk_expr_for_providers(
                    left,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
                self.walk_expr_for_providers(
                    right,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
            }
            ast::Expr::Unary { operand, .. } => self.walk_expr_for_providers(
                operand,
                provided,
                consumes,
                bodies,
                children_slots,
                reached,
                diagnosed,
                path,
            ),
            ast::Expr::IndexAccess { object, index, .. } => {
                self.walk_expr_for_providers(
                    object,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
                self.walk_expr_for_providers(
                    index,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
            }
            ast::Expr::Object { fields, .. } => {
                for field in *fields {
                    self.walk_expr_for_providers(
                        &field.value,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Expr::StructLiteral { fields, .. } => {
                for field in *fields {
                    self.walk_expr_for_providers(
                        &field.value,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            ast::Expr::Match {
                scrutinee, arms, ..
            } => {
                self.walk_expr_for_providers(
                    scrutinee,
                    provided,
                    consumes,
                    bodies,
                    children_slots,
                    reached,
                    diagnosed,
                    path,
                );
                for arm in *arms {
                    self.walk_expr_for_providers(
                        &arm.body,
                        provided,
                        consumes,
                        bodies,
                        children_slots,
                        reached,
                        diagnosed,
                        path,
                    );
                }
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn enter_function_with_providers(
        &mut self,
        name: &'a str,
        body: &'a [ast::Stmt<'a>],
        provided: &HashSet<usize>,
        site: Option<ast::Span>,
        check_self: bool,
        consumes: &HashMap<&'a str, Vec<(usize, ast::Span)>>,
        bodies: &HashMap<&'a str, &'a [ast::Stmt<'a>]>,
        children_slots: &HashMap<&'a str, HashSet<usize>>,
        reached: &mut HashSet<&'a str>,
        diagnosed: &mut HashSet<(u32, usize)>,
        path: &mut Vec<&'a str>,
    ) {
        reached.insert(name);
        let cyclic = path.iter().any(|n| *n == name);
        if !cyclic {
            path.push(name);
        }
        if check_self {
            if let Some(uses) = consumes.get(name) {
                for (id, use_span) in uses {
                    let info = self.contexts.get(id);
                    if info.is_some_and(|c| c.has_default) || provided.contains(id) {
                        continue;
                    }
                    let span = site.unwrap_or(*use_span);
                    self.diagnose_missing_provider(name, *id, span, diagnosed, path);
                }
            }
        }
        if cyclic {
            return;
        }
        for stmt in body {
            self.walk_stmt_for_providers(
                stmt,
                provided,
                consumes,
                bodies,
                children_slots,
                reached,
                diagnosed,
                path,
            );
        }
        path.pop();
    }

    fn diagnose_missing_provider(
        &mut self,
        consumer: &'a str,
        id: usize,
        span: ast::Span,
        diagnosed: &mut HashSet<(u32, usize)>,
        path: &[&'a str],
    ) {
        let Some(info) = self.contexts.get(&id).cloned() else {
            return;
        };
        if info.has_default {
            return;
        }
        if !diagnosed.insert((span.byte_start as u32, id)) {
            return;
        }
        let via = if path.len() > 1 {
            let hops = path
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(" → ");
            format!(" (via {hops})")
        } else {
            String::new()
        };
        self.error_span(
            span,
            format!(
                "`{consumer}` reads `{name}` but this render is not wrapped in `<{name}.Provider>`{via}; {CONTEXT_DEFAULT}",
                name = info.name
            ),
        );
    }
}

struct FunctionParams<'a> {
    first: Option<&'a str>,
    children: bool,
}

fn function_param_names<'a>(program: &'a ast::Program<'a>, name: &str) -> FunctionParams<'a> {
    for stmt in program.statements {
        let params = match stmt {
            ast::Stmt::Function {
                name: fn_name,
                params,
                ..
            }
            | ast::Stmt::Export {
                decl:
                    ast::ExportDecl::Function {
                        name: fn_name,
                        params,
                        ..
                    },
                ..
            } if *fn_name == name => *params,
            _ => continue,
        };
        let first = params.first().map(|p| p.name);
        let children = first == Some("children");
        return FunctionParams { first, children };
    }
    FunctionParams {
        first: None,
        children: false,
    }
}

fn is_children_slot_expr(children: &[ast::Expr<'_>], params: &FunctionParams<'_>) -> bool {
    children.iter().any(|child| match peel(child) {
        ast::Expr::Identifier { name, .. } => params.children && *name == "children",
        ast::Expr::FieldAccess {
            object,
            field: "children",
            ..
        } => match peel(object) {
            ast::Expr::Identifier { name, .. } => params.first == Some(*name),
            _ => false,
        },
        _ => false,
    })
}

fn named_function_bodies<'a>(
    program: &'a ast::Program<'a>,
) -> HashMap<&'a str, &'a [ast::Stmt<'a>]> {
    let mut bodies = HashMap::new();
    for stmt in program.statements {
        match stmt {
            ast::Stmt::Function { name, body, .. }
            | ast::Stmt::Export {
                decl: ast::ExportDecl::Function { name, body, .. },
                ..
            } => {
                bodies.insert(*name, *body);
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
                bodies.insert(*name, *body);
            }
            _ => {}
        }
    }
    bodies
}

/// Uppercase JSX tags that name a component in this module. Used to find
/// functions that have an incoming render edge so they are not re-walked
/// from an empty provided set.
fn jsx_component_tags(program: &ast::Program<'_>) -> HashSet<String> {
    let mut tags = HashSet::new();
    for stmt in program.statements {
        crate::visit::walk_stmt(stmt, &mut |expr| {
            let ast::Expr::JsxElement { element, .. } = expr else {
                return;
            };
            if element.context_provider().is_some() {
                return;
            }
            if element
                .tag
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
            {
                tags.insert(element.tag.to_string());
            }
        });
    }
    tags
}

fn exported_names<'a>(program: &'a ast::Program<'a>) -> HashSet<&'a str> {
    let mut names = HashSet::new();
    for stmt in program.statements {
        match stmt {
            ast::Stmt::Export {
                decl: ast::ExportDecl::Function { name, .. },
                ..
            }
            | ast::Stmt::Export {
                decl: ast::ExportDecl::Const { name, .. },
                ..
            } => {
                names.insert(*name);
            }
            ast::Stmt::Export {
                decl: ast::ExportDecl::NamedGroup { names: group, .. },
                ..
            } => {
                for n in *group {
                    names.insert(n.name);
                }
            }
            _ => {}
        }
    }
    names
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

fn collect_expr_frees<'a>(expr: &ast::Expr<'a>) -> Vec<(&'a str, ast::Span)> {
    let mut walker = CaptureWalker {
        bound: vec![HashSet::new()],
        seen: HashSet::new(),
        frees: Vec::new(),
    };
    walker.expr(expr);
    walker.frees
}

fn is_trivial_memo_expr(expr: &ast::Expr<'_>) -> bool {
    matches!(
        peel(expr),
        ast::Expr::Identifier { .. }
            | ast::Expr::Number { .. }
            | ast::Expr::BigInt { .. }
            | ast::Expr::String { .. }
            | ast::Expr::Boolean { .. }
            | ast::Expr::None { .. }
            | ast::Expr::JsxText { .. }
    )
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

    fn stmts(&mut self, stmts: &[ast::Stmt<'a>]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &ast::Stmt<'a>) {
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

    fn expr(&mut self, expr: &ast::Expr<'a>) {
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

    fn match_arm(&mut self, arm: &ast::MatchArm<'a>) {
        self.push();
        self.pattern(&arm.pattern);
        if let Some(guard) = &arm.guard {
            self.expr(guard);
        }
        self.expr(&arm.body);
        self.pop();
    }

    fn pattern(&mut self, pattern: &ast::Pattern<'a>) {
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
