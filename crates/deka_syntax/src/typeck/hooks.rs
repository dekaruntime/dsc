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

use super::{types::Type, Checker};
use crate::ast;

pub(super) const STRAIGHT_LINE: &str =
    "hooks run in a fixed order every render; move the condition inside the hook";

pub(super) const HOOK_ASSIGN: &str = "this closure calls a hook; hooks run only during render — accept a hook-typed parameter or lift the hook to the component";

/// `None` is not a state type: it is the empty Option payload, and without an
/// annotation `useState(None)` infers `T = none` rather than `Option<T>`.
pub(super) const NONE_INIT: &str =
    "`None` is not a state type; pass an explicit Option, e.g. `useState<Option<number>>(None)`, or a concrete initial value";

pub(super) fn is_hook_builtin(name: &str) -> bool {
    matches!(name, "useState" | "useRef")
}

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

    pub(super) fn note_hook_builtin_ref(&mut self, name: &'static str) {
        self.hook_builtin_refs.insert(name);
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
