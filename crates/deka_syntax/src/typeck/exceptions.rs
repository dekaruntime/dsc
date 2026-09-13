//! Frame-local exception consumption and escape judgment (rfd#62).
use super::{Checker, ExceptionEmit, types::Type};
use crate::ast;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Use {
    #[default]
    Value,
    Match,
    Return,
    Arm,
    Statement,
    Await,
    Convert,
}

fn channels<'a>(ty: &Type<'a>, channel: &str) -> Option<(Type<'a>, Type<'a>)> {
    match ty {
        Type::Generic { base, args } if *base == channel && args.len() == 2 => {
            Some((args[0].clone(), args[1].clone()))
        }
        _ => None,
    }
}

/// Localize named types in an imported Exception signature to the actual
/// imported constructor bindings. Comparing original spellings is unsound:
/// two modules can both export a struct named Fault with different factories.
pub(super) fn localize_export<'a>(
    ty: &Type<'a>,
    specs: &[ast::ImportSpec<'a>],
    exports: &super::ModuleExports<'a>,
) -> Type<'a> {
    fn has_exception(ty: &Type<'_>) -> bool {
        match ty {
            Type::Generic {
                base: "Exception", ..
            } => true,
            Type::Generic {
                base: "Promise",
                args,
            } => args.iter().any(has_exception),
            Type::Function { ret, .. } => has_exception(ret),
            _ => false,
        }
    }
    fn rename<'a>(ty: &Type<'a>, names: &HashMap<&'a str, &'a str>) -> Type<'a> {
        let name = |n: &'a str| names.get(n).copied().unwrap_or(n);
        match ty {
            Type::Struct { name: n } => Type::Struct { name: name(n) },
            Type::Named { name: n } => Type::Named { name: name(n) },
            Type::Interface { name: n, identity } => Type::Interface {
                name: name(n),
                identity: *identity,
            },
            Type::Newtype { name: n, repr } => Type::Newtype {
                name: name(n),
                repr: *repr,
            },
            Type::Generic { base, args } => Type::Generic {
                base: name(base),
                args: args.iter().map(|t| rename(t, names)).collect(),
            },
            Type::Union { members } => Type::Union {
                members: members.iter().map(|t| rename(t, names)).collect(),
            },
            Type::Option { inner } => Type::Option {
                inner: Box::new(rename(inner, names)),
            },
            Type::Array { elem } => Type::Array {
                elem: Box::new(rename(elem, names)),
            },
            Type::Function {
                params,
                ret,
                optional,
            } => Type::Function {
                params: params.iter().map(|t| rename(t, names)).collect(),
                ret: Box::new(rename(ret, names)),
                optional: *optional,
            },
            Type::Object { fields } => Type::Object {
                fields: fields.iter().map(|(n, t)| (*n, rename(t, names))).collect(),
            },
            other => other.clone(),
        }
    }
    if !has_exception(ty) {
        return ty.clone();
    }
    let names = specs
        .iter()
        .filter(|spec| {
            exports.structs.contains_key(spec.imported)
                || exports.enums.contains_key(spec.imported)
                || exports.newtypes.contains_key(spec.imported)
        })
        .map(|spec| (spec.imported, spec.local))
        .collect();
    rename(ty, &names)
}

impl<'a> Checker<'a> {
    pub(super) fn check_exception_use(
        &mut self,
        expr: &ast::Expr<'a>,
        usage: Use,
        expected: Option<Type<'a>>,
    ) -> Type<'a> {
        let saved_use = std::mem::replace(&mut self.exception_use, usage);
        let saved_expected = std::mem::replace(&mut self.exception_expected, expected);
        let ty = self.check_expr(expr);
        self.exception_use = saved_use;
        self.exception_expected = saved_expected;
        ty
    }

    pub(super) fn check_expr(&mut self, expr: &ast::Expr<'a>) -> Type<'a> {
        if matches!(expr, ast::Expr::Function { .. }) {
            self.index_flow.kill();
        }
        let ty = self.check_expr_erasure(expr);
        self.apply_index_effect(expr);
        self.validate_option_erasure(&ty, expr.span());
        if matches!(ty, Type::Option { .. }) {
            self.exception_forms.option_values.insert(expr as *const _);
        }
        if channels(&ty, "Result").is_some() {
            self.exception_forms.result_values.insert(expr as *const _);
        }
        ty
    }

    fn check_expr_erasure(&mut self, expr: &ast::Expr<'a>) -> Type<'a> {
        let usage = std::mem::take(&mut self.exception_use);
        let expected = self.exception_expected.take();
        let ptr = expr as *const ast::Expr<'a>;
        match expr {
            ast::Expr::Paren { expr: inner, .. } | ast::Expr::Safe { expr: inner, .. } => {
                let ty = self.check_exception_use(inner, usage, expected);
                if self
                    .exception_forms
                    .get(&(*inner as *const _))
                    .is_some_and(|form| {
                        matches!(form, ExceptionEmit::Match | ExceptionEmit::FromResult)
                    })
                {
                    self.exception_forms.insert(ptr, ExceptionEmit::Match);
                }
                return ty;
            }
            ast::Expr::EnumConstructor {
                enum_name,
                case_name,
                payload,
                span,
                shared_ok,
            } if *enum_name == "Exception"
                || (*shared_ok
                    && *case_name == "Ok"
                    && expected
                        .as_ref()
                        .is_some_and(|t| channels(t, "Exception").is_some())) =>
            {
                let Some(payload) = payload else {
                    self.error_span(*span, format!("`{case_name}` requires a payload"));
                    return Type::Error;
                };
                let payload_type = self.check_expr(payload);
                if *case_name == "Throw" {
                    if !matches!(usage, Use::Return | Use::Arm | Use::Statement) {
                        self.error_span(*span, "`Throw(e)` must be in escaping position; return it or raise it in a match arm");
                    }
                    self.exception_forms.insert(ptr, ExceptionEmit::Throw);
                    self.exception_escape(payload_type, *span);
                    return Type::Never;
                }
                if *case_name != "Ok" {
                    self.error_span(*span, format!("unknown Exception case `{case_name}`"));
                    return Type::Error;
                }
                if !matches!(usage, Use::Return | Use::Arm) {
                    self.error_span(
                        *span,
                        "Exception is control flow, not data; use Result to store a value",
                    );
                }
                self.exception_forms.insert(ptr, ExceptionEmit::Ok);
                return Type::Generic {
                    base: "Exception",
                    args: vec![payload_type, Type::Var],
                };
            }
            ast::Expr::Match {
                scrutinee,
                arms,
                span,
            } => {
                self.exception_expected = expected;
                let ty = self.check_match(scrutinee, arms, *span);
                // check_match marks the scrutinee at its consumed call site.
                if self
                    .exception_forms
                    .get(&(*scrutinee as *const _))
                    .is_some_and(|form| {
                        matches!(form, ExceptionEmit::Match | ExceptionEmit::FromResult)
                    })
                {
                    self.exception_forms.insert(ptr, ExceptionEmit::Match);
                }
                // A match discharges its source call, but its resulting
                // Exception still cannot be stored as data. Authored matches
                // are not exact-type tail delegation.
                return if matches!(usage, Use::Return | Use::Arm) {
                    ty
                } else {
                    self.consume_exception(expr, ty, usage)
                };
            }
            ast::Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                span,
            } => {
                let condition_type = self.check_expr(condition);
                self.expect_boolean(&condition_type, condition.span());
                let branch_use = if matches!(usage, Use::Return | Use::Arm) {
                    Use::Arm
                } else {
                    Use::Value
                };
                let saved_flow = self.index_flow.clone();
                self.assume_index_condition(condition);
                let left = self.check_exception_use(then_branch, branch_use, expected.clone());
                self.index_flow.restrict_to(&saved_flow);
                let right = self.check_exception_use(else_branch, branch_use, expected);
                self.index_flow.restrict_to(&saved_flow);
                return self.unify_ternary_arms(left, right, *span);
            }
            ast::Expr::Call {
                callee,
                args,
                type_args,
                span,
                ..
            } => {
                if let ast::Expr::FieldAccess { object, field, .. } = *callee {
                    if *field == "to_result" {
                        let ty = self.check_exception_use(object, Use::Convert, None);
                        if let Some((ok, error)) = channels(&ty, "Exception") {
                            if !args.is_empty() || !type_args.is_empty() {
                                self.error_span(*span, "`to_result()` takes no arguments");
                            }
                            self.exception_forms.insert(ptr, ExceptionEmit::ToResult);
                            return Type::Generic {
                                base: "Result",
                                args: vec![ok, error],
                            };
                        }
                        // Preserve user-defined methods with this spelling on
                        // ordinary data types; only Exception owns this builtin.
                        let ty = self.check_expr_inner(expr);
                        return self.consume_exception(expr, ty, usage);
                    }
                    if *field == "from"
                        && matches!(
                            *object,
                            ast::Expr::Identifier {
                                name: "Exception",
                                ..
                            }
                        )
                    {
                        if args.len() != 1 || !type_args.is_empty() {
                            self.error_span(
                                *span,
                                "`Exception.from(res)` requires exactly one Result",
                            );
                            return Type::Error;
                        }
                        let ty = self.check_expr(&args[0]);
                        if let Some((ok, error)) = channels(&ty, "Result") {
                            self.exception_forms.insert(ptr, ExceptionEmit::FromResult);
                            let ty = Type::Generic {
                                base: "Exception",
                                args: vec![ok, error],
                            };
                            return self.consume_exception(expr, ty, usage);
                        }
                        self.error_span(*span, "`Exception.from(res)` requires a Result");
                        return Type::Error;
                    }
                }
            }
            ast::Expr::Await { expr: inner, .. } => {
                let ty = self.check_exception_use(inner, Use::Await, None);
                let awaited = match ty {
                    Type::Generic {
                        base: "Promise",
                        args,
                    } if args.len() == 1 => args[0].clone(),
                    Type::Infer | Type::Error => Type::Infer,
                    other => {
                        self.error_at_expr(
                            expr,
                            format!("`await` expected Promise<T>, found type `{other}`"),
                        );
                        Type::Infer
                    }
                };
                // Preserve the existing await legality diagnostics.
                if !self.in_async_function && self.in_function {
                    self.error_at_expr(
                        expr,
                        "`await` is only allowed inside async functions or at the top level",
                    );
                }
                return self.consume_exception(expr, awaited, usage);
            }
            _ => {}
        }
        let ty = self.check_expr_inner(expr);
        self.consume_exception(expr, ty, usage)
    }

    fn consume_exception(&mut self, expr: &ast::Expr<'a>, ty: Type<'a>, usage: Use) -> Type<'a> {
        let Some((ok, error)) = channels(&ty, "Exception") else {
            return ty;
        };
        match usage {
            Use::Match => {
                self.exception_forms
                    .entry(expr as *const _)
                    .or_insert(ExceptionEmit::Match);
                ty
            }
            Use::Convert | Use::Await => ty,
            Use::Return => {
                if self.return_type.as_ref() != Some(&ty) {
                    self.error_at_expr(expr, "Exception tail delegation requires an EXACT return type match on both channels; use `match` for any type difference");
                }
                self.exception_escape(error, expr.span());
                ty
            }
            _ if !self.exception_catches.is_empty() => {
                self.exception_escape(error, expr.span());
                ok
            }
            _ => {
                self.error_at_expr(expr, "Exception-returning call must be consumed at the call site; match it where you call it, or convert to Result");
                Type::Error
            }
        }
    }

    fn exception_escape(&mut self, error: Type<'a>, span: ast::Span) {
        if let Some(caught) = self.exception_catches.last_mut() {
            caught.push(error);
            return;
        }
        if let Some((_, expected)) = self
            .return_type
            .as_ref()
            .and_then(|t| channels(t, "Exception"))
        {
            if !self.is_assignable(&expected, &error) {
                self.error_span(
                    span,
                    format!(
                        "escaping Throw has type `{error}`, expected exception channel `{expected}`"
                    ),
                );
            }
        } else {
            self.error_span(
                span,
                "escaping `Throw` forces this function's return type to `Exception<T, E>`",
            );
        }
    }

    pub(super) fn check_bodyless_arm(&mut self, arm: &ast::MatchArm<'a>, scrutinee: &Type<'a>) {
        let ast::Pattern::Constructor { name, .. } = &arm.pattern else {
            return;
        };
        let channel = match scrutinee {
            Type::Generic {
                base: base @ ("Result" | "Exception"),
                ..
            } => *base,
            _ => {
                self.error_span(
                    arm.span,
                    "bodyless arms are a privilege of Result and Exception only, never user enums",
                );
                return;
            }
        };
        let expected = self.return_type.clone();
        let expected_channel = expected.as_ref().and_then(|t| match t {
            Type::Generic { base, .. } => Some(*base),
            _ => None,
        });
        // Ok is shared and target-typed; both return channels carry it.
        // Err and Throw remain channel-specific and require authored conversion.
        if expected_channel != Some(channel)
            && !(*name == "Ok" && matches!(expected_channel, Some("Result" | "Exception")))
        {
            let help = if *name == "Err" && expected_channel == Some("Exception") {
                "help: raise it into the exception channel explicitly: `Err(e) => Throw(e)`"
            } else if *name == "Throw" && expected_channel == Some("Result") {
                "help: move it into the result channel explicitly: `Throw(e) => Err(e)`"
            } else {
                "the enclosing return type must carry the bodyless variant"
            };
            self.error_span(
                arm.span,
                format!("bodyless `{name}` cannot propagate across channels; {help}"),
            );
            return;
        }
        // Refutable and wildcard payload patterns still pass the original
        // payload through. The parser reserves a non-source identifier for it.
        if let ast::Expr::EnumConstructor {
            payload: Some(ast::Expr::Identifier { name: binding, .. }),
            ..
        } = &arm.body
        {
            if binding.starts_with("$__deka_passthrough_") {
                if let Some((ok, error)) = channels(scrutinee, channel) {
                    let mut payload_type = if *name == "Ok" { ok } else { error };
                    if let ast::Pattern::Constructor {
                        payload: Some(pattern),
                        ..
                    } = &arm.pattern
                    {
                        if self
                            .union_type_patterns
                            .contains_key(&(*pattern as *const _))
                        {
                            if let (
                                Type::Union { members },
                                ast::Pattern::Constructor { name, .. },
                            ) = (&payload_type, *pattern)
                            {
                                if let Some(member) = members.iter().find(|member| matches!(member, Type::Named { name: n } | Type::Struct { name: n } | Type::Interface { name: n, .. } | Type::Newtype { name: n, .. } if n == name)) {
                                    payload_type = member.clone();
                                }
                            }
                        }
                    }
                    self.declare_var(binding, payload_type);
                }
            }
        }
        // Ok is shared, so target the enclosing return channel explicitly.
        let target = expected.clone();
        let ty = self.check_exception_use(&arm.body, Use::Return, target);
        if let Some(expected) = expected {
            if !self.is_assignable(&expected, &ty) {
                self.error_span(arm.span, format!("bodyless `{name}` has type `{ty}`, incompatible with enclosing return type `{expected}`"));
            }
        }
    }

    pub(super) fn check_try(
        &mut self,
        body: &'a [ast::Stmt<'a>],
        name: &'a str,
        annotation: Option<&ast::Type<'a>>,
        catch_body: &'a [ast::Stmt<'a>],
        span: ast::Span,
    ) {
        self.exception_catches.push(Vec::new());
        self.scopes.push(HashMap::new());
        self.mutables.push(HashSet::new());
        for s in body {
            self.check_statement(s);
        }
        self.pop_value_scope();
        self.mutables.pop();
        let errors = self.exception_catches.pop().unwrap();
        let catch_type = if let Some(annotation) = annotation {
            let ty = self.resolve_ast_type(annotation);
            let constructor = match ty {
                Type::Struct { name } => Some(name),
                Type::Named {
                    name: "SyntaxError",
                } => Some("SyntaxError"),
                Type::Named { name: "TypeError" } => Some("TypeError"),
                Type::Named { name: "RangeError" } => Some("RangeError"),
                Type::Named {
                    name: "Error" | "JsError",
                } => Some("Error"),
                _ => None,
            };
            if let Some(constructor) = constructor {
                self.exception_forms
                    .catches
                    .insert(annotation as *const _, constructor);
            } else {
                self.error_span(span, "typed catch requires a concrete runtime constructor (a struct or JavaScript error class) for its instanceof guard");
            }
            for error in &errors {
                let members = match error {
                    Type::Union { members } => members.clone(),
                    other => vec![other.clone()],
                };
                for member in members {
                    if !self.catch_covers(&ty, &member) {
                        self.exception_escape(member, span);
                    }
                }
            }
            ty
        } else {
            let mut unique = Vec::new();
            for error in errors {
                if !unique.contains(&error) {
                    unique.push(error);
                }
            }
            match unique.len() {
                0 => Type::Never,
                1 => unique.remove(0),
                _ => Type::Union { members: unique },
            }
        };
        self.scopes.push(HashMap::new());
        self.mutables.push(HashSet::new());
        self.declare_var(name, catch_type);
        for s in catch_body {
            self.check_statement(s);
        }
        self.pop_value_scope();
        self.mutables.pop();
    }

    fn catch_covers(&self, catch: &Type<'a>, error: &Type<'a>) -> bool {
        if let Type::Union { members } = error {
            return members.iter().all(|m| self.catch_covers(catch, m));
        }
        if matches!(catch, Type::Named { name: "Error" })
            && matches!(
                error,
                Type::Named {
                    name: "SyntaxError" | "TypeError" | "RangeError" | "Error"
                }
            )
        {
            return true;
        }
        // JsError can contain arbitrary thrown JS values; even Error does not cover it.
        catch == error && !matches!(error, Type::Named { name: "JsError" })
    }
}

#[cfg(test)]
mod tests {
    fn check(source: &str) -> Vec<String> {
        let arena = bumpalo::Bump::new();
        let parsed = crate::parse(source, &arena);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let program = parsed.program.unwrap();
        super::super::check_program(&program, source)
            .errors
            .iter()
            .map(|e| e.message.clone())
            .collect()
    }

    #[test]
    fn core_and_frame_local_handling() {
        for source in [
            "fn f() Exception<number, string> { return Ok(1); } fn g() Exception<number, string> { return f(); }",
            "fn f() Exception<number, string> { return Throw(\"bad\"); } fn g() number { return match f() { Ok(v) => v, Throw(e) => 0 }; }",
            "fn f() number { try { return Throw(\"bad\"); } catch (e) { return e.length; } }",
            "fn f() Exception<number, string> { return match Result.Ok(1) { Ok(v), Err(e) => Throw(e) }; }",
            "fn f() Result<number, string> { const v = match Ok(1) { Ok(v) => v, Err(e) }; return Ok(v); }",
            "fn f() Exception<number, string> { return match Exception.from(Err(\"bad\")) { Ok(v) => Ok(1), Throw(e) }; }",
            "fn f() Exception<number, string> { return Ok(1); } fn g() number { try { const x = f(); return x + 1; } catch (e) { return 0; } }",
            "fn f() Exception<number, string> { return Ok(1); } const r: Result<number, string> = f().to_result();",
            "async fn f() Promise<Exception<number, string>> { return Throw(\"bad\"); } async fn g() Promise<number> { return match await f() { Ok(v) => v, Throw(e) => 0 }; }",
        ] {
            assert!(check(source).is_empty(), "{source}\n{:?}", check(source));
        }
    }

    #[test]
    fn every_rule_has_a_teaching_diagnostic() {
        for (source, diagnostic) in [
            (
                "struct Thing {} fn (t Thing) to_result() Exception<number, string> { return Ok(1); } const t = Thing {}; const x = t.to_result();",
                "match it where you call it, or convert to Result",
            ),
            (
                "fn f() Exception<number, string> { return Ok(1); } const x = match f() { Ok(v) => Exception.Ok(v), Throw(e) => Exception.Ok(0) };",
                "match it where you call it, or convert to Result",
            ),
            (
                "fn f() number { return Throw(\"bad\"); }",
                "escaping `Throw` forces",
            ),
            (
                "fn f() Exception<number, string> { return Ok(1); } const x = f();",
                "match it where you call it, or convert to Result",
            ),
            (
                "fn f() Exception<number, string> { return Ok(1); } fn g() Exception<number, string | number> { return f(); }",
                "EXACT return type match",
            ),
            (
                "fn f() Exception<number, string> { return match Err(\"x\") { Ok(v) => Ok(1), Err(e) }; }",
                "help: raise it into the exception channel explicitly: `Err(e) => Throw(e)`",
            ),
            (
                "fn f() Exception<number, string> { return Ok(1); } fn g() Result<number, string> { return match f() { Ok(v) => Ok(v), Throw(e) }; }",
                "help: move it into the result channel explicitly: `Throw(e) => Err(e)`",
            ),
            (
                "enum User { Value(number) } fn f() User { return match User.Value(1) { User.Value(v) }; }",
                "privilege of Result and Exception only",
            ),
            (
                "fn f() Exception<number, string> { return match Exception.Ok(1) { Ok(v) => Ok(v) }; }",
                "non-exhaustive match",
            ),
            (
                "fn f(e: SyntaxError | TypeError) number { try { return Throw(e); } catch (e: SyntaxError) { return 0; } }",
                "escaping `Throw` forces",
            ),
            (
                "fn f() number { try { const cb = fn() number { return Throw(\"bad\"); }; return cb(); } catch (e) { return 0; } }",
                "escaping `Throw` forces",
            ),
        ] {
            let errors = check(source);
            assert!(
                errors.iter().any(|e| e.contains(diagnostic)),
                "{source}\nexpected {diagnostic:?}\ngot {errors:?}"
            );
        }
    }
}
