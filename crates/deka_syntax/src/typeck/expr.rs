//! Expression typechecking.

use std::collections::{HashMap, HashSet};

use crate::ast;

use super::types::Type;
use super::Checker;

fn is_panic_callee(callee: &ast::Expr<'_>) -> bool {
    match callee {
        ast::Expr::Identifier { name: "panic", .. } => true,
        ast::Expr::FieldAccess {
            object,
            field: "panic",
            ..
        } => matches!(object, ast::Expr::Identifier { name: "deka", .. }),
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum PrimitiveConversionName {
    String,
    ParseNumber,
    UnboxNumber,
    ToNumber,
}

fn primitive_conversion_name(name: &str) -> Option<PrimitiveConversionName> {
    match name {
        "string" => Some(PrimitiveConversionName::String),
        "parseNumber" => Some(PrimitiveConversionName::ParseNumber),
        "unboxNumber" => Some(PrimitiveConversionName::UnboxNumber),
        "toNumber" => Some(PrimitiveConversionName::ToNumber),
        _ => None,
    }
}

fn is_mutating_array_method(name: &str) -> bool {
    matches!(
        name,
        "push"
            | "pop"
            | "shift"
            | "unshift"
            | "splice"
            | "sort"
            | "reverse"
            | "fill"
            | "copyWithin"
    )
}

/// How completely a set of match arms covers a scrutinee type.
///
/// `All` means an irrefutable pattern was seen. `Cases` records, per
/// constructor name, how completely that case's *payload* is covered — which
/// is what makes `Ok(Some(v))` distinguishable from `Ok(_)` (deka#396).
#[derive(Debug, Clone)]
pub(super) enum Coverage<'a> {
    All,
    Cases(HashMap<&'a str, Coverage<'a>>),
}

impl<'a> Coverage<'a> {
    fn nothing() -> Self {
        Coverage::Cases(HashMap::new())
    }

    /// Coverage for the arm `unwrap` supplies itself (deka#445).
    pub(super) fn success_case(name: &'a str) -> Self {
        let mut cases = HashMap::new();
        cases.insert(name, Coverage::All);
        Coverage::Cases(cases)
    }

    pub(super) fn of_pattern(
        pattern: &ast::Pattern<'a>,
        cases: &HashMap<*const ast::Pattern<'a>, &'a str>,
    ) -> Self {
        match pattern {
            // An identifier that resolved to a payload-free case covers that
            // case only; one that binds covers everything (deka#450).
            ast::Pattern::Identifier { .. } => {
                match cases.get(&(pattern as *const ast::Pattern<'a>)) {
                    Some(case) => {
                        let mut covered = HashMap::new();
                        covered.insert(*case, Coverage::All);
                        Coverage::Cases(covered)
                    }
                    None => Coverage::All,
                }
            }
            ast::Pattern::Wildcard { .. } => Coverage::All,
            ast::Pattern::Constructor { name, payload, .. } => {
                let inner = match payload {
                    Some(inner) => Coverage::of_pattern(inner, cases),
                    None => Coverage::All,
                };
                let mut cases = HashMap::new();
                cases.insert(*name, inner);
                Coverage::Cases(cases)
            }
            // A literal matches one value, never a whole case.
            // An or-pattern covers everything all of its alternatives do, so
            // `Timeout | NotFound` counts for both in the exhaustiveness check.
            ast::Pattern::Or { alternatives, .. } => alternatives
                .iter()
                .map(|alternative| Coverage::of_pattern(alternative, cases))
                .fold(Coverage::nothing(), Coverage::merge),
            ast::Pattern::Literal { .. }
            | ast::Pattern::Struct { .. }
            | ast::Pattern::Tuple { .. } => Coverage::nothing(),
        }
    }

    pub(super) fn merge(self, other: Coverage<'a>) -> Coverage<'a> {
        match (self, other) {
            (Coverage::All, _) | (_, Coverage::All) => Coverage::All,
            (Coverage::Cases(mut left), Coverage::Cases(right)) => {
                for (name, cov) in right {
                    let merged = match left.remove(name) {
                        Some(existing) => existing.merge(cov),
                        None => cov,
                    };
                    left.insert(name, merged);
                }
                Coverage::Cases(left)
            }
        }
    }
}

/// A builtin member of a primitive type or an array: either a plain property
/// (`length`) or a builtin method that JavaScript provides (`toUpperCase`).
/// User extensions shadow builtins only for call-shaped access; property-
/// shaped reads keep resolving to the builtin entry (deka#527).
#[derive(Debug)]
pub(super) enum PrimitiveMember<'a> {
    Property(Type<'a>),
    BuiltinMethod(Type<'a>),
}

fn fn0<'a>(ret: Type<'a>) -> Type<'a> {
    Type::Function {
        params: Vec::new(),
        ret: Box::new(ret),
        optional: 0,
    }
}

fn fn1<'a>(p: &Type<'a>, ret: &Type<'a>) -> Type<'a> {
    Type::Function {
        params: vec![p.clone()],
        ret: Box::new(ret.clone()),
        optional: 0,
    }
}

fn fn2<'a>(p1: &Type<'a>, p2: &Type<'a>, ret: &Type<'a>) -> Type<'a> {
    Type::Function {
        params: vec![p1.clone(), p2.clone()],
        ret: Box::new(ret.clone()),
        optional: 0,
    }
}

/// The builtin member table for primitives and arrays, keyed by
/// `(type_name, field)`. `elem` supplies the array element type for the
/// `"Array"` entries. Returns `None` for names outside the table.
pub(super) fn primitive_member<'a>(
    type_name: &str,
    field: &str,
    elem: Option<&Type<'a>>,
) -> Option<PrimitiveMember<'a>> {
    let string_ty = Type::Named { name: "string" };
    let number_ty = Type::Named { name: "number" };
    let boolean_ty = Type::Named { name: "boolean" };

    let member = match (type_name, field) {
        // `JsError` is whatever JavaScript threw, surfaced through the
        // emitter's try/catch. JS can throw anything -- `throw "boom"` has
        // no `.message` -- so the emitter normalises a non-Error throw into
        // an Error. These two fields are always present because of that
        // guarantee; without it, declaring them would be a lie the type
        // system could not catch (deka#460, deka#469).
        ("JsError", "message" | "name") => PrimitiveMember::Property(string_ty),
        // `Type` is the first-class runtime type descriptor returned by
        // `.getType()` (rfd#41, deka#529). Only `toString()` is in scope for
        // this slice; the rest of the descriptor API is deferred.
        ("Type", "toString") => PrimitiveMember::BuiltinMethod(fn0(string_ty)),
        ("string", "length") => PrimitiveMember::Property(number_ty),
        ("string", "toUpperCase" | "toLowerCase" | "trim") => {
            PrimitiveMember::BuiltinMethod(fn0(string_ty))
        }
        ("string", "charAt" | "indexOf" | "lastIndexOf") => {
            PrimitiveMember::BuiltinMethod(fn1(&number_ty, &number_ty))
        }
        ("string", "includes" | "startsWith" | "endsWith") => {
            PrimitiveMember::BuiltinMethod(fn1(&string_ty, &boolean_ty))
        }
        ("string", "slice") => {
            PrimitiveMember::BuiltinMethod(fn2(&number_ty, &number_ty, &string_ty))
        }
        ("string", "split") => PrimitiveMember::BuiltinMethod(fn1(
            &string_ty,
            &Type::Array {
                elem: Box::new(string_ty.clone()),
            },
        )),
        ("string", "replace" | "replaceAll" | "concat") => {
            PrimitiveMember::BuiltinMethod(fn2(&string_ty, &string_ty, &string_ty))
        }
        ("string", "substring") => {
            PrimitiveMember::BuiltinMethod(fn2(&number_ty, &number_ty, &string_ty))
        }
        // Math-backed methods on `number` (deka#378 step 2, rfd#40 phase 2).
        // Total functions return a plain `number`; partial functions — those
        // where JavaScript answers some inputs with `NaN` (`sqrt(-1)`,
        // `pow(-2, 0.5)`) — return `Option<number>`, so the emitted wrapper
        // cannot hand back a number that is not one (rfd#13). The emitter
        // rewrites every call to a `Math.*` expression (`number_math_calls`);
        // verbatim passthrough would be a runtime lie since JS numbers have
        // no such methods.
        ("number", "max" | "min") => PrimitiveMember::BuiltinMethod(fn1(&number_ty, &number_ty)),
        ("number", "pow") => PrimitiveMember::BuiltinMethod(fn1(
            &number_ty,
            &Type::Option {
                inner: Box::new(number_ty.clone()),
            },
        )),
        ("number", field) if NUMBER_MATH_TOTAL.contains(&field) => {
            PrimitiveMember::BuiltinMethod(fn0(number_ty))
        }
        ("number", field) if NUMBER_MATH_PARTIAL.contains(&field) => {
            PrimitiveMember::BuiltinMethod(fn0(Type::Option {
                inner: Box::new(number_ty),
            }))
        }
        ("Array", "length") => PrimitiveMember::Property(number_ty),
        ("Array", "includes") => {
            PrimitiveMember::BuiltinMethod(fn2(elem?, &number_ty, &boolean_ty))
        }
        ("Array", "slice") => PrimitiveMember::BuiltinMethod(fn2(
            &number_ty,
            &number_ty,
            &Type::Array {
                elem: Box::new(elem?.clone()),
            },
        )),
        ("Array", "push") => PrimitiveMember::BuiltinMethod(fn1(elem?, &number_ty)),
        ("Array", "pop") => PrimitiveMember::BuiltinMethod(fn0(Type::Option {
            inner: Box::new(elem?.clone()),
        })),
        // `first`/`last` have no JS builtin; `pop`/`shift` return raw values
        // where the type system declares Option<T>. The emitter rewrites all
        // four to a real Option construction at the site, recorded in
        // `array_builtin_calls` (deka#561, deka#566).
        ("Array", "first" | "last") => PrimitiveMember::BuiltinMethod(fn0(Type::Option {
            inner: Box::new(elem?.clone()),
        })),
        ("Array", "shift") => PrimitiveMember::BuiltinMethod(fn0(Type::Option {
            inner: Box::new(elem?.clone()),
        })),
        ("Array", "unshift") => PrimitiveMember::BuiltinMethod(fn1(elem?, &number_ty)),
        ("Array", "concat") => PrimitiveMember::BuiltinMethod(fn1(
            &Type::Array {
                elem: Box::new(elem?.clone()),
            },
            &Type::Array {
                elem: Box::new(elem?.clone()),
            },
        )),
        ("Array", "join") => PrimitiveMember::BuiltinMethod(fn1(&string_ty, &string_ty)),
        ("Array", "reverse" | "sort") => PrimitiveMember::BuiltinMethod(fn0(Type::Array {
            elem: Box::new(elem?.clone()),
        })),
        ("Array", "splice") => PrimitiveMember::BuiltinMethod(fn2(
            &number_ty,
            &number_ty,
            &Type::Array {
                elem: Box::new(elem?.clone()),
            },
        )),
        ("Array", "fill") => PrimitiveMember::BuiltinMethod(fn2(
            elem?,
            &number_ty,
            &Type::Array {
                elem: Box::new(elem?.clone()),
            },
        )),
        ("Array", "copyWithin") => PrimitiveMember::BuiltinMethod(fn2(
            &number_ty,
            &number_ty,
            &Type::Array {
                elem: Box::new(elem?.clone()),
            },
        )),
        ("Array", "filter") => PrimitiveMember::BuiltinMethod(fn1(
            &Type::Function {
                params: vec![elem?.clone()],
                ret: Box::new(boolean_ty.clone()),
                optional: 0,
            },
            &Type::Array {
                elem: Box::new(elem?.clone()),
            },
        )),
        // `map` is generic in a second parameter U that `elem` cannot
        // supply: (T -> U) -> Array<U>. U is a real type parameter, solved
        // from the callback at the call site by the same substitution
        // machinery generic functions use (deka#467).
        ("Array", "map") => PrimitiveMember::BuiltinMethod(Type::Function {
            params: vec![Type::Function {
                params: vec![elem?.clone()],
                ret: Box::new(Type::Param { name: "U" }),
                optional: 0,
            }],
            ret: Box::new(Type::Array {
                elem: Box::new(Type::Param { name: "U" }),
            }),
            optional: 0,
        }),
        ("Array", "find") => PrimitiveMember::BuiltinMethod(fn1(
            &Type::Function {
                params: vec![elem?.clone()],
                ret: Box::new(boolean_ty.clone()),
                optional: 0,
            },
            &Type::Option {
                inner: Box::new(elem?.clone()),
            },
        )),
        ("Array", "forEach") => PrimitiveMember::BuiltinMethod(fn1(
            &Type::Function {
                params: vec![elem?.clone()],
                ret: Box::new(Type::None),
                optional: 0,
            },
            &Type::None,
        )),
        // `reduce` is generic in the accumulator A: ((A, T) -> A) -> A.
        // A is solved from the callback at the call site (deka#467).
        ("Array", "reduce") => PrimitiveMember::BuiltinMethod(Type::Function {
            params: vec![Type::Function {
                params: vec![Type::Param { name: "A" }, elem?.clone()],
                ret: Box::new(Type::Param { name: "A" }),
                optional: 0,
            }],
            ret: Box::new(Type::Param { name: "A" }),
            optional: 0,
        }),
        _ => return None,
    };
    Some(member)
}

/// Total `Math` functions exposed as methods on `number`: every input —
/// including the non-finite ones (`Infinity`, `NaN`) — yields a genuine
/// number (deka#378 step 2, rfd#40 phase 2). `sin`/`cos`/`tan` are NOT
/// total: `Math.sin(Infinity)` and friends answer `NaN` (deka#594 review),
/// so they live in `NUMBER_MATH_PARTIAL`. `max`/`min` are handled
/// separately because they take one argument.
pub(super) const NUMBER_MATH_TOTAL: &[&str] = &[
    "abs", "ceil", "floor", "round", "trunc", "sign", "cbrt", "exp", "atan", "sinh", "cosh",
    "tanh",
];

/// Partial `Math` functions: some inputs make JavaScript produce `NaN`, so
/// the method returns `Option<number>` and the emitted wrapper rewrites
/// `NaN` to `None`. `sin`/`cos`/`tan` are partial because the non-finite
/// inputs (`Math.sin(Infinity)` → `NaN`) are ordinary reachable `number`s
/// in DekaScript (`1.0/0.0`); classifying them as total would type a NaN
/// result as `number` — the exact "type that claims something false" defect
/// rfd#13 exists to close. `pow` is handled separately because it takes one
/// argument.
pub(super) const NUMBER_MATH_PARTIAL: &[&str] = &[
    "sqrt", "log", "log2", "log10", "asin", "acos", "acosh", "atanh", "sin", "cos", "tan",
];

/// Classify a builtin `Math`-backed method on `number` for the emitter
/// (deka#378 step 2). Must stay in sync with the `("number", …)` arms of
/// `primitive_member`.
pub(super) fn number_math_kind(method: &str) -> Option<super::types::NumberMath> {
    if NUMBER_MATH_TOTAL.contains(&method) || matches!(method, "max" | "min") {
        Some(super::types::NumberMath::Total)
    } else if NUMBER_MATH_PARTIAL.contains(&method) || method == "pow" {
        Some(super::types::NumberMath::Partial)
    } else {
        None
    }
}

impl<'a> Checker<'a> {
    pub(super) fn check_expr(&mut self, expr: &ast::Expr<'a>) -> Type<'a> {
        match expr {
            ast::Expr::Number { .. } => Type::Named { name: "number" },
            ast::Expr::String { .. } => Type::Named { name: "string" },
            ast::Expr::Boolean { value: true, .. } | ast::Expr::Boolean { value: false, .. } => {
                Type::Named { name: "boolean" }
            }
            ast::Expr::None { .. } => Type::None,
            ast::Expr::Identifier { name, span } => match self.lookup_var(name) {
                Some(ty) => ty,
                None => {
                    self.error_span(*span, format!("unknown identifier `{name}`"));
                    Type::Error
                }
            },
            ast::Expr::Binary {
                op,
                left,
                right,
                span,
            } => self.check_binary(expr, *op, left, right, *span),
            ast::Expr::Unary { op, operand, span } => self.check_unary(expr, *op, operand, *span),
            ast::Expr::Call {
                callee,
                type_args,
                args,
                span,
                ..
            } => {
                // `deka.panic` must not go through method-call typeck: `deka`
                // is not a typed object (RFD 21 lang item).
                if is_panic_callee(callee) {
                    self.check_call(expr, callee, type_args, args, *span)
                } else if let Some(ret) =
                    self.try_check_method_call(expr, callee, type_args, args, *span)
                {
                    ret
                } else {
                    self.check_call(expr, callee, type_args, args, *span)
                }
            }
            ast::Expr::FieldAccess {
                object,
                field,
                span,
            } => self.check_field_access(object, field, *span),
            ast::Expr::StructLiteral { name, fields, span } => {
                self.check_struct_literal(name, fields, *span)
            }
            ast::Expr::Paren { expr, .. } => self.check_expr(expr),
            ast::Expr::Match {
                scrutinee,
                arms,
                span,
            } => self.check_match(scrutinee, arms, *span),
            ast::Expr::EnumConstructor {
                enum_name,
                case_name,
                payload,
                span,
            } => self.check_enum_constructor(enum_name, case_name, payload.as_deref(), *span),
            ast::Expr::Array { elements, .. } => {
                let mut elem_type = None;
                for element in elements.iter() {
                    let ty = self.check_expr(element);
                    if ty.is_error() {
                        return Type::Error;
                    }
                    if elem_type.is_none() {
                        elem_type = Some(ty);
                    }
                }
                Type::Array {
                    // `[]` is genuinely polymorphic (deka#468).
                    elem: Box::new(elem_type.unwrap_or(Type::Var)),
                }
            }
            ast::Expr::Object { fields, .. } => {
                let mut field_types = Vec::new();
                for field in fields.iter() {
                    let ty = self.check_expr(&field.value);
                    if ty.is_error() {
                        return Type::Error;
                    }
                    field_types.push((field.key, ty));
                }
                Type::Object {
                    fields: field_types,
                }
            }
            ast::Expr::IndexAccess { object, index, .. } => {
                let object_type = self.check_expr(object);
                self.check_expr(index);
                object_type.collection_element()
            }
            ast::Expr::Spread { expr, .. } => {
                self.check_expr(expr);
                Type::Infer
            }
            ast::Expr::Await { expr, span } => {
                if self.in_function && !self.in_async_function {
                    self.error_span(
                        *span,
                        "`await` is only allowed inside async functions or at the top level",
                    );
                }
                let operand_type = self.check_expr(expr);
                match operand_type {
                    Type::Generic {
                        base: "Promise",
                        args,
                    } if args.len() == 1 => args.into_iter().next().unwrap(),
                    Type::Infer | Type::Error => Type::Infer,
                    other => {
                        self.error_span(
                            *span,
                            format!("`await` expected Promise<T>, found type `{other}`"),
                        );
                        Type::Infer
                    }
                }
            }
            ast::Expr::JsxElement { element, span } => {
                // Uppercase JSX tags are component references and must be in
                // scope; lowercase tags are plain HTML element names.
                if let Some(first) = element.tag.chars().next() {
                    if first.is_uppercase() && self.lookup_var(element.tag).is_none() {
                        self.error_span(
                            *span,
                            format!(
                                "`{}` is used here but is not initialized until later",
                                element.tag
                            ),
                        );
                    }
                }
                self.check_jsx_attributes(element, *span);
                for child in element.children.iter() {
                    let child_type = self.check_expr(child);
                    self.reject_unrendered_option(&child_type, child.span());
                }
                // A JSX element is a `Component` (deka#461). It used to be
                // `Infer`, which is universally assignable, so every JSX value
                // silently stopped being checked -- `let n: number = <p/>` was
                // accepted.
                Type::Named { name: "Component" }
            }
            ast::Expr::JsxFragment { children, .. } => {
                for child in children.iter() {
                    let child_type = self.check_expr(child);
                    self.reject_unrendered_option(&child_type, child.span());
                }
                Type::Named { name: "Component" }
            }
            ast::Expr::JsxText { .. } => Type::Named { name: "string" },
            ast::Expr::Unsafe {
                result_type, span, ..
            } => {
                // Raw JavaScript block. The emitter wraps the body in
                // try/catch, so the block is a Result. The success type is
                // declared; the error side is whatever JavaScript threw, which
                // is always `JsError` (deka#460).
                match result_type {
                    Some(ty) => {
                        let ok = self.resolve_ast_type(ty);
                        Type::Generic {
                            base: "Result",
                            args: vec![ok, Type::Named { name: "JsError" }],
                        }
                    }
                    None => {
                        // Legacy bare `unsafe { }`. This is the load-bearing
                        // source of `Infer` in the language (deka#252): every
                        // value flowing out of it is universally assignable
                        // and silently stops being checked.
                        //
                        // No diagnostic yet: the published stdlib on the
                        // registry still contains bare `unsafe`, so this
                        // cannot become an error until those packages are
                        // republished. That is deka#460 phase 4.
                        let _ = span;
                        Type::Generic {
                            base: "Result",
                            args: vec![Type::Infer, Type::Infer],
                        }
                    }
                }
            }
            ast::Expr::Bridge { kind, action, args, .. } => {
                // Host bridge calls are validated by the runtime catalog. The
                // result shape is always Result<T, E>; async ops (deka#578)
                // resolve through a Promise, so `await bridge fs.read_file(p)`
                // typechecks as rfd#27 describes while sync ops such as
                // `bridge crypto.random_bytes(n)` stay plain Results.
                for arg in args.iter() {
                    self.check_expr(arg);
                }
                let result = Type::Generic {
                    base: "Result",
                    args: vec![Type::Infer, Type::Infer],
                };
                if crate::bridge::bridge_op_is_async(kind, action) {
                    Type::Generic {
                        base: "Promise",
                        args: vec![result],
                    }
                } else {
                    result
                }
            }
            ast::Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                let cond_type = self.check_expr(condition);
                if !Self::is_boolean(&cond_type)
                    && !cond_type.is_error()
                    && !matches!(cond_type, Type::Infer)
                {
                    self.error_span(
                        *span,
                        format!("ternary condition must be boolean, found type `{cond_type}`"),
                    );
                }
                let then_type = self.check_expr(then_branch);
                let else_type = self.check_expr(else_branch);
                if self.is_assignable(&then_type, &else_type) {
                    then_type
                } else if self.is_assignable(&else_type, &then_type) {
                    else_type
                } else {
                    self.error_span(
                        *span,
                        format!("ternary branches have incompatible types `{then_type}` and `{else_type}`"),
                    );
                    Type::Error
                }
            }
            ast::Expr::TemplateLiteral { .. } => Type::Named { name: "string" },
            ast::Expr::Function {
                params,
                return_type,
                body,
                is_async,
                span,
            } => self.check_function_expr(params, return_type.as_ref(), body, *is_async, *span),
            _ => {
                self.error_at_expr(expr, "unsupported expression in v2 typeck");
                Type::Error
            }
        }
    }

    fn check_function_expr(
        &mut self,
        params: &'a [ast::Param<'a>],
        return_type: Option<&ast::Type<'a>>,
        body: &'a [ast::Stmt<'a>],
        is_async: bool,
        span: ast::Span,
    ) -> Type<'a> {
        let mut param_types = Vec::new();
        for p in params {
            match &p.ty {
                Some(t) => param_types.push(self.resolve_ast_type(t)),
                None => {
                    self.error_span(
                        p.span,
                        format!("parameter `{}` is missing a type annotation", p.name),
                    );
                    param_types.push(Type::Error);
                }
            }
        }

        let explicit_ret = return_type.map(|t| self.resolve_ast_type(t));
        let (body_expected_ret, _final_ret) =
            self.function_return_context(is_async, explicit_ret.clone(), span);

        self.scopes.push(HashMap::new());
        self.mutables.push(HashSet::new());

        for (p, t) in params.iter().zip(param_types.iter()) {
            self.declare_var(p.name, t.clone());
        }

        let saved_in_function = self.in_function;
        let saved_in_async = self.in_async_function;
        let saved_return_type = self.return_type.clone();
        self.in_function = true;
        self.in_async_function = is_async;
        self.return_type = body_expected_ret.clone();

        for stmt in body {
            self.check_statement(stmt);
        }

        self.in_async_function = saved_in_async;

        let final_ret = if is_async {
            match explicit_ret {
                Some(ret) => ret,
                None => self
                    .return_type
                    .take()
                    .map(|inner| Type::Generic {
                        base: "Promise",
                        args: vec![inner],
                    })
                    .unwrap_or(Type::Generic {
                        base: "Promise",
                        args: vec![Type::None],
                    }),
            }
        } else {
            body_expected_ret.unwrap_or_else(|| self.return_type.take().unwrap_or(Type::None))
        };

        self.in_function = saved_in_function;
        self.return_type = saved_return_type;
        self.scopes.pop();
        self.mutables.pop();

        let optional = params
            .iter()
            .rev()
            .take_while(|p| p.default_value.is_some())
            .count();

        Type::Function {
            params: param_types,
            ret: Box::new(final_ret),
            optional,
        }
    }

    fn check_struct_literal(
        &mut self,
        name: &'a str,
        fields: &'a [ast::StructLiteralField<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        let info = match self.structs.get(name).cloned() {
            Some(info) => info,
            None => {
                self.error_span(span, format!("unknown struct `{name}`"));
                return Type::Error;
            }
        };

        let fields: Vec<(&'a str, &ast::Expr<'a>, ast::Span)> =
            fields.iter().map(|f| (f.name, &f.value, f.span)).collect();
        self.check_struct_literal_fields(name, &info, &fields, span);
        Type::Struct { name }
    }

    fn check_struct_literal_fields(
        &mut self,
        name: &'a str,
        info: &super::StructInfo<'a>,
        fields: &[(&'a str, &ast::Expr<'a>, ast::Span)],
        span: ast::Span,
    ) {
        let embed_names: HashSet<&str> = info.embeds.iter().map(|e| e.name).collect();
        let mut seen_fields = HashSet::new();
        for (field_name, value, field_span) in fields {
            if !seen_fields.insert(*field_name) {
                self.error_span(
                    *field_span,
                    format!("duplicate field `{}` in struct literal", field_name),
                );
            }

            let expected_type = if let Some(f) = info.fields.iter().find(|f| f.name == *field_name)
            {
                self.resolve_ast_type(&f.ty)
            } else if embed_names.contains(field_name) {
                Type::Struct { name: field_name }
            } else {
                // Promoted field: a literal may initialize a field declared on
                // an embedded struct directly (`Employee { name: ... }`
                // instead of `Employee { Person: Person { name: ... } }`).
                let mut embed_path = Vec::new();
                if self.find_promoted_field_path(name, field_name, &mut embed_path) {
                    let root = embed_path[0];
                    if fields.iter().any(|(n, _, _)| *n == root) {
                        self.error_span(
                            *field_span,
                            format!(
                                "struct literal for `{name}` supplies embedded struct `{root}` both directly and via promoted field `{field_name}`"
                            ),
                        );
                    }
                    self.resolve_field_type(name, field_name)
                        .unwrap_or(Type::Error)
                } else {
                    self.error_span(
                        *field_span,
                        format!("struct `{name}` has no field or embed `{}`", field_name),
                    );
                    Type::Error
                }
            };

            let value_type = self.check_expr(value);
            if !self.is_assignable(&expected_type, &value_type) {
                self.error_span(
                    *field_span,
                    super::with_union_narrowing_hint(
                        format!(
                            "field `{}` expected type `{expected_type}`, found type `{value_type}`",
                            field_name
                        ),
                        &expected_type,
                        &value_type,
                    ),
                );
            }
        }

        for field in info.fields {
            if field.default_value.is_none() && !field.optional && !seen_fields.contains(field.name)
            {
                self.error_span(
                    span,
                    format!(
                        "missing required field `{}` in struct literal for `{name}`",
                        field.name
                    ),
                );
            }
        }

        for embed in info.embeds {
            if seen_fields.contains(embed.name) {
                continue;
            }
            // Empty embedded structs (no fields and only empty embeds) are
            // auto-filled by the emitter, so they need not be supplied literally.
            if self.is_empty_embed_struct(embed.name) {
                continue;
            }
            // An embedded struct may also be supplied piecemeal through its
            // promoted fields (`Employee { name: ... }`).
            if self.embed_satisfied_by_promoted_fields(embed.name, &seen_fields) {
                continue;
            }
            self.error_span(
                span,
                format!(
                    "missing embedded struct `{}` in struct literal for `{name}`",
                    embed.name
                ),
            );
        }
    }

    /// Search the embedded structs of `struct_name` for a field named `field`,
    /// recording the chain of embed names that leads to the struct declaring
    /// it. Own fields are not searched; callers check those first. The
    /// traversal is depth-first, matching `resolve_field_type`.
    fn find_promoted_field_path(
        &self,
        struct_name: &'a str,
        field: &str,
        path: &mut Vec<&'a str>,
    ) -> bool {
        let info = match self.structs.get(struct_name) {
            Some(i) => i,
            None => return false,
        };
        for embed in info.embeds {
            path.push(embed.name);
            let declares = self
                .structs
                .get(embed.name)
                .map(|i| i.fields.iter().any(|f| f.name == field))
                .unwrap_or(false);
            if declares || self.find_promoted_field_path(embed.name, field, path) {
                return true;
            }
            path.pop();
        }
        false
    }

    /// An embedded struct counts as supplied when every required field it
    /// owns — directly or through its own embeds — appears in the literal as
    /// a promoted field.
    fn embed_satisfied_by_promoted_fields(&self, name: &'a str, supplied: &HashSet<&str>) -> bool {
        let info = match self.structs.get(name) {
            Some(i) => i,
            None => return false,
        };
        for field in info.fields {
            if field.default_value.is_none() && !field.optional && !supplied.contains(field.name) {
                return false;
            }
        }
        info.embeds.iter().all(|e| {
            supplied.contains(e.name)
                || self.is_empty_embed_struct(e.name)
                || self.embed_satisfied_by_promoted_fields(e.name, supplied)
        })
    }

    fn is_empty_embed_struct(&self, name: &'a str) -> bool {
        let info = match self.structs.get(name) {
            Some(i) => i,
            None => return false,
        };
        if !info.fields.is_empty() {
            return false;
        }
        info.embeds
            .iter()
            .all(|e| self.is_empty_embed_struct(e.name))
    }

    fn check_field_access(
        &mut self,
        object: &ast::Expr<'a>,
        field: &'a str,
        span: ast::Span,
    ) -> Type<'a> {
        let object_type = self.check_expr(object);
        if object_type.is_error() {
            return Type::Error;
        }

        match &object_type {
            Type::Infer | Type::Var => {
                // An externally-provided or unresolved value (`Infer`) and an
                // unconstrained one (`Var`) may both have any field. Cloning the
                // object type preserves *which* kind it was through the access:
                // a field of something unconstrained is itself unconstrained,
                // and must not silently become "unresolved" (deka#468).
                object_type.clone()
            }
            Type::Struct { name } => {
                let struct_name = *name;
                match self.resolve_field_type(struct_name, field) {
                    Some(ty) => ty,
                    None => {
                        self.error_span(
                            span,
                            format!("struct `{struct_name}` has no field `{field}`"),
                        );
                        Type::Error
                    }
                }
            }
            Type::Object { fields } => {
                if let Some((_, ty)) = fields.iter().find(|(name, _)| *name == field) {
                    ty.clone()
                } else {
                    self.error_span(span, format!("object has no field `{field}`"));
                    Type::Error
                }
            }
            Type::Array { elem } => self.resolve_array_field(field, elem, span),
            Type::Interface { name } => {
                self.resolve_interface_field(name, field)
                    .unwrap_or_else(|| {
                        self.error_span(span, format!("interface `{name}` has no field `{field}`"));
                        Type::Error
                    })
            }
            Type::Named { name } => self.resolve_primitive_field(name, field, span),
            Type::Union { .. } => {
                // A union value used without narrowing is a compile error
                // (rfd#42, deka#530): the member that provides the field is
                // not known until the value is narrowed with match.
                self.error_span(
                    span,
                    format!(
                        "cannot access field `{field}` on union type `{object_type}`; \
                         narrow it with a match type-pattern first"
                    ),
                );
                Type::Error
            }
            _ => {
                // Enum namespace access: `Color.Red` where `Color` is an enum name.
                if let ast::Expr::Identifier {
                    name: enum_name, ..
                } = object
                {
                    if let Some(info) = self.enums.get(enum_name).cloned() {
                        if info.cases.iter().any(|c| c.name == field) {
                            return Type::Named { name: enum_name };
                        }
                        self.error_span(
                            span,
                            format!("case `{field}` not found in enum `{enum_name}`"),
                        );
                        return Type::Error;
                    }
                }
                self.error_span(
                    span,
                    format!("cannot access field `{field}` on type `{object_type}`"),
                );
                Type::Error
            }
        }
    }

    fn resolve_primitive_field(
        &mut self,
        type_name: &'a str,
        field: &'a str,
        span: ast::Span,
    ) -> Type<'a> {
        if let Some(PrimitiveMember::Property(ty) | PrimitiveMember::BuiltinMethod(ty)) =
            primitive_member(type_name, field, None)
        {
            return ty;
        }
        // User extensions are call-shaped; reading one as a property is a
        // mistake worth naming (deka#527). Builtin members win above, so
        // `s.length` stays a property even when an extension shadows
        // call-shaped `s.length()`.
        if super::is_primitive_receiver_name(type_name)
            && self.receiver_methods.contains_key(&(type_name, field))
        {
            self.error_span(
                span,
                format!(
                    "extension method `{field}` on `{type_name}` must be called, not read as a property"
                ),
            );
            return Type::Error;
        }
        match type_name {
            // `JsError` is whatever JavaScript threw, surfaced through the
            // emitter's try/catch. JS can throw anything -- `throw "boom"` has
            // no `.message` -- so the emitter normalises a non-Error throw into
            // an Error. These two fields are always present because of that
            // guarantee; without it, declaring them would be a lie the type
            // system could not catch (deka#460, deka#469).
            "JsError" => {
                self.error_span(
                    span,
                    format!("`JsError` has no field `{field}` (available: `message`, `name`)"),
                );
                Type::Error
            }
            "string" | "number" | "boolean" => {
                self.error_span(span, format!("`{type_name}` has no field `{field}`"));
                Type::Error
            }
            _ => {
                // Enum values expose a small reflective surface.
                if self.enums.contains_key(type_name) {
                    return match field {
                        "name" => Type::Named { name: "string" },
                        "index" => Type::Named { name: "number" },
                        _ => {
                            self.error_span(
                                span,
                                format!("enum `{type_name}` has no field `{field}`"),
                            );
                            Type::Error
                        }
                    };
                }
                self.error_span(
                    span,
                    format!("cannot access field `{field}` on type `{type_name}`"),
                );
                Type::Error
            }
        }
    }

    /// Reject interpolating an `Option` or `Result` straight into JSX.
    ///
    /// Rendering one puts the enum object itself into the DOM -- `[object
    /// Object]` -- with no diagnostic anywhere. That is the failure mode
    /// deka#416 would otherwise have introduced silently at every existing
    /// `{props.optionalThing}`, so the read has to fail loudly instead.
    fn reject_unrendered_option(&mut self, ty: &Type<'a>, span: ast::Span) {
        let name = match ty {
            Type::Option { .. } => "Option",
            Type::Generic { base: "Result", .. } => "Result",
            _ => return,
        };
        self.error_span(
            span,
            format!(
                "cannot render a `{name}` directly; match it first \
                 (`match (x) {{ Some(v) => …, None => … }}`)"
            ),
        );
    }

    /// The props interface of an uppercase JSX tag, if it has one.
    ///
    /// `<Card … />` resolves `Card` to its function type and takes the first
    /// parameter. A component whose props are a struct, an inline object type
    /// or unannotated yields `None` and is not prop-checked -- this is the
    /// interface case, which is what components are written with.
    fn jsx_props_interface(&mut self, tag: &'a str) -> Option<&'a str> {
        if !tag.chars().next().is_some_and(|c| c.is_uppercase()) {
            return None;
        }
        let Some(Type::Function { params, .. }) = self.lookup_var(tag) else {
            return None;
        };
        let Some(Type::Interface { name }) = params.first().cloned() else {
            return None;
        };
        if self.interfaces.contains_key(name) {
            Some(name)
        } else {
            None
        }
    }

    /// Check a JSX element's attributes against its component's props interface.
    ///
    /// deka#443: attributes were never checked at all. A missing required prop,
    /// a wrong type and an unknown prop all compiled, while a read of the same
    /// interface *inside* the component was checked correctly -- so every
    /// guarantee stopped at the `<`.
    ///
    /// JSX spread (`{...expr}`) is rejected by the parser, so every attribute
    /// here is a named one and the supplied set is known exactly. That is what
    /// makes the missing-prop check sound.
    fn check_jsx_attributes(&mut self, element: &ast::JsxElement<'a>, span: ast::Span) {
        let Some(interface_name) = self.jsx_props_interface(element.tag) else {
            // Not a component with an interface props type: still typecheck the
            // attribute expressions themselves.
            for attr in element.attributes.iter() {
                if let Some(value) = &attr.value {
                    self.check_expr(value);
                }
            }
            return;
        };

        let fields: Vec<(&'a str, &'a ast::Type<'a>, bool)> = {
            let info = match self.interfaces.get(interface_name) {
                Some(info) => info,
                None => return,
            };
            info.members
                .iter()
                .filter_map(|member| match member {
                    ast::InterfaceMember::Field {
                        name, ty, optional, ..
                    } => Some((*name, ty, *optional)),
                    ast::InterfaceMember::Method { .. } => None,
                })
                .collect()
        };

        let mut supplied: Vec<&'a str> = Vec::new();

        for attr in element.attributes.iter() {
            supplied.push(attr.name);

            let Some((_, field_ty, _)) = fields.iter().find(|(name, _, _)| *name == attr.name)
            else {
                if let Some(value) = &attr.value {
                    self.check_expr(value);
                }
                self.error_span(
                    attr.span,
                    format!("interface `{interface_name}` has no prop `{}`", attr.name),
                );
                continue;
            };

            let expected = self.resolve_ast_type(field_ty);

            // `<Card flag />` is boolean shorthand.
            let Some(value) = &attr.value else {
                if !Self::is_boolean(&expected) && !matches!(expected, Type::Infer | Type::Error) {
                    self.error_span(
                        attr.span,
                        format!(
                            "prop `{}` expects type `{expected}`; a bare attribute is `true`",
                            attr.name
                        ),
                    );
                }
                continue;
            };

            let actual = self.check_expr(value);
            if !self.is_assignable(&expected, &actual)
                && !matches!(actual, Type::Infer | Type::Error)
                && !matches!(expected, Type::Infer | Type::Error)
            {
                self.error_span(
                    attr.span,
                    super::with_union_narrowing_hint(
                        format!(
                            "prop `{}` expects type `{expected}`, found type `{actual}`",
                            attr.name
                        ),
                        &expected,
                        &actual,
                    ),
                );
            }
        }

        // The plan the emitter applies: every `?:` prop becomes an `Option`
        // here, because this is a construction site the compiler owns
        // (deka#416).
        let mut plan = crate::typeck::JsxOptionalProps::default();
        for (name, _, optional) in fields.iter() {
            if !*optional {
                continue;
            }
            if supplied.contains(name) {
                plan.wrap_some.push(name);
            } else if *name != "children" {
                plan.fill_none.push(name);
            }
        }
        if !plan.fill_none.is_empty() || !plan.wrap_some.is_empty() {
            self.jsx_optional_props
                .insert(element as *const ast::JsxElement<'a>, plan);
        }

        for (name, _, optional) in fields.iter() {
            // `children` is supplied by nesting, not by an attribute:
            // `<Layout><Page /></Layout>` fills `children: Component`. Every
            // layout in the framework is written that way, so treating it as
            // missing would reject the generated entry for any app.
            if *name == "children" && !element.children.is_empty() {
                continue;
            }
            if !*optional && !supplied.contains(name) {
                self.error_span(
                    span,
                    format!("missing required prop `{name}` on `{}`", element.tag),
                );
            }
        }
    }

    fn resolve_interface_field(
        &mut self,
        interface_name: &'a str,
        field: &'a str,
    ) -> Option<Type<'a>> {
        let info = self.interfaces.get(interface_name)?;
        for member in info.members.iter() {
            match member {
                ast::InterfaceMember::Field {
                    name, ty, optional, ..
                } if *name == field => {
                    let resolved = self.resolve_ast_type(ty);
                    //  is the same thing as  -- one
                    // meaning for optional, whichever way it is spelled
                    // (deka#416).
                    if *optional {
                        return Some(Type::Option {
                            inner: Box::new(resolved),
                        });
                    }
                    return Some(resolved);
                }
                ast::InterfaceMember::Method {
                    name,
                    params,
                    return_type,
                    ..
                } if *name == field => {
                    let param_types: Vec<Type<'a>> = params
                        .iter()
                        .map(|p| {
                            p.ty.as_ref()
                                .map(|t| self.resolve_ast_type(t))
                                .unwrap_or(Type::Infer)
                        })
                        .collect();
                    let ret = return_type
                        .as_ref()
                        .map(|t| self.resolve_ast_type(t))
                        .unwrap_or(Type::Named { name: "void" });
                    return Some(Type::Function {
                        params: param_types,
                        ret: Box::new(ret),
                        optional: 0,
                    });
                }
                _ => {}
            }
        }
        None
    }

    fn resolve_array_field(
        &mut self,
        field: &'a str,
        elem: &Type<'a>,
        span: ast::Span,
    ) -> Type<'a> {
        match primitive_member("Array", field, Some(elem)) {
            Some(PrimitiveMember::Property(ty) | PrimitiveMember::BuiltinMethod(ty)) => ty,
            None => {
                self.error_span(span, format!("array has no field `{field}`"));
                Type::Error
            }
        }
    }

    /// Resolve a field's type, recursively searching embedded structs.
    fn resolve_field_type(&mut self, struct_name: &'a str, field: &'a str) -> Option<Type<'a>> {
        let info = self.structs.get(struct_name)?;
        if let Some(f) = info.fields.iter().find(|f| f.name == field) {
            return Some(self.resolve_ast_type(&f.ty));
        }
        for embed in info.embeds {
            if let Some(ty) = self.resolve_field_type(embed.name, field) {
                return Some(ty);
            }
        }
        None
    }

    fn check_enum_constructor(
        &mut self,
        enum_name: &'a str,
        case_name: &'a str,
        payload: Option<&ast::Expr<'a>>,
        span: ast::Span,
    ) -> Type<'a> {
        let payload_type = payload.map(|expr| self.check_expr(expr));

        if enum_name == "Option" {
            return self.check_option_constructor(case_name, payload, payload_type, span);
        }

        if enum_name == "Result" {
            return self.check_result_constructor(case_name, payload, payload_type, span);
        }

        // User-defined enum.
        let info = match self.enums.get(enum_name) {
            Some(i) => i.clone(),
            None => {
                self.error_span(span, format!("unknown enum `{enum_name}`"));
                return Type::Error;
            }
        };

        let case = match info.cases.iter().find(|c| c.name == case_name) {
            Some(c) => c,
            None => {
                self.error_span(
                    span,
                    format!("case `{case_name}` not found in enum `{enum_name}`"),
                );
                return Type::Error;
            }
        };

        let params: Vec<&'a str> = info.type_params.iter().map(|p| p.name).collect();
        let mut inferred: HashMap<&'a str, Type<'a>> = HashMap::new();

        match (&case.payload, payload_type) {
            (Some(expected), Some(actual)) => {
                self.push_type_params(info.type_params);
                let expected_ty = self.resolve_ast_type(expected);
                self.pop_type_params();
                // `Box.Full(5)` must infer `Box<number>` rather than reporting a
                // mismatch between the declared `T` and the argument (deka#372).
                infer_type_args(&expected_ty, &actual, &params, &mut inferred);
                let expected_ty = substitute_type(&expected_ty, &inferred);
                if !self.is_assignable(&expected_ty, &actual) {
                    self.error_span(
                        span,
                        super::with_union_narrowing_hint(
                            format!(
                                "enum case `{case_name}` expected payload type `{expected_ty}`, found type `{actual}`"
                            ),
                            &expected_ty,
                            &actual,
                        ),
                    );
                }
            }
            (Some(_), None) => {
                self.error_span(span, format!("`{case_name}` requires a payload"));
            }
            (None, Some(_)) => {
                self.error_span(span, format!("`{case_name}` cannot have a payload"));
            }
            (None, None) => {}
        }

        if params.is_empty() {
            return Type::Named { name: enum_name };
        }
        // Parameters a payload-free case cannot pin stay Infer, which is
        // compatible with any concrete type until one is available.
        let args: Vec<Type<'a>> = params
            .iter()
            .map(|p| inferred.get(p).cloned().unwrap_or(Type::Infer))
            .collect();
        Type::Generic {
            base: enum_name,
            args,
        }
    }

    fn check_option_constructor(
        &mut self,
        case_name: &'a str,
        payload: Option<&ast::Expr<'a>>,
        payload_type: Option<Type<'a>>,
        span: ast::Span,
    ) -> Type<'a> {
        match case_name {
            "Some" => match payload_type {
                Some(t) => Type::Option { inner: Box::new(t) },
                None => {
                    self.error_span(span, "`Some` requires a payload");
                    Type::Error
                }
            },
            "None" => {
                if payload.is_some() {
                    self.error_span(span, "`None` cannot have a payload");
                }
                // `None` is polymorphic: it names no payload type at all, so the
                // inner type is unconstrained rather than unresolved (deka#468).
                Type::Option {
                    inner: Box::new(Type::Var),
                }
            }
            _ => {
                self.error_span(span, format!("unknown Option case `{case_name}`"));
                Type::Error
            }
        }
    }

    fn check_result_constructor(
        &mut self,
        case_name: &'a str,
        _payload: Option<&ast::Expr<'a>>,
        payload_type: Option<Type<'a>>,
        span: ast::Span,
    ) -> Type<'a> {
        match case_name {
            "Ok" => match payload_type {
                // `Ok(x)` fixes T and says nothing about E (deka#468).
                Some(t) => Type::Generic {
                    base: "Result",
                    args: vec![t, Type::Var],
                },
                None => {
                    self.error_span(span, "`Ok` requires a payload");
                    Type::Error
                }
            },
            "Err" => match payload_type {
                // `Err(e)` fixes E and says nothing about T (deka#468).
                Some(e) => Type::Generic {
                    base: "Result",
                    args: vec![Type::Var, e],
                },
                None => {
                    self.error_span(span, "`Err` requires a payload");
                    Type::Error
                }
            },
            _ => {
                self.error_span(span, format!("unknown Result case `{case_name}`"));
                Type::Error
            }
        }
    }

    fn check_match(
        &mut self,
        scrutinee: &ast::Expr<'a>,
        arms: &'a [ast::MatchArm<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        if arms.is_empty() {
            self.error_span(span, "match expression must have at least one arm");
            return Type::Error;
        }

        let scrutinee_type = self.check_expr(scrutinee);
        let mut result_type: Option<Type<'a>> = None;
        let mut coverage = Coverage::nothing();
        let mut has_catch_all = false;

        for arm in arms {
            self.scopes.push(HashMap::new());
            self.mutables.push(HashSet::new());
            self.check_pattern(&arm.pattern, &scrutinee_type);
            // Union type-patterns rebind the operand within the arm
            // (rfd#42): `match (v) { string(s) => ... }` shadows `v` with
            // `string` inside the arm. Plain shadowing — DekaScript has no
            // flow-sensitive typing — and the scope pop above restores it.
            // `match (v)` wraps the operand in `Expr::Paren`, so unwrap it.
            let scrutinee_ident = match scrutinee {
                ast::Expr::Identifier { name, .. } => Some(name),
                ast::Expr::Paren { expr, .. } => match &**expr {
                    ast::Expr::Identifier { name, .. } => Some(name),
                    _ => None,
                },
                _ => None,
            };
            if let (
                Some(name),
                ast::Pattern::Constructor {
                    name: pattern_name, ..
                },
            ) = (scrutinee_ident, &arm.pattern)
            {
                if self
                    .union_type_patterns
                    .contains_key(&(&arm.pattern as *const ast::Pattern<'a>))
                {
                    if let Type::Union { members } = &scrutinee_type {
                        if let Some(member) = members
                            .iter()
                            .find(|m| Self::union_member_name(m) == Some(pattern_name))
                        {
                            self.declare_var(name, member.clone());
                        }
                    }
                }
            }
            if !has_catch_all && Self::pattern_is_catch_all(&arm.pattern, &self.enum_case_patterns)
            {
                has_catch_all = true;
            }
            coverage = coverage.merge(Coverage::of_pattern(&arm.pattern, &self.enum_case_patterns));
            let arm_type = self.check_expr(&arm.body);
            self.scopes.pop();
            self.mutables.pop();

            match &result_type {
                // A `never` arm — one that calls `panic` or otherwise cannot
                // return — carries no information about the match's type, so a
                // later arm replaces it. Assignability only runs the other way:
                // `never` is assignable to anything, nothing is assignable to
                // `never`, so seeding from a `never` arm rejected every arm
                // after it and made the result depend on arm order (deka#407).
                Some(Type::Never) => result_type = Some(arm_type),
                Some(expected) => {
                    if !self.is_assignable(expected, &arm_type) {
                        self.error_at_expr(
                            &arm.body,
                            super::with_union_narrowing_hint(
                                format!("match arm has type `{arm_type}`, expected type `{expected}`"),
                                expected,
                                &arm_type,
                            ),
                        );
                    }
                }
                None => result_type = Some(arm_type),
            }
        }

        if !has_catch_all && !scrutinee_type.is_error() {
            let scrutinee_type = scrutinee_type.clone();
            self.check_match_exhaustiveness(span, &scrutinee_type, &coverage);
        }

        result_type.unwrap_or(Type::None)
    }

    fn pattern_is_catch_all(
        pattern: &ast::Pattern<'a>,
        cases: &HashMap<*const ast::Pattern<'a>, &'a str>,
    ) -> bool {
        match pattern {
            ast::Pattern::Wildcard { .. } => true,
            ast::Pattern::Identifier { .. } => {
                !cases.contains_key(&(pattern as *const ast::Pattern<'a>))
            }
            ast::Pattern::Or { alternatives, .. } => alternatives
                .iter()
                .any(|alternative| Self::pattern_is_catch_all(alternative, cases)),
            _ => false,
        }
    }

    pub(super) fn check_match_exhaustiveness(
        &mut self,
        span: ast::Span,
        scrutinee_type: &Type<'a>,
        coverage: &Coverage<'a>,
    ) {
        let mut missing = Vec::new();
        self.collect_missing(scrutinee_type, coverage, "", &mut missing);
        if !missing.is_empty() {
            self.error_span(
                span,
                format!("non-exhaustive match: missing {}", missing.join(", ")),
            );
        }
    }

    /// The constructors of `ty`, with each payload type, when `ty` is an enum.
    /// `Option` and `Result` are prelude enums and are not in `self.enums`, so
    /// they are spelled out here rather than skipped (deka#396).
    fn enum_shape(&mut self, ty: &Type<'a>) -> Option<(String, Vec<(&'a str, Option<Type<'a>>)>)> {
        match ty {
            Type::Option { inner } => Some((
                "Option".to_string(),
                vec![("Some", Some((**inner).clone())), ("None", None)],
            )),
            Type::Generic {
                base: "Result",
                args,
            } if args.len() == 2 => Some((
                "Result".to_string(),
                vec![
                    ("Ok", Some(args[0].clone())),
                    ("Err", Some(args[1].clone())),
                ],
            )),
            Type::Named { name } => {
                let info = self.enums.get(name)?.clone();
                self.push_type_params(info.type_params);
                let cases = info
                    .cases
                    .iter()
                    .map(|case| {
                        let payload = case.payload.as_ref().map(|p| self.resolve_ast_type(p));
                        (case.name, payload)
                    })
                    .collect();
                self.pop_type_params();
                Some(((*name).to_string(), cases))
            }
            // A generic enum at a use site: resolve each payload with the
            // declared params in scope, then substitute the arguments in
            // (deka#372).
            Type::Generic { base, args } if self.enums.contains_key(base) => {
                let info = self.enums.get(base)?.clone();
                let subst: HashMap<&'a str, Type<'a>> = info
                    .type_params
                    .iter()
                    .map(|p| p.name)
                    .zip(args.iter().cloned())
                    .collect();
                self.push_type_params(info.type_params);
                let cases: Vec<(&'a str, Option<Type<'a>>)> = info
                    .cases
                    .iter()
                    .map(|case| {
                        let payload = case
                            .payload
                            .as_ref()
                            .map(|p| substitute_type(&self.resolve_ast_type(p), &subst));
                        (case.name, payload)
                    })
                    .collect();
                self.pop_type_params();
                Some(((*base).to_string(), cases))
            }
            _ => None,
        }
    }

    /// Walk type and coverage together. A constructor pattern covers its case
    /// only as far as its payload pattern covers the payload type, so
    /// `Ok(Some(v))` leaves `Ok(None)` uncovered.
    fn collect_missing(
        &mut self,
        ty: &Type<'a>,
        coverage: &Coverage<'a>,
        path: &str,
        out: &mut Vec<String>,
    ) {
        let Coverage::Cases(covered) = coverage else {
            return;
        };
        // A union is exhaustive only when every member is named by a
        // type-pattern (or a catch-all made coverage `All` above) — v1
        // requires one type-pattern per member rather than recursing into
        // enum case coverage (rfd#42, deka#530).
        if let Type::Union { members } = ty {
            for member in members {
                let Some(label) = Self::union_member_name(member) else {
                    continue;
                };
                if !covered.contains_key(label) {
                    out.push(if path.is_empty() {
                        label.to_string()
                    } else {
                        format!("{path}({label})")
                    });
                }
            }
            return;
        }
        let Some((label, cases)) = self.enum_shape(ty) else {
            // Not an enum: nothing to enumerate. A refutable pattern here (a
            // literal) is left alone rather than guessed at.
            return;
        };
        for (case_name, payload_ty) in cases {
            let qualified = if path.is_empty() {
                format!("{label}::{case_name}")
            } else {
                format!("{path}({label}::{case_name})")
            };
            match covered.get(case_name) {
                None => out.push(qualified),
                Some(sub) => match payload_ty {
                    Some(payload) => self.collect_missing(&payload, sub, &qualified, out),
                    None => {
                        if !matches!(sub, Coverage::All) {
                            out.push(qualified);
                        }
                    }
                },
            }
        }
    }

    pub(super) fn check_pattern(&mut self, pattern: &ast::Pattern<'a>, scrutinee_type: &Type<'a>) {
        match pattern {
            ast::Pattern::Wildcard { .. } => {}
            ast::Pattern::Identifier { name, span } => {
                // A bare name is a *case* when the scrutinee is an enum that
                // has one by that name and it carries no payload. It used to
                // always bind, which meant `match (c) { Red => …, Blue => … }`
                // compiled `Red` to a test of `true` and returned the first arm
                // for every input, with exhaustiveness satisfied (deka#450).
                if let Some((_, cases)) = self.enum_shape(scrutinee_type) {
                    if let Some((case_name, payload)) = cases.iter().find(|(case, _)| case == name)
                    {
                        if payload.is_some() {
                            self.error_span(
                                *span,
                                format!(
                                    "`{case_name}` carries a payload; write `{case_name}(value)`"
                                ),
                            );
                        }
                        self.enum_case_patterns
                            .insert(pattern as *const ast::Pattern<'a>, case_name);
                        return;
                    }
                }
                // A bare name that matches a union member is a type-pattern
                // attempt without its binding (`match (v) { string => ... }`).
                // Spec examples always bind; fail closed rather than silently
                // treating the member name as a catch-all binding (rfd#42,
                // deka#530).
                if let Type::Union { members } = scrutinee_type {
                    if members
                        .iter()
                        .any(|m| Self::union_member_name(m) == Some(name))
                    {
                        self.error_span(
                            *span,
                            format!(
                                "type pattern `{name}` requires a binding, e.g. `{name}(value)`"
                            ),
                        );
                        return;
                    }
                }
                self.declare_var(name, scrutinee_type.clone());
            }
            ast::Pattern::Literal { expr, span } => {
                let literal_type = self.check_expr(expr);
                if !self.is_assignable(scrutinee_type, &literal_type) {
                    self.error_span(
                        *span,
                        format!(
                            "literal pattern has type `{literal_type}`, expected type `{scrutinee_type}`"
                        ),
                    );
                }
            }
            ast::Pattern::Constructor {
                name,
                payload,
                span,
            } => {
                // Union member type-patterns (`string(s)` on a `string | number`
                // scrutinee) take priority over the user-enum constructor lookup
                // so a primitive name is not reported as an unknown constructor
                // (rfd#42, deka#530).
                if !self.check_union_type_pattern(
                    pattern,
                    name,
                    payload.as_deref(),
                    *span,
                    scrutinee_type,
                ) {
                    self.check_constructor_pattern(name, payload.as_deref(), *span, scrutinee_type);
                }
            }
            ast::Pattern::Or { alternatives, span } => {
                // Resolve each alternative first so a bare case name is known
                // to be a case and not a binding (deka#450), then reject any
                // that genuinely binds.
                for alternative in alternatives.iter() {
                    if let ast::Pattern::Identifier { .. } = alternative {
                        self.check_pattern(alternative, scrutinee_type);
                    }
                }
                let cases = self.enum_case_patterns.clone();
                for alternative in alternatives.iter() {
                    // A binding would have to come from whichever alternative
                    // matched, and every alternative would have to bind the
                    // same names for the arm body to be well-typed. Neither is
                    // built yet, so say so rather than bind from one branch
                    // (deka#446).
                    if let Some(name) = Self::pattern_binding_name(alternative, &cases) {
                        self.error_span(
                            *span,
                            format!(
                                "an alternative in `A | B` cannot bind (`{name}` here); \
                                 every alternative would have to bind the same names"
                            ),
                        );
                        continue;
                    }
                    self.check_pattern(alternative, scrutinee_type);
                }
            }
            ast::Pattern::Struct { span, .. } | ast::Pattern::Tuple { span, .. } => {
                self.error_span(
                    *span,
                    "struct/tuple patterns are not supported in v2 typeck",
                );
            }
        }
    }

    /// The first name an alternative would bind, if any.
    fn pattern_binding_name(
        pattern: &ast::Pattern<'a>,
        cases: &HashMap<*const ast::Pattern<'a>, &'a str>,
    ) -> Option<&'a str> {
        match pattern {
            ast::Pattern::Identifier { name, .. } => {
                if cases.contains_key(&(pattern as *const ast::Pattern<'a>)) {
                    None
                } else {
                    Some(name)
                }
            }
            ast::Pattern::Constructor { payload, .. } => {
                payload.and_then(|inner| Self::pattern_binding_name(inner, cases))
            }
            ast::Pattern::Or { alternatives, .. } => alternatives
                .iter()
                .find_map(|alternative| Self::pattern_binding_name(alternative, cases)),
            _ => None,
        }
    }

    /// Union member type-patterns (rfd#42, deka#530): `match (v) {
    /// string(s) => ... }` where `v: string | number`. The pattern tests
    /// the member and binds the payload to the member type in one construct,
    /// syntactically identical to the enum-case patterns `match` already
    /// handles.
    ///
    /// Returns true when this pattern was handled here (matched member, or a
    /// failed attempt against a union / Var / Infer scrutinee that must not
    /// fall through to the unknown-constructor error).
    fn check_union_type_pattern(
        &mut self,
        pattern: &ast::Pattern<'a>,
        name: &'a str,
        payload: Option<&ast::Pattern<'a>>,
        span: ast::Span,
        scrutinee_type: &Type<'a>,
    ) -> bool {
        let Type::Union { members } = scrutinee_type else {
            // A primitive type name against a non-union scrutinee is a
            // type-pattern attempt; guide instead of reporting an unknown
            // constructor. `Var` must not silently unify with a union
            // member (deka#468) — demand an annotation. `Infer` propagates
            // silently as elsewhere in error recovery.
            if !Self::is_union_primitive_name(name) {
                return false;
            }
            match scrutinee_type {
                Type::Var => self.error_span(
                    span,
                    format!(
                        "cannot match type pattern `{name}` on an unconstrained type; \
                         annotate the scrutinee with a union type, e.g. `v: string | number`"
                    ),
                ),
                Type::Infer => {}
                _ => return false,
            }
            return true;
        };

        let Some(member) = members
            .iter()
            .find(|m| Self::union_member_name(m) == Some(name))
        else {
            self.error_span(
                span,
                format!("`{name}` is not a member of union `{scrutinee_type}`"),
            );
            return true;
        };

        // Interfaces are allowed as union members but have no runtime
        // predicate to emit, so a type-pattern on one fails closed.
        if matches!(member, Type::Interface { .. }) {
            self.error_span(
                span,
                format!(
                    "cannot match type pattern `{name}`: interface `{name}` has no \
                     runtime predicate; match on a struct or primitive member instead"
                ),
            );
            return true;
        }

        if let Some(test) = self.union_member_test(member) {
            self.union_type_patterns
                .insert(pattern as *const ast::Pattern<'a>, test);
        }

        match payload {
            Some(p) => self.check_pattern(p, member),
            // Spec examples always bind (`string(s)`); a bare test-only
            // pattern is rejected to fail closed.
            None => self.error_span(
                span,
                format!("type pattern `{name}` requires a binding, e.g. `{name}(value)`"),
            ),
        }
        true
    }

    /// The name a union member is matched by in a type-pattern: the type
    /// name for primitives, structs, enums and interfaces.
    fn union_member_name(ty: &Type<'a>) -> Option<&'a str> {
        match ty {
            Type::Named { name } | Type::Struct { name } | Type::Interface { name } => Some(name),
            _ => None,
        }
    }

    fn is_union_primitive_name(name: &str) -> bool {
        matches!(name, "string" | "number" | "boolean" | "bytes" | "void")
    }

    /// The runtime predicate for a union member, if one exists. Interfaces
    /// have none; membership validation already rejected everything else.
    fn union_member_test(&self, member: &Type<'a>) -> Option<super::types::UnionMemberTest<'a>> {
        match member {
            Type::Named { name: "bytes" } => Some(super::types::UnionMemberTest::Bytes),
            Type::Named { name } if Self::is_union_primitive_name(name) => {
                Some(super::types::UnionMemberTest::Primitive(name))
            }
            Type::Named { name } if self.enums.contains_key(name) => {
                Some(super::types::UnionMemberTest::Enum(name))
            }
            Type::Struct { name } => Some(super::types::UnionMemberTest::Struct(name)),
            _ => None,
        }
    }

    fn check_constructor_pattern(
        &mut self,
        name: &'a str,
        payload: Option<&ast::Pattern<'a>>,
        span: ast::Span,
        scrutinee_type: &Type<'a>,
    ) {
        // Built-in Option cases.
        if name == "Some" || name == "None" {
            match scrutinee_type {
                Type::Option { inner } => {
                    if name == "None" {
                        if payload.is_some() {
                            self.error_span(span, "`None` pattern cannot have a payload");
                        }
                    } else if let Some(p) = payload {
                        self.check_pattern(p, inner);
                    } else {
                        self.error_span(span, "`Some` pattern requires a payload");
                    }
                    return;
                }
                _ if scrutinee_type.is_error() => return,
                _ => {
                    self.error_span(
                        span,
                        format!("`{name}` is not a case of type `{scrutinee_type}`"),
                    );
                    return;
                }
            }
        }

        // Built-in Result cases.
        if name == "Ok" || name == "Err" {
            match scrutinee_type {
                Type::Generic {
                    base: "Result",
                    args,
                } if args.len() == 2 => {
                    let expected_payload = if name == "Ok" { &args[0] } else { &args[1] };
                    if let Some(p) = payload {
                        self.check_pattern(p, expected_payload);
                    } else {
                        self.error_span(span, format!("`{name}` pattern requires a payload"));
                    }
                    return;
                }
                _ if scrutinee_type.is_error() => return,
                _ => {
                    self.error_span(
                        span,
                        format!("`{name}` is not a case of type `{scrutinee_type}`"),
                    );
                    return;
                }
            }
        }

        // User-defined enum cases.
        let enum_name = match self.case_to_enum.get(name).copied() {
            Some(n) => n,
            None => {
                self.error_span(span, format!("unknown constructor `{name}`"));
                return;
            }
        };

        let info = match self.enums.get(enum_name) {
            Some(i) => i.clone(),
            None => return,
        };

        // `enum Box<T>` used as `Box<number>` arrives as Type::Generic, not
        // Type::Named. Accept both and remember the type arguments so the case
        // payload can be substituted below (deka#372).
        let type_args: Vec<Type<'a>> = match scrutinee_type {
            Type::Named { name } if *name == enum_name => Vec::new(),
            Type::Generic { base, args } if *base == enum_name => args.clone(),
            _ if scrutinee_type.is_error() => Vec::new(),
            _ => {
                self.error_span(
                    span,
                    format!("`{name}` is not a case of type `{scrutinee_type}`"),
                );
                return;
            }
        };

        let case = match info.cases.iter().find(|c| c.name == name) {
            Some(c) => c,
            None => {
                self.error_span(
                    span,
                    format!("case `{name}` not found in enum `{enum_name}`"),
                );
                return;
            }
        };

        if let Some(payload_type) = &case.payload {
            let params: Vec<&'a str> = info.type_params.iter().map(|p| p.name).collect();
            let resolved_payload = {
                self.push_type_params(info.type_params);
                let base = self.resolve_ast_type(payload_type);
                self.pop_type_params();
                if params.is_empty() || type_args.is_empty() {
                    base
                } else {
                    let subst: HashMap<&'a str, Type<'a>> = params
                        .iter()
                        .copied()
                        .zip(type_args.iter().cloned())
                        .collect();
                    substitute_type(&base, &subst)
                }
            };
            if let Some(p) = payload {
                self.check_pattern(p, &resolved_payload);
            } else {
                self.error_span(span, format!("`{name}` pattern requires a payload"));
            }
        } else if payload.is_some() {
            self.error_span(span, format!("`{name}` pattern cannot have a payload"));
        }
    }

    fn check_binary(
        &mut self,
        expr: &ast::Expr<'a>,
        op: ast::BinOp,
        left: &ast::Expr<'a>,
        right: &ast::Expr<'a>,
        span: ast::Span,
    ) -> Type<'a> {
        let left_type = self.check_expr(left);
        // Pipe checks its right-hand side specially (it desugars into a call),
        // so avoid the generic check_expr here.
        let right_type = if op == ast::BinOp::Pipe {
            Type::Infer
        } else {
            self.check_expr(right)
        };

        use ast::BinOp::*;
        match op {
            Add => {
                if left_type.is_error() || right_type.is_error() {
                    return Type::Named { name: "number" };
                }
                if matches!(left_type, Type::Infer) || matches!(right_type, Type::Infer) {
                    return Type::Infer;
                }
                if let Some(ty) =
                    self.check_newtype_arithmetic(expr, op, &left_type, &right_type, span)
                {
                    return ty;
                }
                if Self::is_number(&left_type) && Self::is_number(&right_type) {
                    Type::Named { name: "number" }
                } else if Self::is_string(&left_type) || Self::is_string(&right_type) {
                    // String concatenation: JS coerces the other operand to string.
                    Type::Named { name: "string" }
                } else if Self::is_promise(&left_type) || Self::is_promise(&right_type) {
                    // Promise<T> + primitive coerces to string in JS.
                    Type::Named { name: "string" }
                } else if matches!(left_type, Type::Newtype { .. })
                    || matches!(right_type, Type::Newtype { .. })
                {
                    self.error_span(
                        span,
                        format!("cannot add types `{left_type}` and `{right_type}`"),
                    );
                    Type::Error
                } else {
                    self.error_span(
                        span,
                        format!("cannot add types `{left_type}` and `{right_type}`"),
                    );
                    Type::Error
                }
            }
            Sub | Mul | Div | Mod => {
                if let Some(rewrite) =
                    self.check_newtype_arithmetic(expr, op, &left_type, &right_type, span)
                {
                    return rewrite;
                }
                if !matches!(left_type, Type::Infer) {
                    self.expect_number(&left_type, left.span());
                }
                if !matches!(right_type, Type::Infer) {
                    self.expect_number(&right_type, right.span());
                }
                Type::Named { name: "number" }
            }
            Eq | Ne | Lt | Le | Gt | Ge => {
                if left_type.is_error() || right_type.is_error() {
                    return Type::Named { name: "boolean" };
                }
                if matches!(left_type, Type::Infer) || matches!(right_type, Type::Infer) {
                    return Type::Named { name: "boolean" };
                }
                if let Some(rewrite) =
                    self.check_newtype_comparison(expr, op, &left_type, &right_type, span)
                {
                    return rewrite;
                }
                if left_type == right_type
                    && (Self::is_number(&left_type)
                        || Self::is_string(&left_type)
                        || Self::is_boolean(&left_type)
                        // `Type` descriptors are interned singletons, so
                        // `==` is identity comparison (deka#529).
                        || matches!(left_type, Type::Named { name: "Type" }))
                {
                    Type::Named { name: "boolean" }
                } else {
                    self.error_span(
                        span,
                        format!("cannot compare types `{left_type}` and `{right_type}`"),
                    );
                    Type::Named { name: "boolean" }
                }
            }
            And | Or => {
                if !matches!(left_type, Type::Infer) {
                    self.expect_boolean(&left_type, left.span());
                }
                if !matches!(right_type, Type::Infer) {
                    self.expect_boolean(&right_type, right.span());
                }
                Type::Named { name: "boolean" }
            }
            Pipe => {
                // Pipe desugars at emit time. Type-check the effective call.
                match right {
                    ast::Expr::Identifier { name, span } => {
                        let callee_type = self.lookup_var(name).unwrap_or(Type::Infer);
                        if let Type::Function {
                            params,
                            ret,
                            optional,
                        } = callee_type
                        {
                            let required = params.len().saturating_sub(optional);
                            if params.len() < 1 || required > 1 {
                                self.error_span(
                                    *span,
                                    format!(
                                        "pipe right-hand side expects 1 argument, found {} parameters",
                                        params.len()
                                    ),
                                );
                            }
                            if !params.is_empty()
                                && !self.is_assignable(&params[0], &left_type)
                                && !matches!(left_type, Type::Infer)
                                && !matches!(params[0], Type::Infer)
                            {
                                self.error_span(
                                    *span,
                                    super::with_union_narrowing_hint(
                                        format!(
                                            "pipe expected argument type `{}`, found type `{left_type}`",
                                            params[0]
                                        ),
                                        &params[0],
                                        &left_type,
                                    ),
                                );
                            }
                            *ret
                        } else {
                            Type::Infer
                        }
                    }
                    ast::Expr::Call {
                        callee,
                        type_args,
                        args,
                        span,
                    } => {
                        let callee_type = self.check_expr(callee);
                        if let Type::Function {
                            params,
                            ret,
                            optional,
                        } = callee_type
                        {
                            let subst = if params.iter().any(|p| contains_param(p))
                                || contains_param(&ret)
                            {
                                self.infer_substitution(type_args, &params, args)
                            } else {
                                HashMap::new()
                            };
                            let substituted_params: Vec<Type<'a>> =
                                params.iter().map(|p| substitute_type(p, &subst)).collect();
                            let substituted_ret = substitute_type(&ret, &subst);

                            let has_hole = args.iter().any(|a| Self::is_hole_expr(a));
                            let provided: Vec<&ast::Expr<'a>> = args.iter().collect();
                            let expected_provided: Vec<&Type<'a>> = if has_hole {
                                substituted_params.iter().collect()
                            } else {
                                substituted_params.iter().skip(1).collect()
                            };

                            for (expected, arg) in expected_provided.iter().zip(provided.iter()) {
                                if Self::is_hole_expr(arg) {
                                    continue;
                                }
                                let arg_type = self.check_expr(arg);
                                if !self.is_assignable(expected, &arg_type) {
                                    self.error_at_expr(
                                        arg,
                                        super::with_union_narrowing_hint(
                                            format!(
                                                "expected argument type `{expected}`, found type `{arg_type}`"
                                            ),
                                            expected,
                                            &arg_type,
                                        ),
                                    );
                                }
                            }

                            if !has_hole {
                                if substituted_params.is_empty() {
                                    self.error_span(
                                        *span,
                                        "pipe right-hand call takes no arguments",
                                    );
                                } else if !self.is_assignable(&substituted_params[0], &left_type)
                                    && !matches!(left_type, Type::Infer)
                                    && !matches!(substituted_params[0], Type::Infer)
                                {
                                    self.error_span(
                                        left.span(),
                                        super::with_union_narrowing_hint(
                                            format!(
                                                "pipe expected argument type `{}`, found type `{left_type}`",
                                                substituted_params[0]
                                            ),
                                            &substituted_params[0],
                                            &left_type,
                                        ),
                                    );
                                }
                            }

                            let required = substituted_params.len().saturating_sub(optional);
                            let effective_count =
                                if has_hole { args.len() } else { args.len() + 1 };
                            if effective_count < required
                                || effective_count > substituted_params.len()
                            {
                                self.error_span(
                                    *span,
                                    format!(
                                        "pipe right-hand call expected {} to {} arguments, found {}",
                                        required,
                                        substituted_params.len(),
                                        effective_count
                                    ),
                                );
                            }

                            substituted_ret
                        } else if matches!(callee_type, Type::Infer) {
                            for arg in args.iter() {
                                self.check_expr(arg);
                            }
                            Type::Infer
                        } else {
                            self.error_span(
                                *span,
                                format!("value of type `{callee_type}` is not callable"),
                            );
                            Type::Error
                        }
                    }
                    _ => {
                        self.error_span(span, "pipe right-hand side must be a function or call");
                        Type::Error
                    }
                }
            }
            Assign => {
                match left {
                    ast::Expr::Identifier { name, .. } => {
                        if !self
                            .mutables
                            .iter()
                            .rev()
                            .any(|scope| scope.contains(*name))
                        {
                            self.error_span(
                                left.span(),
                                format!("cannot assign to immutable variable `{name}`"),
                            );
                        }
                    }
                    ast::Expr::IndexAccess { object, .. } => {
                        let object_type = self.check_expr(object);
                        if matches!(object_type, Type::Array { .. } | Type::Object { .. })
                            && !self.is_mutable_expr(object)
                        {
                            self.error_at_expr(
                                left,
                                self.immutable_mutation_message(
                                    object,
                                    "assign to an indexed element",
                                ),
                            );
                        }
                    }
                    ast::Expr::FieldAccess { object, field, .. } => {
                        let object_type = self.check_expr(object);
                        let field_mutable = self.field_is_mutable(&object_type, field);
                        if !self.is_mutable_expr(object) && !field_mutable {
                            self.error_at_expr(
                                left,
                                self.immutable_field_message(
                                    object,
                                    field,
                                ),
                            );
                        }
                    }
                    _ => {
                        self.error_span(
                            span,
                            "assignment target must be a mutable local variable, field, or index",
                        );
                        return right_type;
                    }
                }
                if !left_type.is_error()
                    && !right_type.is_error()
                    && !self.is_assignable(&left_type, &right_type)
                {
                    self.error_span(
                        span,
                        super::with_union_narrowing_hint(
                            format!("cannot assign type `{right_type}` to `{left_type}`"),
                            &left_type,
                            &right_type,
                        ),
                    );
                }
                right_type
            }
            AddAssign | SubAssign | MulAssign | DivAssign | ModAssign => {
                if let ast::Expr::Identifier { name, .. } = left {
                    if !self
                        .mutables
                        .iter()
                        .rev()
                        .any(|scope| scope.contains(*name))
                    {
                        self.error_span(
                            left.span(),
                            format!("cannot assign to immutable variable `{name}`"),
                        );
                    }
                } else {
                    self.error_span(
                        span,
                        "compound assignment target must be a mutable local variable",
                    );
                }
                if !matches!(left_type, Type::Infer | Type::Error) {
                    self.expect_number(&left_type, left.span());
                }
                if !matches!(right_type, Type::Infer | Type::Error) {
                    self.expect_number(&right_type, right.span());
                }
                left_type
            }
            _ => {
                self.error_span(
                    span,
                    format!("binary operator `{op:?}` is not supported in v2 typeck"),
                );
                Type::Error
            }
        }
    }

    /// Try to typecheck an arithmetic operator where one or both operands are
    /// newtypes. Returns the result type and records an operator rewrite when
    /// applicable. Returns None when no newtype is involved.
    fn check_newtype_arithmetic(
        &mut self,
        expr: &ast::Expr<'a>,
        op: ast::BinOp,
        left_type: &Type<'a>,
        right_type: &Type<'a>,
        span: ast::Span,
    ) -> Option<Type<'a>> {
        use ast::BinOp::*;
        let newtype_name = |t: &Type<'a>| match t {
            Type::Newtype {
                name,
                repr: crate::ast::NewtypeRepr::Number,
            } => Some(*name),
            _ => None,
        };

        let same_newtype = match (left_type, right_type) {
            (
                Type::Newtype {
                    name: l,
                    repr: crate::ast::NewtypeRepr::Number,
                },
                Type::Newtype {
                    name: r,
                    repr: crate::ast::NewtypeRepr::Number,
                },
            ) if l == r => Some(*l),
            _ => None,
        };

        if let Some(name) = same_newtype {
            let rewrite = match op {
                Add | Sub => Some(super::types::OperatorRewrite::NewtypeBinary { name }),
                Div => Some(super::types::OperatorRewrite::NewtypeDiv),
                Mul | Mod => {
                    self.error_span(
                        span,
                        format!("cannot multiply or modulo two `{name}` values; use the payload instead"),
                    );
                    return Some(Type::Error);
                }
                _ => None,
            };
            if let Some(rewrite) = rewrite {
                self.operator_rewrites
                    .insert(expr as *const ast::Expr<'a>, rewrite);
            }
            return Some(match op {
                Div => Type::Named { name: "number" },
                _ => Type::Newtype {
                    name,
                    repr: crate::ast::NewtypeRepr::Number,
                },
            });
        }

        if let Some(name) = newtype_name(left_type) {
            if Self::is_number(right_type) {
                let rewrite = match op {
                    Mul | Div | Mod => Some(super::types::OperatorRewrite::NewtypeScalar {
                        name,
                        side: super::types::NewtypeSide::Left,
                    }),
                    Add | Sub => {
                        self.error_span(
                            span,
                            format!("cannot add or subtract a `{name}` and a raw number"),
                        );
                        return Some(Type::Error);
                    }
                    _ => None,
                };
                if let Some(rewrite) = rewrite {
                    self.operator_rewrites
                        .insert(expr as *const ast::Expr<'a>, rewrite);
                }
                return Some(Type::Newtype {
                    name,
                    repr: crate::ast::NewtypeRepr::Number,
                });
            }
        }

        if let Some(name) = newtype_name(right_type) {
            if Self::is_number(left_type) {
                let rewrite = match op {
                    Mul | Div | Mod => Some(super::types::OperatorRewrite::NewtypeScalar {
                        name,
                        side: super::types::NewtypeSide::Right,
                    }),
                    Add | Sub => {
                        self.error_span(
                            span,
                            format!("cannot add or subtract a raw number and a `{name}`"),
                        );
                        return Some(Type::Error);
                    }
                    _ => None,
                };
                if let Some(rewrite) = rewrite {
                    self.operator_rewrites
                        .insert(expr as *const ast::Expr<'a>, rewrite);
                }
                return Some(Type::Newtype {
                    name,
                    repr: crate::ast::NewtypeRepr::Number,
                });
            }
        }

        None
    }

    /// Try to typecheck a comparison where one or both operands are newtypes.
    /// Same-newtype comparisons compare payloads. Returns None when no newtype
    /// is involved.
    fn check_newtype_comparison(
        &mut self,
        expr: &ast::Expr<'a>,
        op: ast::BinOp,
        left_type: &Type<'a>,
        right_type: &Type<'a>,
        span: ast::Span,
    ) -> Option<Type<'a>> {
        let _ = op;
        match (left_type, right_type) {
            (Type::Newtype { name: l, .. }, Type::Newtype { name: r, .. }) if l == r => {
                self.operator_rewrites.insert(
                    expr as *const ast::Expr<'a>,
                    super::types::OperatorRewrite::NewtypeCompare,
                );
                Some(Type::Named { name: "boolean" })
            }
            (Type::Newtype { name, .. }, other) | (other, Type::Newtype { name, .. }) => {
                self.error_span(span, format!("cannot compare `{name}` with `{other}`"));
                Some(Type::Named { name: "boolean" })
            }
            _ => None,
        }
    }

    fn check_unary(
        &mut self,
        expr: &ast::Expr<'a>,
        op: ast::UnOp,
        operand: &ast::Expr<'a>,
        _span: ast::Span,
    ) -> Type<'a> {
        let operand_type = self.check_expr(operand);
        match op {
            ast::UnOp::Neg | ast::UnOp::Plus => {
                if let Type::Newtype {
                    name,
                    repr: crate::ast::NewtypeRepr::Number,
                } = &operand_type
                {
                    self.operator_rewrites.insert(
                        expr as *const ast::Expr<'a>,
                        super::types::OperatorRewrite::NewtypeUnary { name: *name },
                    );
                    return Type::Newtype {
                        name: *name,
                        repr: crate::ast::NewtypeRepr::Number,
                    };
                }
                self.expect_number(&operand_type, operand.span());
                Type::Named { name: "number" }
            }
            ast::UnOp::Not => {
                self.expect_boolean(&operand_type, operand.span());
                Type::Named { name: "boolean" }
            }
        }
    }

    fn try_check_method_call(
        &mut self,
        call_expr: &ast::Expr<'a>,
        callee: &ast::Expr<'a>,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Option<Type<'a>> {
        let (object, method_name) = match callee {
            ast::Expr::FieldAccess { object, field, .. } => (object, *field),
            _ => return None,
        };

        // Builtin `Name.type()` on `super` declarations (rfd#41, deka#561
        // PR B): the receiver is a type name, not a value, so this must be
        // intercepted BEFORE `check_expr(object)` — a bare struct/enum name
        // is not a value expression. Instance calls (`user.type()`) have a
        // non-identifier object and are unaffected.
        if method_name == "type" {
            if let ast::Expr::Identifier { name, .. } = object {
                if let Some(ty) = self.check_builtin_static_type(call_expr, name, args, span) {
                    return Some(ty);
                }
            }
        }

        let object_type = self.check_expr(object);

        // Builtin `.getType()` (rfd#41, deka#529): returns a first-class
        // `Type` value. User code named `getType` (interface member,
        // receiver method, or primitive extension) shadows the builtin,
        // mirroring the deka#527 shadowing rule; the helper returns `None`
        // in that case so the ordinary paths below handle the call.
        if method_name == "getType" {
            if let Some(ty) = self.check_builtin_get_type(call_expr, &object_type, args, span) {
                return Some(ty);
            }
        }

        if method_name == "signature" {
            if let Some(ty) = self.check_builtin_signature(call_expr, &object_type, args, span) {
                return Some(ty);
            }
        }

        if matches!(method_name, "toJSON" | "parseJSON") {
            if let Some(ret) =
                self.check_builtin_json(call_expr, &object_type, method_name, type_args, args, span)
            {
                return Some(ret);
            }
        }

        // Interface receiver: dispatch is dynamic; validate against the
        // interface signature and enforce mutable-method requirements inferred
        // from satisfying structs.
        if let Type::Interface { name: iface_name } = &object_type {
            let info = self.interfaces.get(iface_name)?;
            let method = info.members.iter().find(|m| match m {
                ast::InterfaceMember::Method { name, .. } => *name == method_name,
                _ => false,
            })?;
            let method_mutable =
                matches!(method, ast::InterfaceMember::Method { mutable: true, .. });
            if method_mutable && !self.is_mutable_expr(object) {
                self.error_at_expr(
                    object,
                    format!("cannot call mutable method `{method_name}` on an immutable receiver"),
                );
            }
            let (params, return_type) = match method {
                ast::InterfaceMember::Method {
                    params,
                    return_type,
                    ..
                } => (*params, return_type.as_ref()),
                _ => unreachable!(),
            };
            let expected_params: Vec<Type<'a>> = params
                .iter()
                .map(|p| match &p.ty {
                    Some(t) => self.resolve_ast_type(t),
                    None => Type::Error,
                })
                .collect();
            if expected_params.len() != args.len() {
                self.error_span(
                    span,
                    format!(
                        "method `{method_name}` on `{iface_name}` expected {} argument{}, found {}",
                        expected_params.len(),
                        if expected_params.len() == 1 { "" } else { "s" },
                        args.len()
                    ),
                );
            } else {
                for (expected, arg) in expected_params.iter().zip(args.iter()) {
                    let arg_type = self.check_expr(arg);
                    if !self.is_assignable(expected, &arg_type) {
                        self.error_at_expr(
                            arg,
                            super::with_union_narrowing_hint(
                                format!("expected argument type `{expected}`, found type `{arg_type}`"),
                                expected,
                                &arg_type,
                            ),
                        );
                    }
                }
            }
            return return_type
                .map(|t| self.resolve_ast_type(t))
                .unwrap_or(Type::None)
                .into();
        }

        let receiver_type = match &object_type {
            Type::Struct { name } => *name,
            Type::Newtype { name, .. } => *name,
            Type::Array { .. } => {
                if is_mutating_array_method(method_name) && !self.is_mutable_expr(object) {
                    self.error_at_expr(
                        object,
                        self.immutable_mutation_message(
                            object,
                            &format!("call mutable method `{method_name}`"),
                        ),
                    );
                }
                // Record `first`/`last`/`pop`/`shift` so the emitter rewrites
                // them to an Option-producing expression. Returning `None`
                // keeps the existing `check_call` flow (argument arity
                // checking against the `BuiltinMethod` signature). Extensions
                // cannot target `Array` receivers (deka#527), so this cannot
                // shadow user code. The mutability guard above already
                // rejects pop/shift on immutable receivers (deka#590's
                // richer message), so no per-method check is needed here.
                if matches!(method_name, "first" | "last" | "pop" | "shift") {
                    self.array_builtin_calls.insert(
                        call_expr as *const ast::Expr,
                        match method_name {
                            "first" => super::types::ArrayAccess::First,
                            "last" => super::types::ArrayAccess::Last,
                            "pop" => super::types::ArrayAccess::Pop,
                            _ => super::types::ArrayAccess::Shift,
                        },
                    );
                }
                return None;
            }
            Type::Named { name } => {
                // Builtin Math-backed methods on `number` (deka#378 step 2,
                // rfd#40 phase 2): record the call site so the emitter
                // rewrites it to a `Math.*` expression. A declared extension
                // of the same name shadows the builtin (deka#527), so record
                // only when none exists.
                if *name == "number"
                    && !self.receiver_methods.contains_key(&("number", method_name))
                {
                    if let Some(kind) = number_math_kind(method_name) {
                        self.number_math_calls
                            .insert(call_expr as *const ast::Expr, kind);
                    }
                }
                // Primitive receiver: a user extension shadows builtin members
                // of the same name. On a miss, fall through to `check_call` so
                // builtin property-functions keep working (deka#527).
                return self.check_primitive_extension_call(
                    call_expr,
                    name,
                    method_name,
                    args,
                    span,
                );
            }
            _ => return None,
        };

        let mut embed_path = Vec::new();
        let info = self.find_receiver_method(receiver_type, method_name, &mut embed_path)?;

        if info.mutable && !self.is_mutable_expr(object) {
            self.error_at_expr(
                object,
                format!("cannot call mutable method `{method_name}` on an immutable receiver"),
            );
        }

        self.check_method_call_args(method_name, receiver_type, &info, args, span)
            .into()
    }

    /// Check a builtin `.getType()` call (rfd#41, deka#529). Returns `None`
    /// when user code shadows the builtin (a declared receiver method,
    /// interface member, or primitive extension named `getType`) so the
    /// ordinary method paths handle the call. Otherwise validates arity,
    /// records the rewrite for the emitter (`__deka_type_of(x)`), and returns
    /// the `Type` descriptor type.
    fn check_builtin_get_type(
        &mut self,
        call_expr: &ast::Expr<'a>,
        object_type: &Type<'a>,
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Option<Type<'a>> {
        // User code shadows the builtin.
        match object_type {
            Type::Struct { name } | Type::Newtype { name, .. } => {
                if self
                    .find_receiver_method(name, "getType", &mut Vec::new())
                    .is_some()
                {
                    return None;
                }
            }
            Type::Named { name } => {
                if self.receiver_methods.contains_key(&(*name, "getType")) {
                    return None;
                }
            }
            Type::Interface { name } => {
                let info = self.interfaces.get(name)?;
                let declared = info.members.iter().any(|m| match m {
                    ast::InterfaceMember::Method { name: n, .. } => *n == "getType",
                    _ => false,
                });
                if declared {
                    return None;
                }
            }
            _ => {}
        }

        if !args.is_empty() {
            self.error_span(span, "`getType` expects no arguments".to_string());
            return Some(Type::Error);
        }

        // Record the rewrite for every real receiver kind, including
        // Infer/Var: an unrecorded `v.getType()` on an unsafe-derived value
        // would emit verbatim and miscompile silently. Error/None/Never
        // receivers fall through to their existing diagnostics.
        match object_type {
            Type::Error | Type::None | Type::Never => return None,
            _ => {}
        }
        self.type_of_calls.insert(call_expr as *const ast::Expr<'a>);
        Some(Type::Named { name: "Type" })
    }

    /// Check builtin `Name.type()` on a `super` declaration (rfd#41, deka#561
    /// PR B). Returns `None` when `name` is not a struct/enum declaration so
    /// the ordinary path reports `unknown identifier`; otherwise validates
    /// arity and the super mark, records the rewrite for the emitter (the
    /// interned `__deka_super_desc$<Name>` const), and returns `Type`.
    fn check_builtin_static_type(
        &mut self,
        call_expr: &ast::Expr<'a>,
        name: &'a str,
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Option<Type<'a>> {
        let (decl_name, kind) = if self.structs.contains_key(name) {
            (name, "struct")
        } else if self.enums.contains_key(name) {
            (name, "enum")
        } else if let Some(target) = self.alias_target_decl(name) {
            let kind = if self.structs.contains_key(target) {
                "struct"
            } else {
                "enum"
            };
            (target, kind)
        } else if self.newtypes.contains_key(name) || self.aliases.contains_key(name) {
            self.error_span(
                span,
                format!(
                    "`{name}.type()` is only available on `super struct` and `super enum` declarations"
                ),
            );
            return Some(Type::Error);
        } else {
            return None;
        };

        if !args.is_empty() {
            self.error_span(span, "`type` expects no arguments".to_string());
            return Some(Type::Error);
        }

        if !self.is_super_decl(decl_name) {
            self.error_span(
                span,
                format!(
                    "`{decl_name}` does not carry runtime type information; declare it \
                     `super {kind} {decl_name}` to use `{decl_name}.type()`"
                ),
            );
            return Some(Type::Error);
        }

        // Imported super declarations have no locally built tree yet; build
        // on demand — including the recursive group, since a tree may
        // reference sibling declarations via `Recurse` nodes and the emitter
        // interns consts for the whole group. Errors surface at the call
        // site naming the reason.
        let mut worklist: Vec<&'a str> = vec![decl_name];
        while let Some(n) = worklist.pop() {
            if self.super_trees.contains_key(n) {
                continue;
            }
            let tree = if self.structs.contains_key(n) {
                self.super_struct_tree(n, span)
            } else {
                self.super_enum_tree(n, span)
            };
            match tree {
                Ok(tree) => {
                    let mut refs = Vec::new();
                    super::descriptor::collect_recurse_refs(&tree, &mut refs);
                    self.super_trees.insert(n, tree);
                    for referenced in refs {
                        if !self.super_trees.contains_key(referenced) {
                            worklist.push(referenced);
                        }
                    }
                }
                Err(message) => {
                    self.error_span(
                        span,
                        format!("`{decl_name}.type()` cannot be described here: {message}"),
                    );
                    return Some(Type::Error);
                }
            }
        }

        let tree = self.super_trees.get(decl_name).unwrap().clone();
        self.static_type_calls.insert(
            call_expr as *const ast::Expr<'a>,
            super::descriptor::StaticTypeCall {
                tree: Some(tree),
                param: None,
            },
        );
        Some(Type::Named { name: "Type" })
    }

    /// Check `.signature()`, which describes the receiver's declared type at
    /// compile time. User-defined methods with the same name shadow it.
    fn check_builtin_signature(
        &mut self,
        call_expr: &ast::Expr<'a>,
        object_type: &Type<'a>,
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Option<Type<'a>> {
        match object_type {
            Type::Struct { name } | Type::Newtype { name, .. } => {
                if self
                    .find_receiver_method(name, "signature", &mut Vec::new())
                    .is_some()
                {
                    return None;
                }
            }
            Type::Named { name } => {
                if self.receiver_methods.contains_key(&(*name, "signature")) {
                    return None;
                }
            }
            Type::Interface { name } => {
                let info = self.interfaces.get(name)?;
                if info.members.iter().any(|m| {
                    matches!(m,
                    ast::InterfaceMember::Method { name: n, .. } if *n == "signature")
                }) {
                    return None;
                }
            }
            _ => {}
        }
        if !args.is_empty() {
            self.error_span(span, "`signature` expects no arguments".to_string());
            return Some(Type::Error);
        }
        if matches!(object_type, Type::Error | Type::None | Type::Never) {
            return None;
        }
        let tree = match self.descriptor_tree(object_type, span) {
            Ok(tree) => tree,
            Err(message) => {
                self.error_span(
                    span,
                    message.replace("at this `super` call site", "at this `signature` call site"),
                );
                return Some(Type::Error);
            }
        };
        self.signature_calls
            .insert(call_expr as *const ast::Expr<'a>, tree);
        Some(Type::Named { name: "Type" })
    }

    fn check_builtin_json(
        &mut self,
        call_expr: &ast::Expr<'a>,
        object_type: &Type<'a>,
        method_name: &str,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Option<Type<'a>> {
        // User-defined receiver methods take precedence over the builtin.
        match object_type {
            Type::Struct { name } | Type::Newtype { name, .. } => {
                if self
                    .find_receiver_method(name, method_name, &mut Vec::new())
                    .is_some()
                {
                    return None;
                }
            }
            Type::Named { name } => {
                if self.receiver_methods.contains_key(&(*name, method_name)) {
                    return None;
                }
            }
            Type::Interface { name } => {
                let Some(info) = self.interfaces.get(name) else {
                    return None;
                };
                if info.members.iter().any(|member| {
                    matches!(member,
                    ast::InterfaceMember::Method { name, .. } if *name == method_name)
                }) {
                    return None;
                }
            }
            _ => {}
        }
        let operation = if method_name == "toJSON" {
            super::descriptor::JsonOperation::ToJson
        } else {
            super::descriptor::JsonOperation::ParseJson
        };
        if !args.is_empty() {
            self.error_span(span, format!("`{method_name}` expects no arguments"));
            return Some(Type::Error);
        }
        if operation == super::descriptor::JsonOperation::ToJson && !type_args.is_empty() {
            self.error_span(span, "`toJSON` does not accept type arguments");
            return Some(Type::Error);
        }
        if operation == super::descriptor::JsonOperation::ParseJson {
            if !matches!(object_type, Type::Named { name: "string" }) {
                return None;
            }
            if type_args.len() != 1 {
                self.error_span(span, "`parseJSON` expects exactly one type argument");
                return Some(Type::Error);
            }
            let target = self.resolve_ast_type(&type_args[0]);
            let shape = match self.descriptor_tree(&target, span) {
                Ok(shape) => shape,
                Err(message) => {
                    self.error_span(
                        span,
                        message
                            .replace("at this `super` call site", "at this `parseJSON` call site"),
                    );
                    return Some(Type::Error);
                }
            };
            if let Err(message) = json_shape_error(&shape, None) {
                self.error_span(span, message);
                return Some(Type::Error);
            }
            self.json_calls.insert(
                call_expr as *const ast::Expr<'a>,
                super::descriptor::JsonCall { operation, shape },
            );
            return Some(Type::Generic {
                base: "Result",
                args: vec![target, Type::Named { name: "string" }],
            });
        }
        if matches!(object_type, Type::Error | Type::None | Type::Never) {
            return None;
        }
        let shape = match self.descriptor_tree(object_type, span) {
            Ok(shape) => shape,
            Err(message) => {
                self.error_span(
                    span,
                    message.replace("at this `super` call site", "at this `toJSON` call site"),
                );
                return Some(Type::Error);
            }
        };
        if let Err(message) = json_shape_error(&shape, None) {
            self.error_span(span, message);
            return Some(Type::Error);
        }
        self.json_calls.insert(
            call_expr as *const ast::Expr<'a>,
            super::descriptor::JsonCall { operation, shape },
        );
        Some(Type::Named { name: "string" })
    }

    /// Resolve a method call on a primitive receiver (deka#527). A declared
    /// extension is rewritten to a free-function call; a miss falls through
    /// to `check_call`, except when the same method name is declared on a
    /// different primitive, where the diagnostic names the receiver type.
    fn check_primitive_extension_call(
        &mut self,
        call_expr: &ast::Expr<'a>,
        receiver_name: &'a str,
        method_name: &'a str,
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Option<Type<'a>> {
        let info = match self.receiver_methods.get(&(receiver_name, method_name)) {
            Some(info) => info.clone(),
            None => {
                let declared_on = self
                    .receiver_methods
                    .keys()
                    .find(|(rt, mn)| {
                        *mn == method_name
                            && super::is_primitive_receiver_name(rt)
                            && *rt != receiver_name
                    })
                    .map(|(rt, _)| *rt);
                if let Some(declared_on) = declared_on {
                    self.error_span(
                        span,
                        format!(
                            "method `{method_name}` is declared on `{declared_on}`, not `{receiver_name}`"
                        ),
                    );
                    return Type::Error.into();
                }
                return None;
            }
        };

        // Primitives cannot carry a prototype, so the emitter rewrites this
        // call to a module-local free function named `method$receiver`.
        let mangled = format!("{method_name}${receiver_name}");
        self.method_calls.insert(
            call_expr as *const ast::Expr<'a>,
            ast::MethodTarget {
                mangled,
                embed_path: Vec::new(),
            },
        );

        Some(self.check_method_call_args(method_name, receiver_name, &info, args, span))
    }

    /// Check call arguments against a receiver method's resolved parameter
    /// types (collected in the declaring module, deka#494) and produce the
    /// call's result type.
    fn check_method_call_args(
        &mut self,
        method_name: &'a str,
        receiver_type: &str,
        info: &super::MethodInfo<'a>,
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        let expected_params: Vec<Type<'a>> = info.param_types.clone();

        if expected_params.len() != args.len() {
            self.error_span(
                span,
                format!(
                    "method `{method_name}` on `{receiver_type}` expected {} argument{}, found {}",
                    expected_params.len(),
                    if expected_params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
            );
        } else {
            for (expected, arg) in expected_params.iter().zip(args.iter()) {
                let arg_type = self.check_expr(arg);
                if !self.is_assignable(expected, &arg_type) {
                    self.error_at_expr(
                        arg,
                        super::with_union_narrowing_hint(
                            format!("expected argument type `{expected}`, found type `{arg_type}`"),
                            expected,
                            &arg_type,
                        ),
                    );
                }
            }
        }

        info.resolved_return.clone().unwrap_or(Type::None)
    }

    /// Look up a receiver method on a struct type, recursively searching
    /// embedded structs. On success, returns the method info and the path of
    /// embed names that must be traversed to reach the method's owner.
    fn find_receiver_method(
        &self,
        receiver_type: &'a str,
        method_name: &'a str,
        path: &mut Vec<&'a str>,
    ) -> Option<super::MethodInfo<'a>> {
        if let Some(info) = self.receiver_methods.get(&(receiver_type, method_name)) {
            return Some(info.clone());
        }
        // Newtypes do not support embedding, so there is nothing else to search.
        if self.newtypes.contains_key(receiver_type) {
            return None;
        }
        let info = self.structs.get(receiver_type)?;
        for embed in info.embeds {
            path.push(embed.name);
            if let Some(found) = self.find_receiver_method(embed.name, method_name, path) {
                return Some(found);
            }
            path.pop();
        }
        None
    }

    /// Returns true if the expression denotes a mutable location.
    fn is_mutable_expr(&self, expr: &ast::Expr<'a>) -> bool {
        match expr {
            ast::Expr::Identifier { name, .. } => self
                .mutables
                .iter()
                .rev()
                .any(|scope| scope.contains(*name)),
            ast::Expr::FieldAccess { object, .. } => self.is_mutable_expr(object),
            _ => false,
        }
    }

    fn immutable_mutation_message(&self, expr: &ast::Expr<'a>, operation: &str) -> String {
        let binding = self
            .immutable_binding_name(expr)
            .map(|name| format!("const binding `{name}`"))
            .unwrap_or_else(|| "an immutable receiver".to_string());
        format!(
            "cannot {operation} on an immutable receiver ({binding}; use `let` to allow mutation)"
        )
    }

    fn immutable_field_message(&self, expr: &ast::Expr<'a>, field: &str) -> String {
        let binding = self
            .immutable_binding_name(expr)
            .map(|name| format!("const binding `{name}`"))
            .unwrap_or_else(|| "an immutable receiver".to_string());
        format!(
            "cannot assign to field `{field}` of immutable value ({binding}; use `let` to allow mutation)"
        )
    }

    fn immutable_binding_name(&self, expr: &ast::Expr<'a>) -> Option<&'a str> {
        match expr {
            ast::Expr::Identifier { name, .. } => Some(*name),
            ast::Expr::FieldAccess { object, .. }
            | ast::Expr::IndexAccess { object, .. }
            | ast::Expr::Paren { expr: object, .. } => self.immutable_binding_name(object),
            _ => None,
        }
    }

    /// Returns true if `field` is declared mutable on `receiver_type`.
    /// Struct fields are never mutable in isolation; interface fields may be
    /// declared with `mut`.
    fn field_is_mutable(&self, receiver_type: &Type<'a>, field: &str) -> bool {
        match receiver_type {
            Type::Interface { name } => self
                .interfaces
                .get(name)
                .and_then(|info| {
                    info.members.iter().find(|m| match m {
                        ast::InterfaceMember::Field { name: n, .. } => n == &field,
                        _ => false,
                    })
                })
                .map(|m| match m {
                    ast::InterfaceMember::Field { mutable, .. } => *mutable,
                    _ => false,
                })
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Returns true if the named interface declares any `mut` field. Such
    /// interfaces grant mutation through their parameters, so an immutable
    /// value must not be passed as one (deka#590).
    fn interface_has_mut_fields(&self, name: &str) -> bool {
        self.interfaces.get(name).map_or(false, |info| {
            info.members.iter().any(|m| match m {
                ast::InterfaceMember::Field { mutable, .. } => *mutable,
                _ => false,
            })
        })
    }

    fn check_call(
        &mut self,
        expr: &ast::Expr<'a>,
        callee: &ast::Expr<'a>,
        type_args: &'a [ast::Type<'a>],
        args: &'a [ast::Expr<'a>],
        span: ast::Span,
    ) -> Type<'a> {
        // `unwrap(x)` with no `or` block. The parser only claims the name when
        // `or` follows, so a program with its own `unwrap` function is
        // unaffected and reaches this only when the name is genuinely unbound
        // (deka#445).
        if let ast::Expr::Identifier { name: "unwrap", .. } = callee {
            if self.lookup_var("unwrap").is_none() {
                for arg in args.iter() {
                    self.check_expr(arg);
                }
                self.error_span(
                    span,
                    "`unwrap` needs the absent case handled; add `or { … }`, or match on the value"
                        .to_string(),
                );
                return Type::Error;
            }
        }

        // `isset` was removed in deka#416. It existed only to test presence on
        // an interface `?:` field, which is now an `Option` like every other
        // maybe-absent value. Name it explicitly rather than letting it fall
        // through to `unknown identifier`, because the useful thing to say is
        // what replaced it.
        if let ast::Expr::Identifier { name: "isset", .. } = callee {
            for arg in args.iter() {
                self.check_expr(arg);
            }
            self.error_span(
                span,
                "`isset` was removed: optional fields are `Option<T>`, so match on the value \
                 (`match (x) { Some(v) => …, None => … }`)"
                    .to_string(),
            );
            return Type::Error;
        }

        // `panic(msg)` / `deka.panic(msg)`: never-returning lang item (RFD 21).
        if is_panic_callee(callee) {
            if args.len() != 1 {
                self.error_span(span, "`panic` expects exactly one argument");
                return Type::Never;
            }
            let arg_type = self.check_expr(&args[0]);
            if !self.is_assignable(&Type::Named { name: "string" }, &arg_type) {
                self.error_at_expr(
                    &args[0],
                    format!("expected type `string`, found type `{arg_type}`"),
                );
            }
            return Type::Never;
        }

        // Newtype constructor: `Cents(500)` is only legal in the declaring module.
        if let ast::Expr::Identifier { name, .. } = callee {
            if let Some(info) = self.newtypes.get(name).cloned() {
                if args.len() != 1 {
                    self.error_span(
                        span,
                        format!("newtype constructor `{name}` expects exactly one argument"),
                    );
                    return Type::Error;
                }
                let arg_type = self.check_expr(&args[0]);
                let expected = Type::from_newtype_repr(info.repr);
                if !self.is_assignable(&expected, &arg_type) {
                    self.error_at_expr(
                        &args[0],
                        format!("expected `{expected}` for newtype `{name}`, found `{arg_type}`"),
                    );
                }
                return Type::Newtype {
                    name,
                    repr: info.repr,
                };
            }
        }

        // Primitive conversion: `string(x)`, `parseNumber(x)`,
        // `unboxNumber(x)`, `toNumber(x)` — always public, no import
        // (#364). `number(x)` is removed in PR 2.
        if let ast::Expr::Identifier { name, .. } = callee {
            if let Some(conversion) = primitive_conversion_name(name) {
                if args.len() != 1 {
                    self.error_span(
                        span,
                        format!("`{name}` conversion expects exactly one argument"),
                    );
                    return Type::Error;
                }
                let arg_type = self.check_expr(&args[0]);
                let (kind, ret) = match conversion {
                    PrimitiveConversionName::String => {
                        let ret = Type::Named { name: "string" };
                        use crate::ast::NewtypeRepr as Repr;
                        match &arg_type {
                            Type::Newtype {
                                repr: Repr::String, ..
                            } => (Some(super::types::UnwrapKind::Payload), ret),
                            Type::Named { name: "string" } => {
                                (Some(super::types::UnwrapKind::Identity), ret)
                            }
                            Type::Named {
                                name: "number" | "boolean",
                            } => (Some(super::types::UnwrapKind::WidenToString), ret),
                            // Rejected, not widened: `String(undefined)` is
                            // "undefined", `String({})` is "[object Object]" —
                            // total but silently wrong. Ask for an annotation
                            // instead of trusting a value the checker cannot see
                            // (deka#370 review).
                            Type::Infer => (None, ret),
                            _ => (None, ret),
                        }
                    }
                    PrimitiveConversionName::ParseNumber => {
                        let ret = Type::Option {
                            inner: Box::new(Type::Named { name: "number" }),
                        };
                        match &arg_type {
                            Type::Named { name: "string" } => {
                                (Some(super::types::UnwrapKind::StringToOptionNumber), ret)
                            }
                            Type::Infer => (None, ret),
                            _ => (None, ret),
                        }
                    }
                    PrimitiveConversionName::UnboxNumber => {
                        let ret = Type::Named { name: "number" };
                        use crate::ast::NewtypeRepr as Repr;
                        match &arg_type {
                            Type::Newtype {
                                repr: Repr::Number, ..
                            } => (Some(super::types::UnwrapKind::Payload), ret),
                            Type::Infer => (None, ret),
                            _ => (None, ret),
                        }
                    }
                    PrimitiveConversionName::ToNumber => {
                        let ret = Type::Named { name: "number" };
                        match &arg_type {
                            Type::Named { name: "number" } => {
                                (Some(super::types::UnwrapKind::Identity), ret)
                            }
                            Type::Named { name: "boolean" } => {
                                (Some(super::types::UnwrapKind::WidenToNumber), ret)
                            }
                            Type::Infer => (None, ret),
                            _ => (None, ret),
                        }
                    }
                };
                if let Some(kind) = kind {
                    self.unwrap_calls.insert(expr as *const ast::Expr<'a>, kind);
                } else if !arg_type.is_error() {
                    if matches!(arg_type, Type::Infer) {
                        self.error_at_expr(
                            &args[0],
                            format!(
                                "cannot convert a value of unknown type to `{name}`; add a type annotation"
                            ),
                        );
                    } else {
                        self.error_at_expr(
                            &args[0],
                            format!("cannot convert `{arg_type}` to `{name}`"),
                        );
                    }
                }
                return ret;
            }
        }

        let callee_type = self.check_expr(callee);

        match callee_type {
            Type::Function {
                params,
                ret,
                optional,
            } => {
                // Build a substitution for any type parameters appearing in the
                // function signature. Explicit type args are used when present;
                // otherwise we try to infer from the first argument.
                let subst = if params.iter().any(|p| contains_param(p)) || contains_param(&ret) {
                    self.infer_substitution(type_args, &params, args)
                } else {
                    HashMap::new()
                };

                // Parameters inference left unsolved name no type the call
                // could pin: they are unconstrained (`Var`), not unresolved.
                let substituted_params: Vec<Type<'a>> = params
                    .iter()
                    .map(|p| unsolved_params_to_var(&substitute_type(p, &subst), &subst))
                    .collect();
                let substituted_ret = unsolved_params_to_var(&substitute_type(&ret, &subst), &subst);

                let hole_positions: Vec<usize> = args
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| Self::is_hole_expr(a))
                    .map(|(i, _)| i)
                    .collect();

                if hole_positions.len() > 1 {
                    self.error_span(
                        span,
                        "function capture requires exactly one hole, found multiple".to_string(),
                    );
                }

                if !hole_positions.is_empty() {
                    // Partial application: `add(1, _)` becomes a function that
                    // takes the hole arguments and forwards them.
                    for (i, (expected, arg)) in
                        substituted_params.iter().zip(args.iter()).enumerate()
                    {
                        if Self::is_hole_expr(arg) {
                            continue;
                        }
                        let arg_type = self.check_expr(arg);
                        if !self.is_assignable(expected, &arg_type) {
                            self.error_at_expr(
                                arg,
                                super::with_union_narrowing_hint(
                                    format!(
                                        "expected argument type `{expected}`, found type `{arg_type}`"
                                    ),
                                    expected,
                                    &arg_type,
                                ),
                            );
                        }
                    }
                    let hole_types: Vec<Type<'a>> = hole_positions
                        .iter()
                        .map(|i| substituted_params[*i].clone())
                        .collect();
                    return Type::Function {
                        params: hole_types,
                        ret: Box::new(substituted_ret),
                        optional: 0,
                    };
                }

                let required = substituted_params.len().saturating_sub(optional);
                if args.len() < required || args.len() > substituted_params.len() {
                    let expected_msg = if optional > 0 {
                        format!("{} to {} arguments", required, substituted_params.len())
                    } else {
                        format!(
                            "{} argument{}",
                            substituted_params.len(),
                            if substituted_params.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        )
                    };
                    self.error_span(
                        span,
                        format!("expected {}, found {}", expected_msg, args.len()),
                    );
                } else {
                    for (expected, arg) in substituted_params.iter().zip(args.iter()) {
                        let arg_type = self.check_expr(arg);
                        if !self.is_assignable(expected, &arg_type) {
                            self.error_at_expr(
                                arg,
                                super::with_union_narrowing_hint(
                                    format!(
                                        "expected argument type `{expected}`, found type `{arg_type}`"
                                    ),
                                    expected,
                                    &arg_type,
                                ),
                            );
                        }
                        // A mut field on an interface is the type-level grant to
                        // mutate through it. Mutating a value the caller
                        // cannot mutate was previously caught by the
                        // Object.freeze on const literals at emit (deka#590);
                        // with the freeze gone the call site is the guard:
                        // passing an immutable receiver to an interface with
                        // mut fields would let mutation through it succeed
                        // silently. Generalises #591's immutable-receiver
                        // rule from builtins to interface parameters.
                        if let Type::Interface { name: iface_name } = expected {
                            if self.interface_has_mut_fields(iface_name)
                                && !self.is_mutable_expr(arg)
                            {
                                self.error_at_expr(
                                    arg,
                                    format!(
                                        "cannot pass an immutable value as interface `{iface_name}` with mutable fields (bind it with `let` to allow mutation)"
                                    ),
                                );
                            }
                        }
                    }
                }
                substituted_ret
            }
            Type::Error => Type::Error,
            Type::Infer | Type::Var => {
                // Imported or otherwise externally-provided binding with no
                // known type, or an unconstrained one. Treat the call as opaque
                // rather than erroring (deka#468).
                for arg in args.iter() {
                    self.check_expr(arg);
                }
                Type::Infer
            }
            Type::Struct { name } => {
                // Factory-call syntax: Person({ name: "Ada" }) is equivalent to
                // Person { name: "Ada" }.
                if args.len() != 1 {
                    self.error_span(
                        span,
                        format!("struct factory `{name}` expects exactly one argument"),
                    );
                    return Type::Error;
                }
                let info = match self.structs.get(name).cloned() {
                    Some(info) => info,
                    None => {
                        self.error_span(span, format!("unknown struct `{name}`"));
                        return Type::Error;
                    }
                };
                let arg = &args[0];
                match arg {
                    ast::Expr::Object { fields, .. } => {
                        let mapped: Vec<(&'a str, &ast::Expr<'a>, ast::Span)> =
                            fields.iter().map(|f| (f.key, &f.value, f.span)).collect();
                        self.check_struct_literal_fields(name, &info, &mapped, span);
                    }
                    _ => {
                        self.error_at_expr(
                            arg,
                            format!("struct factory `{name}` expects an object literal argument"),
                        );
                    }
                }
                Type::Struct { name }
            }
            other => {
                self.error_span(span, format!("value of type `{other}` is not callable"));
                Type::Error
            }
        }
    }
}

fn json_shape_error(
    shape: &super::descriptor::DescriptorTree<'_>,
    field: Option<&str>,
) -> Result<(), String> {
    use super::descriptor::DescriptorTree as T;
    match shape {
        T::Leaf {
            kind: "unknown",
            name,
        } => Err(match field {
            Some(field) => format!("cannot serialize field `{field}` of unknown type `{name}`"),
            None => format!("cannot serialize unknown type `{name}`"),
        }),
        T::Interface { name } => Err(match field {
            Some(field) => format!("cannot serialize field `{field}` of interface `{name}`"),
            None => format!("cannot serialize interface `{name}`"),
        }),
        // JSON walks with `allow_recurse: false`, so a `Recurse` node should
        // never reach here -- the walker errors on the cycle first. Reject
        // rather than panic: an unreachable arm that becomes reachable is how
        // a refactor turns a diagnostic into a crash.
        T::Recurse { name } => Err(match field {
            Some(field) => format!("cannot serialize field `{field}`: recursive type `{name}`"),
            None => format!("cannot serialize recursive type `{name}`"),
        }),
        T::Struct { fields, .. } => {
            for item in fields {
                json_shape_error(&item.ty, Some(item.name))?;
            }
            Ok(())
        }
        T::Newtype { repr, .. } | T::Option { inner: repr } | T::Array { elem: repr } => {
            json_shape_error(repr, field)
        }
        T::Enum { cases, .. } => {
            for (_, payload) in cases {
                if let Some(payload) = payload {
                    json_shape_error(payload, field)?;
                }
            }
            Ok(())
        }
        T::Union { members } => {
            for member in members {
                json_shape_error(member, field)?;
            }
            Ok(())
        }
        T::Leaf { .. } => Ok(()),
    }
}

impl<'a> Checker<'a> {
    fn infer_substitution(
        &mut self,
        explicit_type_args: &'a [ast::Type<'a>],
        function_params: &[Type<'a>],
        call_args: &'a [ast::Expr<'a>],
    ) -> HashMap<&'a str, Type<'a>> {
        let param_names: Vec<&'a str> = collect_param_names(function_params);

        if !explicit_type_args.is_empty() {
            let mut subst = HashMap::new();
            if explicit_type_args.len() != param_names.len() {
                // Error reported at call site; return empty substitution.
                return subst;
            }
            for (name, ty) in param_names.iter().zip(explicit_type_args.iter()) {
                subst.insert(*name, self.resolve_ast_type(ty));
            }
            return subst;
        }

        // No explicit type args: infer from arguments.
        //
        // This used to match only a *top-level* `Type::Param`, so `fn head<T>(xs:
        // Array<T>)` inferred nothing and reported `expected Array<T>, found
        // Array<number>`. infer_type_args descends through Array, Option and
        // nested generics, and is the same helper the enum constructors use.
        let mut subst = HashMap::new();
        for (param_ty, arg) in function_params.iter().zip(call_args.iter()) {
            if !contains_param(param_ty) {
                continue;
            }
            let arg_type = self.check_expr(arg);
            infer_type_args(param_ty, &arg_type, &param_names, &mut subst);
        }
        subst
    }
}

fn collect_param_names<'a>(tys: &[Type<'a>]) -> Vec<&'a str> {
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for ty in tys {
        collect_param_names_rec(ty, &mut names, &mut seen);
    }
    names
}

fn collect_param_names_rec<'a>(
    ty: &Type<'a>,
    names: &mut Vec<&'a str>,
    seen: &mut std::collections::HashSet<&'a str>,
) {
    match ty {
        Type::Param { name } => {
            if seen.insert(*name) {
                names.push(*name);
            }
        }
        Type::Option { inner } => collect_param_names_rec(inner, names, seen),
        Type::Array { elem } => collect_param_names_rec(elem, names, seen),
        Type::Function { params, ret, .. } => {
            for p in params {
                collect_param_names_rec(p, names, seen);
            }
            collect_param_names_rec(ret, names, seen);
        }
        Type::Generic { args, .. } => {
            for a in args {
                collect_param_names_rec(a, names, seen);
            }
        }
        _ => {}
    }
}

fn contains_param(ty: &Type<'_>) -> bool {
    match ty {
        Type::Param { .. } => true,
        Type::Option { inner } => contains_param(inner),
        Type::Array { elem } => contains_param(elem),
        Type::Function { params, ret, .. } => {
            params.iter().any(contains_param) || contains_param(ret)
        }
        Type::Generic { args, .. } => args.iter().any(contains_param),
        _ => false,
    }
}

/// A type parameter the call site left unsolved names no type at all: it is
/// unconstrained, not unresolved, and takes the `Var` marker (deka#468).
/// Applied after call-site substitution, so `map` on a callback of unknown
/// type yields `Array<Var>` — the type it declared before it gained a type
/// parameter (deka#467) — while a callback of known type solves the
/// parameter to a real type.
fn unsolved_params_to_var<'a>(ty: &Type<'a>, subst: &HashMap<&'a str, Type<'a>>) -> Type<'a> {
    match ty {
        Type::Param { name } if !subst.contains_key(name) => Type::Var,
        Type::Option { inner } => Type::Option {
            inner: Box::new(unsolved_params_to_var(inner, subst)),
        },
        Type::Array { elem } => Type::Array {
            elem: Box::new(unsolved_params_to_var(elem, subst)),
        },
        Type::Function {
            params,
            ret,
            optional,
        } => Type::Function {
            params: params
                .iter()
                .map(|p| unsolved_params_to_var(p, subst))
                .collect(),
            ret: Box::new(unsolved_params_to_var(ret, subst)),
            optional: *optional,
        },
        Type::Generic { base, args } => Type::Generic {
            base,
            args: args
                .iter()
                .map(|a| unsolved_params_to_var(a, subst))
                .collect(),
        },
        _ => ty.clone(),
    }
}

/// Replace type parameters according to `subst`.
/// Structurally match a declared type against an actual one, binding any
/// declared type parameter it encounters. Used to infer `Box<number>` from
/// `Box.Full(5)` where the case is declared `Full(T)` (deka#372).
fn infer_type_args<'a>(
    declared: &Type<'a>,
    actual: &Type<'a>,
    params: &[&'a str],
    out: &mut HashMap<&'a str, Type<'a>>,
) {
    match (declared, actual) {
        (Type::Param { name }, concrete) if params.contains(name) => {
            out.entry(name).or_insert_with(|| concrete.clone());
        }
        (Type::Option { inner: d }, Type::Option { inner: a }) => {
            infer_type_args(d, a, params, out)
        }
        (Type::Array { elem: d }, Type::Array { elem: a }) => infer_type_args(d, a, params, out),
        // Function-typed parameters carry type parameters too: `map`'s
        // `(T -> U) -> Array<U>` solves U from the callback's return type
        // (deka#467). Positional binding is a heuristic (parameters are
        // contravariant), but it agrees with the function-subtyping check on
        // the argument that follows.
        (
            Type::Function {
                params: dp,
                ret: dr,
                ..
            },
            Type::Function {
                params: ap,
                ret: ar,
                ..
            },
        ) if dp.len() == ap.len() => {
            for (d, a) in dp.iter().zip(ap.iter()) {
                infer_type_args(d, a, params, out);
            }
            infer_type_args(dr, ar, params, out);
        }
        (Type::Generic { base: db, args: da }, Type::Generic { base: ab, args: aa })
            if db == ab && da.len() == aa.len() =>
        {
            for (d, a) in da.iter().zip(aa.iter()) {
                infer_type_args(d, a, params, out);
            }
        }
        _ => {}
    }
}

fn substitute_type<'a>(ty: &Type<'a>, subst: &HashMap<&'a str, Type<'a>>) -> Type<'a> {
    super::types::substitute_type(ty, subst)
}
