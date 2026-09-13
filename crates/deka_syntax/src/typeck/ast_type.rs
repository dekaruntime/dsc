//! AST type -> typechecker type resolution.

use std::collections::HashSet;

use crate::ast;
use crate::diagnostics::Diagnostic;

use super::Checker;
use super::types::Type;

impl<'a> Checker<'a> {
    pub(super) fn checked_option(&mut self, inner: Type<'a>, span: ast::Span) -> Type<'a> {
        match &inner {
            Type::Named { name: "void" } => self.error_span(span, "`Option<void>` is indistinguishable under erasure: Some(noop()) and None both become undefined; it is semantically a bool — use `boolean` or a purpose-built enum"),
            Type::Option { .. } | Type::None => self.error_span(span, "`Option<Option<T>>` is ambiguous under erasure: Some(None) and None both become undefined; model the distinct states with a purpose-built enum"),
            _ => {}
        }
        Type::Option { inner: Box::new(inner) }
    }

    /// Instantiation may put an unrepresentable Option below a container.
    pub(super) fn validate_option_erasure(&mut self, ty: &Type<'a>, span: ast::Span) {
        match ty {
            Type::Option { inner } => {
                self.checked_option((**inner).clone(), span);
                self.validate_option_erasure(inner, span);
            }
            Type::Array { elem } => self.validate_option_erasure(elem, span),
            Type::Generic { args, .. } | Type::Union { members: args } => {
                for arg in args { self.validate_option_erasure(arg, span); }
            }
            Type::Function { params, ret, .. } => {
                for param in params { self.validate_option_erasure(param, span); }
                self.validate_option_erasure(ret, span);
            }
            Type::Object { fields } => {
                for (_, ty) in fields { self.validate_option_erasure(ty, span); }
            }
            _ => {}
        }
    }

    pub(super) fn resolve_ast_type(&mut self, ty: &ast::Type<'a>) -> Type<'a> {
        self.resolve_ast_type_rec(ty, &mut HashSet::new())
    }

    fn resolve_ast_type_rec(
        &mut self,
        ty: &ast::Type<'a>,
        seen: &mut HashSet<&'a str>,
    ) -> Type<'a> {
        match ty {
            ast::Type::Named { name, span } => match *name {
                "number" | "string" | "boolean" | "never" | "void" | "bytes" | "Component"
                | "JsError" | "SyntaxError" | "TypeError" | "RangeError" | "Error" | "Type" => {
                    Type::Named { name }
                }
                "Option" => {
                    self.error_span(
                        *span,
                        "Option requires a type argument, e.g. Option<number>",
                    );
                    Type::Error
                }
                _ => {
                    if let Some(param) = self.lookup_type_param(name) {
                        return param;
                    }
                    if let Some(ty) = self.opaques.get(name) {
                        ty.clone()
                    } else if self.structs.contains_key(name) {
                        Type::Struct { name }
                    } else if self.enums.contains_key(name) {
                        Type::Named { name }
                    } else if let Some(info) = self.interfaces.get(name) {
                        Type::Interface {
                            name,
                            identity: info.members.as_ptr() as usize,
                        }
                    } else if let Some(info) = self.newtypes.get(name).cloned() {
                        Type::Newtype { name, repr: info.repr }
                    } else if let Some(alias) = self.aliases.get(name).cloned() {
                        if !seen.insert(name) {
                            self.error_span(*span, format!("cyclic type alias `{name}`"));
                            return Type::Error;
                        }
                        let resolved = self.resolve_ast_type_rec(&alias, seen);
                        seen.remove(name);
                        resolved
                    } else if let Some(kind) = self.build_factory_kinds.get(name).copied() {
                        // A factory private to an imported module, named here
                        // only through its descriptor fragment (dsc#52). It
                        // has no local declaration to check against, so the
                        // kind from the declaring module stands in.
                        kind.to_type(name)
                    } else {
                        self.error_span(*span, format!("unknown type `{name}`"));
                        Type::Error
                    }
                }
            },

            ast::Type::Generic { base, args, span } => {
                if base == &"Option" {
                    if args.len() == 1 {
                        let inner = self.resolve_ast_type_rec(&args[0], seen);
                        self.checked_option(inner, *span)
                    } else {
                        self.error_span(*span, "Option requires exactly one type argument");
                        Type::Error
                    }
                } else if base == &"Promise" {
                    if args.len() == 1 {
                        Type::Generic {
                            base: "Promise",
                            args: vec![self.resolve_ast_type_rec(&args[0], seen)],
                        }
                    } else {
                        self.error_span(*span, "Promise requires exactly one type argument");
                        Type::Error
                    }
                } else if base == &"Array" {
                    if args.len() == 1 {
                        Type::Array {
                            elem: Box::new(self.resolve_ast_type_rec(&args[0], seen)),
                        }
                    } else {
                        self.error_span(*span, "Array requires exactly one type argument");
                        Type::Error
                    }
                } else if base == &"Result" || base == &"Exception" {
                    if args.len() == 2 {
                        Type::Generic {
                            base,
                            args: args
                                .iter()
                                .map(|arg| self.resolve_ast_type_rec(arg, seen))
                                .collect(),
                        }
                    } else {
                        self.error_span(
                            *span,
                            format!("{base} requires exactly two type arguments"),
                        );
                        Type::Error
                    }
                } else if self.enums.contains_key(base) || self.structs.contains_key(base) {
                    Type::Generic {
                        base,
                        args: args
                            .iter()
                            .map(|arg| self.resolve_ast_type_rec(arg, seen))
                            .collect(),
                    }
                } else {
                    self.error_span(*span, format!("unsupported generic type `{base}`"));
                    Type::Error
                }
            }

            ast::Type::Function { params, ret, .. } => Type::Function {
                params: params
                    .iter()
                    .map(|p| self.resolve_ast_type_rec(p, seen))
                    .collect(),
                ret: Box::new(self.resolve_ast_type_rec(ret, seen)),
                optional: 0,
            },

            ast::Type::Option { inner, span } => {
                let inner = self.resolve_ast_type_rec(inner, seen);
                self.checked_option(inner, *span)
            },

            // Membership and overlap validation live in `check_union_members`
            // (rfd#42, deka#530); members resolve individually so aliases and
            // type params keep working through them.
            ast::Type::Union { members, span } => {
                let resolved: Vec<Type<'a>> = members
                    .iter()
                    .map(|member| self.resolve_ast_type_rec(member, seen))
                    .collect();
                self.check_union_members(&resolved, *span);
                Type::Union { members: resolved }
            }

            ast::Type::Tuple { span, .. } | ast::Type::Record { span, .. } => {
                self.error_span(*span, "tuple/record types are not supported in v2 typeck");
                Type::Error
            }
        }
    }

    /// Validate union membership and member overlap (rfd#42, deka#530).
    ///
    /// Membership rule: a type may appear in a union only if it has a
    /// decidable runtime predicate — primitives (string/number/boolean/
    /// bytes/void), named structs, enums and interfaces. Function types,
    /// unconstrained type params (`Var`), checker-unknown types (`Infer`),
    /// and `Option`/`Generic`/`Array`/`Object` are rejected: there is no
    /// runtime test the emitter could emit for them today. Newtypes are
    /// rejected in v1 even though they carry a `__deka_newtype` runtime tag:
    /// the tag-based predicate is not wired into match emission yet, and
    /// allowing them now would commit to a shape before it exists.
    /// `Type::Error` propagates silently (error recovery).
    ///
    /// Overlap rule: two members may not both match a single value, or
    /// narrowing is ambiguous. Distinct primitives never overlap; identical
    /// members do; struct-vs-struct and interface-vs-interface overlap iff
    /// either direction is assignable; anything vs a primitive never
    /// overlaps; enum-vs-enum overlaps iff it is the same enum.
    fn check_union_members(&mut self, members: &[Type<'a>], span: ast::Span) {
        for member in members {
            if member.is_error() {
                continue;
            }
            let allowed = match member {
                Type::Named { name } => {
                    Self::is_union_primitive(name)
                        || matches!(*name, "SyntaxError" | "TypeError" | "RangeError" | "Error")
                        || self.enums.contains_key(name)
                }
                Type::Struct { .. } | Type::Interface { .. } => true,
                _ => false,
            };
            if !allowed {
                self.error_span(
                    span,
                    format!(
                        "union member `{member}` is not allowed: union members must have a \
                         decidable runtime predicate — only primitives, structs, enums and \
                         interfaces may appear in a union (rfd#42)"
                    ),
                );
            }
        }

        for (i, a) in members.iter().enumerate() {
            for b in members.iter().skip(i + 1) {
                if a.is_error() || b.is_error() {
                    continue;
                }
                if self.union_members_overlap(a, b) {
                    self.error_span(
                        span,
                        format!(
                            "union members `{a}` and `{b}` overlap — a value matching `{b}` \
                             also matches `{a}`, so narrowing would be ambiguous"
                        ),
                    );
                }
            }
        }
    }

    fn is_union_primitive(name: &str) -> bool {
        matches!(name, "string" | "number" | "boolean" | "bytes" | "void")
    }

    /// Conservative v1 overlap test. Assumes membership validation has
    /// already run, so only allowed member shapes reach this.
    fn union_members_overlap(&mut self, a: &Type<'a>, b: &Type<'a>) -> bool {
        match (a, b) {
            // Identical members always overlap (`string | string`).
            _ if a == b => true,
            (
                Type::Named { name: "Error" },
                Type::Named {
                    name: "SyntaxError" | "TypeError" | "RangeError",
                },
            )
            | (
                Type::Named {
                    name: "SyntaxError" | "TypeError" | "RangeError",
                },
                Type::Named { name: "Error" },
            ) => true,
            (Type::Named { .. }, Type::Named { .. }) => {
                // Distinct primitives never overlap; enum-vs-enum overlaps
                // only for the same enum, which equality above caught.
                false
            }
            (Type::Struct { .. }, Type::Struct { .. })
            | (Type::Interface { .. }, Type::Interface { .. })
            | (Type::Struct { .. }, Type::Interface { .. })
            | (Type::Interface { .. }, Type::Struct { .. }) => {
                self.is_assignable(a, b) || self.is_assignable(b, a)
            }
            // A primitive (or enum) value never matches a struct/interface.
            _ => false,
        }
    }

    pub(super) fn error_span(&mut self, span: ast::Span, message: impl Into<String>) {
        if self.infer_only {
            return;
        }
        self.errors.push(Diagnostic::error(
            span.start.line,
            span.start.column,
            message,
        ));
    }

    pub(super) fn error_span_with_underline(
        &mut self,
        span: ast::Span,
        underline_length: usize,
        message: impl Into<String>,
    ) {
        if self.infer_only {
            return;
        }
        self.errors.push(
            Diagnostic::error(span.start.line, span.start.column, message)
                .with_underline(underline_length),
        );
    }

    pub(super) fn lookup_type_param(&self, name: &'a str) -> Option<Type<'a>> {
        for scope in self.type_scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty.clone());
            }
        }
        None
    }
}
