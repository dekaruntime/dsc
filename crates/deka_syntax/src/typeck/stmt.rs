//! Statement typechecking.

use super::expr::Coverage;
use std::collections::{HashMap, HashSet};

use crate::ast;

use super::Checker;
use super::types::Type;

/// Receiver type parameters — `(name, declared bound)` — with the struct's
/// declared bounds inherited at positions the receiver leaves unbounded:
/// with `struct Holder<T: Named> { value: T }`, `fn (x Holder<T>) ...` still
/// sees `T: Named` in the body — the bound is a property of the parameter,
/// not of one spelling of it (rfd#56 phase 2). Bound references keep the
/// `'a` lifetime so they can be resolved through `resolve_bound`.
fn effective_receiver_params<'a>(
    struct_params: &'a [ast::TypeParam<'a>],
    receiver_args: &'a [ast::TypeParam<'a>],
) -> Vec<(&'a str, Option<&'a ast::Type<'a>>)> {
    receiver_args
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let inherited = struct_params.get(i).and_then(|s| s.bound.as_ref());
            (p.name, p.bound.as_ref().or(inherited))
        })
        .collect()
}

impl<'a> Checker<'a> {
    /// Push a type-parameter scope from `(name, bound)` pairs — the shape
    /// produced by [`effective_receiver_params`], where the bound may be
    /// inherited from the struct declaration rather than a `TypeParam` node.
    pub(super) fn push_receiver_params(
        &mut self,
        params: &[(&'a str, Option<&'a ast::Type<'a>>)],
    ) {
        let mut scope = HashMap::new();
        let mut bounds = HashMap::new();
        for &(name, bound) in params {
            scope.insert(name, Type::Param { name });
            if let Some(bound) = bound {
                let resolved = self.resolve_bound(bound);
                bounds.insert(name, resolved);
            }
        }
        self.type_scopes.push(scope);
        self.param_bounds.push(bounds);
    }

    /// Build a descriptor fragment for every describable declared factory.
    /// See [`super::build_module_build_fragments`] for the contract.
    pub(super) fn build_declared_build_fragments(
        &mut self,
    ) -> HashMap<&'a str, super::DescriptorTree<'a>> {
        self.collect_declarations();
        let mut declared: Vec<Type<'a>> = Vec::new();
        for stmt in self.program.statements {
            match stmt {
                ast::Stmt::Struct {
                    name, type_params, ..
                } if type_params.is_empty() => {
                    declared.push(Type::Struct { name: *name });
                }
                ast::Stmt::Enum {
                    name, type_params, ..
                } if type_params.is_empty() => {
                    declared.push(Type::Named { name: *name });
                }
                ast::Stmt::Newtype { name, repr, .. } => {
                    declared.push(Type::Newtype {
                        name: *name,
                        repr: *repr,
                    });
                }
                _ => {}
            }
        }
        let span = ast::Span::dummy();
        let mut fragments = HashMap::new();
        for ty in declared {
            let name = match &ty {
                Type::Struct { name } | Type::Named { name } | Type::Newtype { name, .. } => *name,
                _ => continue,
            };
            if let Ok(tree) = self.descriptor_tree(&ty, span) {
                fragments.insert(name, tree);
            }
        }
        fragments
    }

    pub(super) fn check_program(&mut self) {
        self.collect_declarations();
        self.collect_module_value_bindings();
        self.collect_function_signatures();
        self.collect_summon_signatures();
        self.collect_receiver_methods();
        self.check_embedded_method_ambiguity();
        self.validate_interface_declarations();

        // Infer return types for unannotated functions before emitting
        // diagnostics. This resolves forward references within a module (e.g.
        // `sha256` calling `digest` in @deka/crypto).
        self.infer_function_return_types();

        // Check function and receiver-method bodies first so that inferred
        // return types are available to later top-level statements.
        for stmt in self.program.statements {
            match stmt {
                ast::Stmt::Function {
                    name,
                    type_params,
                    params,
                    return_type,
                    body,
                    span,
                    is_async,
                    ..
                } => self.check_function(
                    name,
                    type_params,
                    params,
                    return_type.as_ref(),
                    body,
                    *is_async,
                    *span,
                ),
                ast::Stmt::ReceiverMethod {
                    receiver_type,
                    receiver_type_args,
                    receiver_name,
                    receiver_mutable,
                    name,
                    type_params,
                    params,
                    return_type,
                    body,
                    span,
                    is_async,
                    ..
                } => self.check_receiver_method(
                    receiver_type,
                    receiver_type_args,
                    receiver_name,
                    *receiver_mutable,
                    name,
                    type_params,
                    params,
                    return_type.as_ref(),
                    body,
                    *is_async,
                    *span,
                ),
                ast::Stmt::Export {
                    decl: ast::ExportDecl::Function { .. },
                    ..
                } => {
                    // Handled in the same shape as top-level functions below.
                    self.check_export_function(stmt);
                }
                _ => {}
            }
        }

        // Now check non-function top-level statements in source order.
        for stmt in self.program.statements {
            match stmt {
                ast::Stmt::Function { .. } | ast::Stmt::ReceiverMethod { .. } => {}
                ast::Stmt::Export {
                    decl: ast::ExportDecl::Function { .. },
                    ..
                } => {}
                _ => self.check_statement(stmt),
            }
        }
    }

    fn collect_declarations(&mut self) {
        for stmt in self.program.statements {
            if let ast::Stmt::Opaque { name, span } = stmt {
                if self
                    .opaques
                    .insert(
                        name,
                        Type::Opaque {
                            name,
                            identity: stmt as *const _ as usize,
                        },
                    )
                    .is_some()
                {
                    self.error_span(*span, format!("duplicate opaque type `{name}`"));
                }
            }
        }
        // `Type` is the builtin first-class type descriptor (rfd#41,
        // deka#529). Reserving the name keeps user declarations from
        // colliding with the builtin in annotations; restriction-first,
        // relaxable later.
        const BUILTIN_TYPE_DIAGNOSTIC: &str = "`Type` is a builtin type";
        for stmt in self.program.statements {
            if let ast::Stmt::TypeAlias {
                name, value, span, ..
            } = stmt
            {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if matches!(*name, "Setter" | "Ref") {
                    self.error_span(*span, format!("`{name}` is a builtin type"));
                }
                if self.aliases.insert(name, value.clone()).is_some() {
                    self.error_span(*span, format!("duplicate type alias `{name}`"));
                }
            }
            if let ast::Stmt::Enum {
                name,
                cases,
                type_params,
                is_super,
                span,
            } = stmt
            {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self
                    .enums
                    .insert(
                        name,
                        super::EnumInfo {
                            cases,
                            type_params,
                            is_super: *is_super,
                        },
                    )
                    .is_some()
                {
                    self.error_span(*span, format!("duplicate enum definition `{name}`"));
                    continue;
                }
                for case in cases.iter() {
                    if self.case_to_enum.insert(case.name, name).is_some() {
                        self.error_span(
                            case.span,
                            format!("duplicate enum case name `{}`", case.name),
                        );
                    }
                }
            }
            if let ast::Stmt::Struct {
                name,
                fields,
                embeds,
                type_params,
                is_super,
                span,
                ..
            } = stmt
            {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self
                    .structs
                    .insert(
                        name,
                        super::StructInfo {
                            fields,
                            embeds,
                            type_params,
                            is_super: *is_super,
                        },
                    )
                    .is_some()
                {
                    self.error_span(*span, format!("duplicate struct definition `{name}`"));
                    continue;
                }
                // Make the struct factory available as a value so it can be
                // referenced before its definition (hoisting) and invoked with
                // factory-call syntax `Person({ ... })`.
                self.declare_var(name, Type::Struct { name });

                let mut seen_fields = HashSet::new();
                for field in fields.iter() {
                    if !seen_fields.insert(field.name) {
                        self.error_span(
                            field.span,
                            format!("Duplicate field '{}' in struct '{}'.", field.name, name),
                        );
                    }
                }

                let mut seen_embeds = HashSet::new();
                for embed in embeds.iter() {
                    if embed.name == *name {
                        self.error_span(
                            embed.span,
                            format!("Struct cannot embed itself (`{name}`)"),
                        );
                        continue;
                    }
                    if !seen_embeds.insert(embed.name) {
                        self.error_span(
                            embed.span,
                            format!("Duplicate embedded struct '{}'", embed.name),
                        );
                    }
                }
            }
            if let ast::Stmt::Interface {
                name,
                type_params,
                members,
                span,
                ..
            } = stmt
            {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self
                    .interfaces
                    .insert(
                        name,
                        super::InterfaceInfo {
                            members,
                            type_params,
                            span: *span,
                        },
                    )
                    .is_some()
                {
                    self.error_span(*span, format!("duplicate interface definition `{name}`"));
                }
            }
            if let ast::Stmt::Newtype { name, repr, span } = stmt {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self
                    .newtypes
                    .insert(name, super::NewtypeInfo { repr: *repr })
                    .is_some()
                {
                    self.error_span(*span, format!("duplicate newtype definition `{name}`"));
                    continue;
                }
                if self.aliases.contains_key(name)
                    || self.structs.contains_key(name)
                    || self.enums.contains_key(name)
                    || self.interfaces.contains_key(name)
                {
                    self.error_span(
                        *span,
                        format!("`{name}` conflicts with an existing type declaration"),
                    );
                }
            }
        }

        // Super declarations (rfd#41, deka#561 PR B): after every type
        // declaration is collected, compute the transitive super marking and
        // build/validate descriptor trees so both this pass and later
        // `Name.type()` call sites see them.
        for stmt in self.program.statements {
            if let ast::Stmt::Opaque { name, span } = stmt {
                if self.structs.contains_key(name)
                    || self.enums.contains_key(name)
                    || self.aliases.contains_key(name)
                    || self.interfaces.contains_key(name)
                    || self.newtypes.contains_key(name)
                    || matches!(
                        *name,
                        "number"
                            | "string"
                            | "boolean"
                            | "void"
                            | "Option"
                            | "Result"
                            | "Exception"
                            | "JsValue"
                            | "JsError"
                            | "Error"
                            | "TypeError"
                            | "SyntaxError"
                            | "RangeError"
                            | "bytes"
                            | "never"
                            | "Component"
                            | "ReactNode"
                            | "Promise"
                            | "Type"
                            | "Setter"
                            | "Ref"
                    )
                {
                    self.error_span(
                        *span,
                        format!("opaque type `{name}` conflicts with an existing or reserved type"),
                    );
                }
            }
        }
        self.collect_super_declarations();
    }

    /// Eagerly resolve interface member types so that unknown types and other
    /// annotation errors are reported even when the interface is not used.
    fn validate_interface_declarations(&mut self) {
        let interfaces: Vec<_> = self
            .interfaces
            .iter()
            .map(|(n, i)| (*n, i.clone()))
            .collect();
        for (name, info) in interfaces {
            if info.members.is_empty() {
                self.error_span(
                    info.span,
                    format!("interface `{name}` must declare at least one member"),
                );
                continue;
            }
            // The interface's own type parameters are in scope for member
            // validation (rfd#56 phase 1): `interface Container<T> { value: T }`
            // resolves `value` to `T` here. Use-site instantiation of a
            // generic interface is not part of phase 1.
            self.push_type_params(info.type_params);
            for member in info.members.iter() {
                match member {
                    ast::InterfaceMember::Field { ty, span, .. } => {
                        let resolved = self.resolve_ast_type(ty);
                        if resolved.is_error() {
                            // Error already reported by resolve_ast_type.
                            continue;
                        }
                        if matches!(resolved, Type::Function { .. }) {
                            self.error_span(
                                *span,
                                "interface fields may not have function types; use a method instead",
                            );
                        }
                    }
                    ast::InterfaceMember::Method {
                        params,
                        return_type,
                        span,
                        ..
                    } => {
                        for param in params.iter() {
                            if let Some(ty) = param.ty.as_ref() {
                                self.resolve_ast_type(ty);
                            }
                        }
                        if let Some(ty) = return_type.as_ref() {
                            self.resolve_ast_type(ty);
                        }
                    }
                }
            }
            self.pop_type_params();
        }
    }

    /// Push a type-parameter scope, resolving any declared bounds (rfd#56
    /// phase 2). Even a parameterless declaration pushes (empty) scopes:
    /// `pop_type_params` always pops, and the early return this replaced
    /// silently dropped an unrelated outer scope at every empty push/pop
    /// pair — e.g. a `match` on a non-generic enum inside a generic
    /// function popped the function's `<T>` scope mid-body.
    pub(super) fn push_type_params(&mut self, type_params: &'a [ast::TypeParam<'a>]) {
        let mut scope = HashMap::new();
        let mut bounds = HashMap::new();
        for param in type_params {
            scope.insert(param.name, Type::Param { name: param.name });
            if let Some(bound) = &param.bound {
                let resolved = self.resolve_bound(bound);
                bounds.insert(param.name, resolved);
            }
        }
        self.type_scopes.push(scope);
        self.param_bounds.push(bounds);
    }

    /// Resolve a type-parameter bound, caching the result per AST node.
    /// The silent inference pass resolves first; caching successful results
    /// keeps the real pass from re-reporting, while an unresolvable bound
    /// stays uncached there so its error surfaces exactly once, later.
    pub(super) fn resolve_bound(&mut self, bound: &'a ast::Type<'a>) -> Type<'a> {
        let key = bound as *const ast::Type<'a>;
        if let Some(resolved) = self.bound_cache.get(&key) {
            return resolved.clone();
        }
        let resolved = self.resolve_ast_type(bound);
        if !resolved.is_error() || !self.infer_only {
            self.bound_cache.insert(key, resolved.clone());
        }
        resolved
    }

    pub(super) fn pop_type_params(&mut self) {
        self.type_scopes.pop();
        self.param_bounds.pop();
    }

    /// The declared bound of a type parameter, innermost scope first
    /// (rfd#56 phase 2). `None` means the parameter is unbounded.
    pub(super) fn lookup_param_bound(&self, name: &str) -> Option<Type<'a>> {
        for scope in self.param_bounds.iter().rev() {
            if let Some(bound) = scope.get(name) {
                return Some(bound.clone());
            }
        }
        None
    }

    /// The type operations on a value should dispatch against: for a
    /// bounded type parameter, its bound — the bound is what unlocks
    /// operations (rfd#56 phase 2). Anything else dispatches on itself.
    pub(super) fn bounded_param_type(&self, ty: &Type<'a>) -> Type<'a> {
        match ty {
            Type::Param { name } => self.lookup_param_bound(name).unwrap_or_else(|| ty.clone()),
            _ => ty.clone(),
        }
    }

    fn collect_receiver_methods(&mut self) {
        for stmt in self.program.statements {
            if let ast::Stmt::ReceiverMethod {
                receiver_type,
                receiver_type_args,
                receiver_mutable,
                name,
                type_params,
                params,
                return_type,
                span,
                ..
            } = stmt
            {
                if !self.structs.contains_key(receiver_type)
                    && !self.newtypes.contains_key(receiver_type)
                    && !self.opaques.contains_key(receiver_type)
                    && !super::is_primitive_receiver_name(receiver_type)
                {
                    self.error_span(*span, format!("unknown receiver type `{receiver_type}`"));
                    continue;
                }
                // rfd#56 / dsc#101: a receiver method on a generic type binds
                // its type parameters in the receiver position. The receiver
                // must actually be generic, the arity must match the struct's
                // declaration, and the parameters may not also be declared on
                // the method — `fn (s Signal) get<T>()` would read as if the
                // method could pick a `T` unrelated to the receiver's.
                let struct_params: &[ast::TypeParam<'a>] = self
                    .structs
                    .get(receiver_type)
                    .map_or(&[], |info| info.type_params);
                if !receiver_type_args.is_empty() {
                    if struct_params.is_empty() {
                        self.error_span(
                            *span,
                            format!(
                                "receiver type `{receiver_type}` has no type parameters to bind"
                            ),
                        );
                        continue;
                    }
                    if struct_params.len() != receiver_type_args.len() {
                        self.error_span(
                            *span,
                            format!(
                                "receiver type `{receiver_type}` declares {} type parameter{}, \
                                 found {}",
                                struct_params.len(),
                                if struct_params.len() == 1 { "" } else { "s" },
                                receiver_type_args.len()
                            ),
                        );
                        continue;
                    }
                    if !type_params.is_empty() {
                        self.error_span(
                            *span,
                            format!(
                                "type parameters on receiver method `{name}` are bound by the \
                                 receiver, not the method — declare them on the receiver \
                                 (`fn (s {receiver_type}<T>) {name}(...)`) and remove `<...>` \
                                 from the method name (rfd#56)"
                            ),
                        );
                        continue;
                    }
                }
                // A builtin property keeps its meaning for property-shaped
                // access, so an extension of the same name would give one name
                // two silent meanings (`s.length` vs `s.length()`). Builtin
                // *methods* may be shadowed: call-shaped access has a single
                // meaning either way (deka#527).
                if super::is_primitive_receiver_name(receiver_type)
                    && matches!(
                        super::expr::primitive_member(receiver_type, name, None),
                        Some(super::expr::PrimitiveMember::Property(_))
                    )
                {
                    self.error_span(
                        *span,
                        format!(
                            "cannot declare extension `{name}` on `{receiver_type}`: `{name}` is a builtin property"
                        ),
                    );
                    continue;
                }
                let key = (*receiver_type, *name);
                // Resolve annotations once, here, so body checking and call
                // sites reuse them instead of re-reporting (deka#494).
                // Receiver-bound parameters (`fn (s Signal<T>)`) are in scope
                // so `T` in the signature resolves to the receiver's type
                // parameter (rfd#56, dsc#101); the phase-1 spelling
                // (`fn (s Signal) set<T>(...)`) resolves through the method's
                // own parameters, as before.
                let effective = effective_receiver_params(struct_params, receiver_type_args);
                self.push_receiver_params(&effective);
                self.push_type_params(type_params);
                let param_types: Vec<Type<'a>> = params
                    .iter()
                    .map(|p| match &p.ty {
                        Some(t) => self.resolve_ast_type(t),
                        None => {
                            self.error_span(
                                p.span,
                                format!("parameter `{}` is missing a type annotation", p.name),
                            );
                            Type::Error
                        }
                    })
                    .collect();
                let resolved_return = return_type.as_ref().map(|t| self.resolve_ast_type(t));
                self.pop_type_params();
                self.pop_type_params();
                if self
                    .receiver_methods
                    .insert(
                        key,
                        super::MethodInfo {
                            params,
                            receiver_type_args,
                            type_params,
                            return_type: return_type.clone(),
                            mutable: *receiver_mutable,
                            param_types,
                            resolved_return,
                        },
                    )
                    .is_some()
                {
                    self.error_span(
                        *span,
                        format!("duplicate receiver method `{name}` on type `{receiver_type}`"),
                    );
                }
            }
        }
    }

    fn check_embedded_method_ambiguity(&mut self) {
        // Collect errors first so we don't borrow `self` mutably while iterating
        // over `self.structs`.
        let mut errors: Vec<(ast::Span, String)> = Vec::new();
        for (struct_name, info) in self.structs.iter() {
            if info.embeds.is_empty() {
                continue;
            }

            // Map each method name to the list of embedded structs that promote it.
            let mut promoted: HashMap<&str, Vec<&str>> = HashMap::new();
            for embed in info.embeds.iter() {
                let mut seen = HashSet::new();
                self.collect_promoted_methods(embed.name, &mut promoted, embed.name, &mut seen);
            }

            for (method_name, sources) in promoted.iter() {
                if sources.len() > 1 {
                    errors.push((
                        info.embeds[0].span,
                        format!(
                            "Ambiguous method '{}' on struct '{}'; multiple embedded structs promote it",
                            method_name, struct_name
                        ),
                    ));
                }
            }
        }

        for (span, message) in errors {
            self.error_span(span, message);
        }
    }

    fn collect_promoted_methods(
        &self,
        embed_name: &'a str,
        promoted: &mut HashMap<&'a str, Vec<&'a str>>,
        source: &'a str,
        seen: &mut HashSet<&'a str>,
    ) {
        if !seen.insert(embed_name) {
            return;
        }

        // Direct methods on the embedded struct are promoted.
        for ((rt, method_name), _) in self.receiver_methods.iter() {
            if *rt == embed_name {
                promoted.entry(*method_name).or_default().push(source);
            }
        }

        // Methods promoted by nested embeds are also promoted.
        if let Some(info) = self.structs.get(embed_name) {
            for nested in info.embeds.iter() {
                self.collect_promoted_methods(nested.name, promoted, source, seen);
            }
        }
    }

    fn collect_summon_signatures(&mut self) {
        let mut names = HashSet::new();
        for stmt in self.program.statements {
            if let ast::Stmt::Summon { functions, .. } = stmt {
                for f in *functions {
                    let ret = self.resolve_ast_type(&f.return_type);
                    let payload = match &ret {
                        Type::Generic {
                            base: "Promise",
                            args,
                        } => &args[0],
                        other => other,
                    };
                    let fallible = matches!(
                        payload,
                        Type::Generic {
                            base: "Exception",
                            ..
                        }
                    );
                    if !fallible && !f.total {
                        self.error_span(f.span, format!("summoned function `{}` requires an Exception<T, E> return or the explicit `total` marker", f.name));
                    }
                    if fallible && f.total {
                        self.error_span(f.span, "`total` cannot declare an Exception return");
                    }
                    let params = f
                        .params
                        .iter()
                        .map(|p| {
                            if p.default_value.is_some() {
                                self.error_span(
                                    p.span,
                                    "summoned parameters cannot have DekaScript default values",
                                );
                            }
                            match &p.ty {
                                Some(ty) => self.resolve_ast_type(ty),
                                None => {
                                    self.error_span(
                                        p.span,
                                        "summoned parameter requires an explicit type",
                                    );
                                    Type::Error
                                }
                            }
                        })
                        .collect();
                    if !names.insert(f.name) || self.lookup_var(f.name).is_some() {
                        self.error_span(f.span, format!("duplicate summoned binding `{}`", f.name));
                    }
                    self.globals.insert(
                        f.name,
                        Type::Function {
                            params,
                            ret: Box::new(ret),
                            optional: 0,
                        },
                    );
                }
            }
        }
        // Only direct calls may reference the binding. Wrapping it in an ordinary
        // DS function creates the authored, checked public boundary.
        let mut calls = HashSet::new();
        let mut references = Vec::new();
        for stmt in self.program.statements {
            crate::visit::walk_stmt(stmt, &mut |expr| match expr {
                ast::Expr::Call {
                    callee: ast::Expr::Identifier { name, span },
                    ..
                } if names.contains(name) => {
                    calls.insert(span.byte_start);
                }
                ast::Expr::Identifier { name, span } if names.contains(name) => {
                    references.push((name.to_string(), *span))
                }
                _ => {}
            });
            if let ast::Stmt::Export {
                decl:
                    ast::ExportDecl::NamedGroup {
                        names: exports,
                        source: None,
                    },
                ..
            } = stmt
            {
                for export in *exports {
                    if names.contains(export.name) {
                        self.error_span(
                            export.span,
                            format!(
                                "summoned function `{}` is file-private and cannot be exported",
                                export.name
                            ),
                        );
                    }
                }
            }
        }
        for (name, span) in references {
            if !calls.contains(&span.byte_start) {
                self.error_span(span, format!("summoned function `{name}` cannot escape its file; use a direct call inside a DekaScript function"));
            }
        }
    }

    fn collect_function_signatures(&mut self) {
        for stmt in self.program.statements {
            let (name, type_params, params, return_type) = match stmt {
                ast::Stmt::Function {
                    name,
                    type_params,
                    params,
                    return_type,
                    ..
                } => (*name, *type_params, *params, return_type.as_ref()),
                ast::Stmt::Export {
                    decl:
                        ast::ExportDecl::Function {
                            name,
                            type_params,
                            params,
                            return_type,
                            ..
                        },
                    ..
                } => (*name, *type_params, *params, return_type.as_ref()),
                _ => continue,
            };

            // rfd#56 phase 2: keep the declared bounds for the call-site
            // check that verifies an inferred type argument against its
            // bound. Resolution reuses the bound cache filled by
            // push_type_params below.
            let bounds: Vec<(&'a str, Type<'a>)> = type_params
                .iter()
                .filter(|p| p.bound.is_some())
                .map(|p| (p.name, self.resolve_bound(p.bound.as_ref().unwrap())))
                .collect();
            if !bounds.is_empty() {
                self.fn_param_bounds.insert(name, bounds);
            }

            self.push_type_params(type_params);

            let param_types: Vec<Type<'a>> = params
                .iter()
                .map(|p| match &p.ty {
                    Some(t) => self.resolve_ast_type(t),
                    None => {
                        self.error_span(
                            p.span,
                            format!("parameter `{}` is missing a type annotation", p.name),
                        );
                        Type::Error
                    }
                })
                .collect();

            let ret = match return_type {
                Some(t) => self.resolve_ast_type(t),
                None => Type::Infer,
            };

            self.pop_type_params();

            let optional = params
                .iter()
                .rev()
                .take_while(|p| p.default_value.is_some())
                .count();

            self.globals.insert(
                name,
                Type::Function {
                    params: param_types,
                    ret: Box::new(ret),
                    optional,
                },
            );
        }
    }

    /// Seed module-scope `const`/`let` bindings into the top-level scope so
    /// function bodies — checked before the top-level statement walk
    /// (deka#600) — can resolve them. The seed type is the declared
    /// annotation or `Type::Infer`; `check_binding` refines it when the
    /// declaration is checked in source order. Seeding stays silent so
    /// duplicate-declaration and annotation diagnostics are reported exactly
    /// once, by `check_binding`. Each seeded name stays in
    /// `pending_module_bindings` until then: module-level statements keep
    /// rejecting forward references, while function bodies may capture.
    fn collect_module_value_bindings(&mut self) {
        let prev_infer_only = self.infer_only;
        self.infer_only = true;
        for stmt in self.program.statements {
            if let ast::Stmt::TupleBinding {
                names,
                ty,
                is_const,
                ..
            } = stmt
            {
                let resolved = ty.as_ref().map(|t| self.resolve_ast_type(t));
                for (i, name) in names.iter().enumerate() {
                    let seed = match &resolved {
                        Some(Type::Tuple { elements }) => {
                            elements.get(i).cloned().unwrap_or(Type::Error)
                        }
                        _ => Type::Infer,
                    };
                    self.scopes[0].insert(name, seed);
                    self.capture_scopes[0].insert(name, super::hooks::CaptureClass::Other);
                    if !*is_const {
                        self.mutables[0].insert(name);
                    }
                    self.pending_module_bindings.insert(name);
                }
                continue;
            }
            let (name, ty, value, mutable) = match stmt {
                ast::Stmt::Const { name, ty, value, .. } => (*name, ty.as_ref(), Some(value), false),
                ast::Stmt::Let { name, ty, value, .. } => (*name, ty.as_ref(), Some(value), true),
                ast::Stmt::Export {
                    decl: ast::ExportDecl::Const { name, ty, value, .. },
                    ..
                } => (*name, ty.as_ref(), Some(value), false),
                _ => continue,
            };
            let seed = if let Some(annot) = ty {
                self.resolve_ast_type(annot)
            } else if let Some(ast::Expr::Identifier { name: init, .. }) = value {
                self.globals
                    .get(init)
                    .cloned()
                    .or_else(|| self.scopes[0].get(init).cloned())
                    .unwrap_or(Type::Infer)
            } else {
                Type::Infer
            };
            let class = match value {
                Some(ast::Expr::Identifier { name: init, .. })
                    if *init == "useEffect"
                        || self.capture_scopes[0].get(init)
                            == Some(&super::hooks::CaptureClass::UseEffect) =>
                {
                    super::hooks::CaptureClass::UseEffect
                }
                _ => super::hooks::CaptureClass::Other,
            };
            self.scopes[0].insert(name, seed);
            self.capture_scopes[0].insert(name, class);
            if mutable {
                self.mutables[0].insert(name);
            }
            self.pending_module_bindings.insert(name);
        }
        self.infer_only = prev_infer_only;
    }

    /// Run the silent inference pass used to seed cross-module function
    /// signatures for `collect_module_exports`.
    pub(crate) fn infer_all_function_signatures(&mut self) {
        self.collect_declarations();
        self.collect_module_value_bindings();
        self.collect_function_signatures();
        self.collect_summon_signatures();
        self.collect_receiver_methods();
        self.check_embedded_method_ambiguity();
        self.validate_interface_declarations();
        self.infer_function_return_types();
    }

    /// Silent pre-check pass that infers return types for unannotated functions.
    ///
    /// Runs without emitting diagnostics and clears lowering side-effects so the
    /// real check pass sees stable, forward-reference-resolved signatures.
    fn infer_function_return_types(&mut self) {
        self.infer_only = true;
        // Each round propagates one hop of a forward-reference chain, and real
        // modules are a handful of hops deep, so ten rounds is generous. If it
        // is ever not enough, the diagnostic below names the function instead
        // of silently continuing with a half-inferred type (deka#367).
        let mut converged = false;
        let mut pending: Vec<(&'a str, ast::Span)> = Vec::new();
        for _ in 0..10 {
            let mut changed = false;
            pending.clear();
            for stmt in self.program.statements {
                let info = match stmt {
                    ast::Stmt::Function {
                        name,
                        type_params,
                        params,
                        return_type,
                        body,
                        span,
                        is_async,
                        ..
                    } => Some((
                        *name,
                        *type_params,
                        *params,
                        return_type.as_ref(),
                        *body,
                        *is_async,
                        *span,
                    )),
                    ast::Stmt::Export {
                        decl:
                            ast::ExportDecl::Function {
                                name,
                                type_params,
                                params,
                                return_type,
                                body,
                                is_async,
                                ..
                            },
                        span,
                        ..
                    } => Some((
                        *name,
                        *type_params,
                        *params,
                        return_type.as_ref(),
                        *body,
                        *is_async,
                        *span,
                    )),
                    _ => None,
                };
                if let Some((name, type_params, params, return_type, body, is_async, span)) = info {
                    if return_type.is_none() {
                        pending.push((name, span));
                    }
                    let prev = self.globals.get(name).cloned();
                    self.check_function(
                        name,
                        type_params,
                        params,
                        return_type,
                        body,
                        is_async,
                        span,
                    );
                    let new = self.globals.get(name).cloned();
                    if prev != new {
                        changed = true;
                    }
                }
            }
            if !changed {
                converged = true;
                break;
            }
        }
        self.infer_only = false;
        if !converged {
            for (name, span) in pending {
                if let Some(ty) = self.globals.get(name).cloned() {
                    if let Type::Function { ret, .. } = ty.unhook() {
                        if matches!(*ret, Type::Infer) {
                            self.error_span(
                                span,
                                format!(
                                    "could not infer a return type for `{name}`; add an explicit return type"
                                ),
                            );
                        }
                    }
                }
            }
        }
        self.reset_lowering_state();
    }

    pub(super) fn check_statement(&mut self, stmt: &ast::Stmt<'a>) {
        match stmt {
            ast::Stmt::Try {
                body,
                catch_name,
                catch_type,
                catch_body,
                span,
            } => self.check_try(body, catch_name, catch_type.as_ref(), catch_body, *span),
            ast::Stmt::TupleBinding {
                names,
                ty,
                value,
                is_const,
                span,
            } => {
                let expected = ty.as_ref().map(|t| self.resolve_ast_type(t));
                let actual = self.check_exception_use(
                    value,
                    super::exceptions::Use::Value,
                    expected.clone(),
                );
                if let Some(expected) = &expected {
                    if !self.is_assignable(expected, &actual) {
                        self.error_span(*span, format!("expected `{expected}`, found `{actual}`"));
                    }
                }
                let binding_type = expected.unwrap_or(actual);
                let elements = if let Type::Tuple { elements } = binding_type {
                    if names.len() != elements.len() {
                        self.error_span(*span, format!("tuple has {} positions, but destructuring binds {} names; bind every position exactly once", elements.len(), names.len()));
                    }
                    elements
                } else {
                    self.error_span(*span, format!("destructuring requires a tuple, found `{binding_type}`; annotate the value with a tuple type such as [number, string]"));
                    Vec::new()
                };
                for (i, name) in names.iter().enumerate() {
                    if names[..i].contains(name) {
                        self.error_span(*span, format!("duplicate tuple binding `{name}`; use a distinct name for each position"));
                    }
                    let ty = elements.get(i).cloned().unwrap_or(Type::Error);
                    let class = self.classify_initializer(value, &ty);
                    if *is_const {
                        self.declare_var_class(name, ty, class);
                    } else {
                        self.declare_mutable_var_class(name, ty, class);
                    }
                    if self.scopes.len() == 1 {
                        self.pending_module_bindings.remove(name);
                    }
                }
            }
            ast::Stmt::Const {
                name,
                ty,
                value,
                span,
            } => {
                self.check_binding(name, ty.as_ref(), value, false, *span);
            }
            ast::Stmt::Let {
                name,
                ty,
                value,
                span,
            } => {
                self.check_binding(name, ty.as_ref(), value, true, *span);
            }
            ast::Stmt::UnwrapLet {
                name,
                ty,
                is_const,
                scrutinee,
                alternative,
                span,
            } => {
                self.check_unwrap_binding(
                    name,
                    ty.as_ref(),
                    *is_const,
                    scrutinee,
                    alternative,
                    *span,
                );
            }
            ast::Stmt::Function {
                name,
                type_params,
                params,
                return_type,
                body,
                span,
                is_async,
                ..
            } => {
                self.check_function(
                    name,
                    type_params,
                    params,
                    return_type.as_ref(),
                    body,
                    *is_async,
                    *span,
                );
            }
            ast::Stmt::Export { decl, .. } => match decl {
                ast::ExportDecl::Const { name, ty, value } => {
                    self.check_binding(name, ty.as_ref(), value, false, value.span());
                    if ty.is_none()
                        && self
                            .scopes
                            .first()
                            .and_then(|scope| scope.get(name))
                            .is_some_and(|value_type| !super::is_concrete_export_type(value_type))
                    {
                        self.error_at_expr(
                            value,
                            format!(
                                "could not infer a type for exported constant `{name}`; add an explicit type annotation"
                            ),
                        );
                    }
                }
                ast::ExportDecl::Function { .. } => {
                    self.check_export_function(stmt);
                }
                ast::ExportDecl::NamedGroup { .. } => {
                    // Named re-exports refer to already-checked top-level
                    // declarations; nothing to validate at this scope.
                }
            },
            ast::Stmt::Expr { expr, .. } => {
                self.check_exception_use(expr, super::exceptions::Use::Statement, None);
            }
            ast::Stmt::Return { value, span } => {
                self.check_return(value.as_ref(), *span);
            }
            ast::Stmt::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                let cond_type = self.check_expr(condition);
                self.expect_boolean(&cond_type, condition.span());
                let saved_flow = self.index_flow.clone();
                self.assume_index_condition(condition);
                self.push_value_scope();
                self.mutables.push(HashSet::new());
                self.with_hook_conditional(|this| {
                    for s in then_body.iter() {
                        this.check_statement(s);
                    }
                });
                self.pop_value_scope();
                self.mutables.pop();
                self.push_value_scope();
                self.mutables.push(HashSet::new());
                self.index_flow.restrict_to(&saved_flow);
                self.with_hook_conditional(|this| {
                    for s in else_body.iter() {
                        this.check_statement(s);
                    }
                });
                self.pop_value_scope();
                self.mutables.pop();
                self.index_flow.restrict_to(&saved_flow);
            }
            ast::Stmt::Block { body, .. } => {
                self.push_value_scope();
                self.mutables.push(HashSet::new());
                for s in body.iter() {
                    self.check_statement(s);
                }
                self.pop_value_scope();
                self.mutables.pop();
            }
            ast::Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                self.push_value_scope();
                self.mutables.push(HashSet::new());
                self.index_flow.kill();
                if let Some(init) = init {
                    self.check_for_init(init);
                }
                if let Some(cond) = condition {
                    let cond_type = self.check_expr(cond);
                    self.expect_boolean(&cond_type, cond.span());
                }
                if let Some(step) = step {
                    self.check_expr(step);
                }
                self.assume_index_loop(init.as_ref(), condition.as_ref(), step.as_ref(), body);
                self.loop_depth += 1;
                for s in body.iter() {
                    self.check_statement(s);
                }
                self.loop_depth -= 1;
                self.index_flow.kill();
                self.pop_value_scope();
                self.mutables.pop();
            }
            ast::Stmt::ForOf {
                name,
                is_const,
                iterable,
                body,
                ..
            } => {
                self.index_flow.kill();
                let iterable_type = self.check_expr(iterable);
                let element_type = if let Type::Param { name: param } = &iterable_type {
                    match self.lookup_param_bound(param) {
                        // rfd#56 phase 2: an Array bound unlocks iteration,
                        // the element type coming from the bound — the same
                        // rule as indexing (expr.rs).
                        Some(bound @ (Type::Array { .. } | Type::Named { name: "string" })) => {
                            bound.collection_element()
                        }
                        Some(bound) => {
                            self.error_span(
                                iterable.span(),
                                format!(
                                    "cannot iterate over a value of type parameter `{param}` \
                                     bounded by `{bound}` (rfd#56)"
                                ),
                            );
                            Type::Error
                        }
                        None => {
                            // rfd#56 phase 1: iteration is not on the
                            // unbounded-T operation list. Without this,
                            // collection_element would silently return
                            // `Infer` for the element type.
                            self.reject_param_operation(param, "iterate over", iterable.span());
                            Type::Error
                        }
                    }
                } else {
                    iterable_type.collection_element()
                };
                self.push_value_scope();
                self.mutables.push(HashSet::new());
                self.declare_var(name, element_type);
                if !*is_const {
                    self.mutables.last_mut().unwrap().insert(*name);
                }
                self.loop_depth += 1;
                for s in body.iter() {
                    self.check_statement(s);
                }
                self.loop_depth -= 1;
                self.pop_value_scope();
                self.mutables.pop();
            }
            ast::Stmt::Break { span } => {
                if self.loop_depth == 0 {
                    self.error_span(*span, "`break` outside of loop");
                }
            }
            ast::Stmt::Continue { span } => {
                if self.loop_depth == 0 {
                    self.error_span(*span, "`continue` outside of loop");
                }
            }
            ast::Stmt::Struct {
                name,
                type_params,
                fields,
                embeds,
                is_super: _,
                span: _,
            } => {
                self.push_type_params(type_params);
                for field in *fields {
                    self.resolve_ast_type(&field.ty);
                }
                for embed in *embeds {
                    if !self.structs.contains_key(embed.name) {
                        self.error_span(
                            embed.span,
                            format!("unknown embed type `{}` in struct `{name}`", embed.name),
                        );
                    }
                }
                self.pop_type_params();
            }
            ast::Stmt::Enum {
                name: _,
                cases,
                type_params,
                is_super: _,
                span: _,
            } => {
                // `enum Box<T> { Full(T) }` — T must be in scope while the case
                // payload types are resolved, or it reports `unknown type T`.
                self.push_type_params(type_params);
                for case in *cases {
                    if let Some(payload) = &case.payload {
                        self.resolve_ast_type(payload);
                    }
                }
                self.pop_type_params();
            }
            ast::Stmt::Empty { .. }
            | ast::Stmt::TypeAlias { .. }
            | ast::Stmt::Opaque { .. }
            | ast::Stmt::Summon { .. }
            | ast::Stmt::Newtype { .. }
            | ast::Stmt::Interface { .. }
            | ast::Stmt::ReceiverMethod { .. } => {
                // Already collected and validated lazily at use sites (or no-op).
            }
            // Imports are seeded before statement checking. Resolved exports
            // retain their signatures; unresolved imports have already emitted
            // a diagnostic and are bound to `Error`, never `Infer`.
            ast::Stmt::Import { .. } => {}
        }
    }

    fn check_export_function(&mut self, stmt: &ast::Stmt<'a>) {
        if let ast::Stmt::Export {
            decl:
                ast::ExportDecl::Function {
                    name,
                    type_params,
                    params,
                    return_type,
                    body,
                    is_async,
                    ..
                },
            span,
        } = stmt
        {
            self.check_function(
                name,
                type_params,
                params,
                return_type.as_ref(),
                body,
                *is_async,
                *span,
            );
        }
    }

    fn check_for_init(&mut self, init: &ast::ForInit<'a>) {
        match init {
            ast::ForInit::Const { name, value } => {
                let value_type = self.check_expr(value);
                self.declare_var(name, value_type);
            }
            ast::ForInit::Let { name, value } => {
                let value_type = self.check_expr(value);
                self.declare_mutable_var(name, value_type);
            }
            ast::ForInit::Expr(expr) => {
                self.check_expr(expr);
            }
        }
    }

    /// `let name = unwrap(scrutinee) or { … }` (deka#445).
    ///
    /// The binding takes the success payload; the block runs when the value is
    /// absent, and either produces the binding's value or leaves the function.
    fn check_unwrap_binding(
        &mut self,
        name: &'a str,
        ty: Option<&ast::Type<'a>>,
        is_const: bool,
        scrutinee: &ast::Expr<'a>,
        alternative: &ast::UnwrapAlternative<'a>,
        span: ast::Span,
    ) {
        let scrutinee_type = self.check_expr(scrutinee);

        // `unwrap` is about a value that might be absent, and DekaScript has
        // exactly two of those. Anything else should use `match`, which says
        // so rather than leaving the reader to guess.
        let bound = match &scrutinee_type {
            Type::Option { inner } => (**inner).clone(),
            Type::Generic {
                base: "Result",
                args,
            } if args.len() == 2 => args[0].clone(),
            Type::Infer | Type::Error => Type::Infer,
            other => {
                self.error_span(
                    span,
                    format!(
                        "`unwrap` works on `Option` and `Result`, found type `{other}`; use `match`"
                    ),
                );
                Type::Error
            }
        };

        let declared = ty.map(|ty| self.resolve_ast_type(ty));
        if let Some(declared) = &declared {
            if !self.is_assignable(declared, &bound) && !matches!(bound, Type::Infer | Type::Error)
            {
                self.error_span(
                    span,
                    format!("binding declared as `{declared}`, but `unwrap` yields `{bound}`"),
                );
            }
        }

        // The alternative is checked in its own scope, then the binding is
        // declared -- it cannot see the name it is providing.
        self.push_value_scope();
        self.mutables.push(HashSet::new());
        match alternative {
            ast::UnwrapAlternative::Block(stmts) => {
                for inner in stmts.iter() {
                    self.check_statement(inner);
                }
            }
            ast::UnwrapAlternative::Match(arms) => {
                self.check_unwrap_match_arms(&scrutinee_type, &bound, arms, span);
            }
        }
        self.mutables.pop();
        self.pop_value_scope();

        let bound_type = declared.unwrap_or(bound);
        if is_const {
            self.declare_var(name, bound_type);
        } else {
            self.declare_mutable_var(name, bound_type);
        }
    }

    /// The arms of `unwrap(x) or match { … }`.
    ///
    /// They match the *original* value, so `unwrap` supplies the success arm
    /// and the author writes the rest. That keeps the desugaring literal --
    /// `or match` is "the remaining arms" -- and lets the existing
    /// exhaustiveness check run over the whole set rather than a stripped
    /// payload (deka#445).
    fn check_unwrap_match_arms(
        &mut self,
        scrutinee_type: &Type<'a>,
        bound: &Type<'a>,
        arms: &[ast::MatchArm<'a>],
        span: ast::Span,
    ) {
        // `Option` has no failure payload, so the only arm would be `None`.
        // A one-armed match written to look thorough is the shape of slop even
        // when it is correct.
        if matches!(scrutinee_type, Type::Option { .. }) {
            self.error_span(
                span,
                format!(
                    "`or match` has nothing to match on for `{scrutinee_type}`; use `or {{ … }}`"
                ),
            );
            return;
        }

        let success_case = match scrutinee_type {
            Type::Generic { base: "Result", .. } => "Ok",
            _ => return,
        };

        let mut coverage = Coverage::success_case(success_case);
        for arm in arms.iter() {
            self.push_value_scope();
            self.mutables.push(HashSet::new());
            self.check_pattern(&arm.pattern, scrutinee_type);
            let arm_type = self.check_expr(&arm.body);
            self.mutables.pop();
            self.pop_value_scope();

            if !self.is_assignable(bound, &arm_type)
                && !matches!(arm_type, Type::Infer | Type::Error | Type::Never)
                && !matches!(bound, Type::Infer | Type::Error)
            {
                self.error_span(
                    arm.span,
                    format!("arm has type `{arm_type}`, but the binding is `{bound}`"),
                );
            }

            coverage = coverage.merge(Coverage::of_pattern(&arm.pattern, &self.enum_case_patterns));
        }

        self.check_match_exhaustiveness(span, scrutinee_type, &coverage);
    }

    fn check_binding(
        &mut self,
        name: &'a str,
        ty: Option<&ast::Type<'a>>,
        value: &ast::Expr<'a>,
        mutable: bool,
        span: ast::Span,
    ) {
        if let ast::Expr::Build { body, .. } = value {
            return self.check_dev_binding(name, ty, body, mutable, value, span);
        }
        let expected = ty.map(|t| self.resolve_ast_type(t));
        let value_type = self.check_exception_use(value, super::exceptions::Use::Value, expected.clone());
        let final_type = if let Some(expected) = expected {
            if let Type::Option { inner } = &expected {
                // Explicit `Option<T>` bindings must be initialized with
                // `Some(...)` or `none`; the struct-field sugar that accepts a
                // concrete `T` does not apply here.
                if !self.is_assignable(&expected, &value_type) {
                    let message = match &value_type {
                        Type::Option { inner: actual } => format!(
                            "`{name}` expects Option payload type `{inner}`, found payload type `{actual}`"
                        ),
                        _ => format!(
                            "`{name}` is declared Option<{inner}> but the initializer is {value_type}"
                        ),
                    };
                    self.error_at_expr(value, message);
                }
            } else if !self.is_assignable(&expected, &value_type) {
                self.error_at_expr(
                    value,
                    super::with_union_narrowing_hint(
                        format!("expected type `{expected}`, found type `{value_type}`"),
                        &expected,
                        &value_type,
                    ),
                );
            }
            if let (Type::Named { name: "Component" }, Type::Function { params, .. }) =
                (&expected, &value_type.unhook())
            {
                if params.len() == 1 { Type::Generic { base: "Component", args: params.clone() } } else { expected }
            } else { expected }
        } else {
            value_type
        };
        let class = self.classify_initializer(value, &final_type);
        if mutable {
            self.declare_mutable_var_class(name, final_type, class);
        } else {
            self.declare_var_class(name, final_type, class);
        }
        self.index_flow.remember_integer(name, value);
        // A declaration at module scope activates its seed for module-level
        // lookups; nested declarations never touch module pending state.
        if self.scopes.len() == 1 {
            self.pending_module_bindings.remove(name);
        }
    }

    /// Check the v1 build-only binding form. The binding has type `T`; its
    /// separate entry must return `Result<T, string>` for the Deka host.
    fn check_dev_binding(
        &mut self,
        name: &'a str,
        annotation: Option<&ast::Type<'a>>,
        body: &'a [ast::Stmt<'a>],
        mutable: bool,
        value: &ast::Expr<'a>,
        span: ast::Span,
    ) {
        let valid_position = !mutable && self.scopes.len() == 1 && !self.in_function;
        if !valid_position {
            self.error_at_expr(
                value,
                "`build` is only valid as the initializer of a module-level `const`",
            );
            self.declare_var(name, Type::Error);
            return;
        }

        let Some(annotation) = annotation else {
            self.error_at_expr(
                value,
                "`build` bindings require an explicit declared type, e.g. `const users: Array<User> = build { ... }`",
            );
            self.declare_var(name, Type::Error);
            self.pending_module_bindings.remove(name);
            return;
        };

        let expected = self.resolve_ast_type(annotation);
        let descriptor = match self.descriptor_tree(&expected, span) {
            Ok(tree) => tree,
            Err(message) => {
                self.error_at_expr(
                    value,
                    format!(
                        "`build` binding `{name}` has an unrepresentable declared type: {message}"
                    ),
                );
                self.declare_var(name, expected);
                self.pending_module_bindings.remove(name);
                return;
            }
        };

        let expected_result = Type::Generic {
            base: "Result",
            args: vec![expected.clone(), Type::Named { name: "string" }],
        };
        let saved_in_function = self.in_function;
        let saved_in_async = self.in_async_function;
        let saved_return_type = self.return_type.clone();
        let saved_catches = std::mem::take(&mut self.exception_catches);
        self.in_function = true;
        // Dev entries may await build-only bridge operations. The DS contract
        // is still Result<T, string>; JavaScript wraps it in a Promise.
        self.in_async_function = true;
        self.return_type = Some(expected_result);
        self.push_value_scope();
        self.mutables.push(HashSet::new());
        for stmt in body {
            self.check_statement(stmt);
        }
        self.pop_value_scope();
        self.mutables.pop();
        self.in_function = saved_in_function;
        self.in_async_function = saved_in_async;
        self.return_type = saved_return_type;
        self.exception_catches = saved_catches;

        if !body_always_returns(body) {
            self.error_at_expr(
                value,
                format!(
                    "`build` binding `{name}` must return `Result<{expected}, string>` on every path"
                ),
            );
        }

        self.dev_blocks.insert(
            value as *const ast::Expr<'a>,
            super::DevBlock { body, descriptor },
        );
        self.declare_var(name, expected);
        self.pending_module_bindings.remove(name);
    }

    pub(super) fn check_function(
        &mut self,
        name: &'a str,
        type_params: &'a [ast::TypeParam<'a>],
        params: &'a [ast::Param<'a>],
        return_type: Option<&ast::Type<'a>>,
        body: &'a [ast::Stmt<'a>],
        is_async: bool,
        _span: ast::Span,
    ) {
        self.index_flow.kill();
        // Use the previously collected signature for parameter types so that
        // errors about missing annotations are reported exactly once.
        let collected = self.globals.get(name).cloned();
        let was_hook = collected.as_ref().is_some_and(Type::is_hook_fn);
        let (param_types, collected_ret, optional) = match collected.map(|ty| ty.unhook()) {
            Some(Type::Function {
                params,
                ret,
                optional,
            }) => (params, Some(*ret), optional),
            _ => {
                let mut pts = Vec::new();
                for p in params {
                    match &p.ty {
                        Some(t) => pts.push(self.resolve_ast_type(t)),
                        None => {
                            self.error_span(
                                p.span,
                                format!("parameter `{}` is missing a type annotation", p.name),
                            );
                            pts.push(Type::Error);
                        }
                    }
                }
                let optional = params
                    .iter()
                    .rev()
                    .take_while(|p| p.default_value.is_some())
                    .count();
                (pts, None, optional)
            }
        };

        self.push_type_params(type_params);

        // Reuse the return type resolved during signature collection so an
        // unresolvable annotation is reported once (deka#494). The fallback
        // path (no collected signature — e.g. a nested function, which
        // collect_function_signatures does not visit) resolves it here, its
        // only resolution.
        let explicit_ret = match (return_type, collected_ret) {
            (Some(_), Some(ret)) => Some(ret),
            (Some(t), None) => Some(self.resolve_ast_type(t)),
            (None, _) => None,
        };
        let (body_expected_ret, final_ret) =
            self.function_return_context(is_async, explicit_ret.clone(), _span);

        self.push_value_scope();
        self.mutables.push(HashSet::new());

        // Make the function available to its own body for recursion. Use the
        // signature collected earlier; the return type will be refined after
        // the body is checked.
        let self_type = Type::Function {
            params: param_types.clone(),
            ret: Box::new(final_ret.clone()),
            optional,
        };
        self.declare_var(
            name,
            if was_hook {
                self_type.as_hook()
            } else {
                self_type
            },
        );

        for (p, t) in params.iter().zip(param_types.iter()) {
            if let Some(default) = &p.default_value {
                let actual = self.check_exception_use(
                    default,
                    super::exceptions::Use::Value,
                    Some(t.clone()),
                );
                if !self.is_assignable(t, &actual) {
                    self.error_at_expr(
                        default,
                        format!("expected default type `{t}`, found type `{actual}`"),
                    );
                }
            }
            self.declare_var_class(p.name, t.clone(), Self::param_capture_class(t));
        }

        let saved_in_function = self.in_function;
        let saved_in_async = self.in_async_function;
        let saved_return_type = self.return_type.clone();
        let saved_catches = std::mem::take(&mut self.exception_catches);
        let is_interactive_component = self.interactive_components.contains(name);
        let hook_frame = self.push_hook_frame(Some(name), Self::is_component_return(&explicit_ret));
        if was_hook {
            self.hook_body_called = true;
        }
        self.in_function = true;
        self.in_async_function = is_async;
        self.return_type = body_expected_ret.clone();
        if is_interactive_component {
            self.interactive_component_depth += 1;
        }

        for stmt in body {
            self.check_statement(stmt);
        }

        if is_interactive_component {
            self.interactive_component_depth -= 1;
        }
        let body_called_hook = self.pop_hook_frame(hook_frame);

        // A function that declares a value-producing return type must actually
        // return on every path. Without this, `fn f() string { }` typechecks
        // and hands every caller `undefined` (deka#476).
        if let Some(declared) = explicit_ret.as_ref() {
            if return_type_requires_value(declared) && !body_always_returns(body) {
                self.error_span(
                    _span,
                    format!(
                        "function `{name}` declares return type `{declared}` but does not return a value on every path"
                    ),
                );
            }
        }

        self.in_async_function = saved_in_async;

        let final_ret = if is_async {
            // The public signature is always the declared Promise type (or a
            // Promise wrapping the inferred payload for unannotated functions).
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
                        args: vec![Type::Generic {
                            base: "Option",
                            args: vec![Type::Never],
                        }],
                    }),
            }
        } else {
            body_expected_ret.unwrap_or_else(|| {
                self.return_type.take().unwrap_or(Type::Generic {
                    base: "Option",
                    args: vec![Type::Never],
                })
            })
        };

        self.in_function = saved_in_function;
        self.return_type = saved_return_type;
        self.exception_catches = saved_catches;
        self.pop_value_scope();
        self.mutables.pop();

        self.pop_type_params();

        // Update the global function type with the final (possibly inferred)
        // return type. A body that called a hook-typed function is itself
        // hook-typed — the color is on the type, not a name table.
        let fn_type = Type::Function {
            params: param_types,
            ret: Box::new(final_ret),
            optional,
        };
        self.globals.insert(
            name,
            if body_called_hook || was_hook {
                fn_type.as_hook()
            } else {
                fn_type
            },
        );
    }

    /// Computes the return type expected from the function body and the
    /// public return type of the function. For async functions the body must
    /// produce the payload type `T`, while the public type is `Promise<T>`.
    pub(super) fn function_return_context(
        &mut self,
        is_async: bool,
        explicit_ret: Option<Type<'a>>,
        span: ast::Span,
    ) -> (Option<Type<'a>>, Type<'a>) {
        if !is_async {
            return (explicit_ret.clone(), explicit_ret.unwrap_or(Type::Infer));
        }

        match explicit_ret {
            Some(Type::Generic {
                base: "Promise",
                args,
            }) if args.len() == 1 => {
                let inner = args[0].clone();
                let public = Type::Generic {
                    base: "Promise",
                    args: vec![inner.clone()],
                };
                (Some(inner), public)
            }
            Some(other) => {
                self.error_span(
                    span,
                    format!("async function must return Promise<T>, found type `{other}`"),
                );
                (Some(other.clone()), other)
            }
            None => (
                None,
                Type::Generic {
                    base: "Promise",
                    args: vec![Type::Generic {
                        base: "Option",
                        args: vec![Type::Never],
                    }],
                },
            ),
        }
    }

    pub(super) fn check_receiver_method(
        &mut self,
        receiver_type: &'a str,
        receiver_type_args: &'a [ast::TypeParam<'a>],
        receiver_name: &'a str,
        receiver_mutable: bool,
        name: &'a str,
        type_params: &'a [ast::TypeParam<'a>],
        params: &'a [ast::Param<'a>],
        // The annotation itself is no longer read here — its resolved form
        // comes from the MethodInfo collected earlier (deka#494).
        _return_type: Option<&ast::Type<'a>>,
        body: &'a [ast::Stmt<'a>],
        is_async: bool,
        _span: ast::Span,
    ) {
        self.index_flow.kill();
        if receiver_mutable && self.newtypes.contains_key(receiver_type) {
            self.error_span(
                _span,
                format!("mutable receiver methods are not allowed on newtype `{receiver_type}`"),
            );
        }
        if receiver_mutable && super::is_primitive_receiver_name(receiver_type) {
            // Primitives are immutable values; there is no mutable location
            // to receive (deka#527).
            self.error_span(
                _span,
                format!("mutable receiver methods are not allowed on primitive `{receiver_type}`"),
            );
        }

        let info = match self.receiver_methods.get(&(receiver_type, name)) {
            Some(i) => i.clone(),
            None => return,
        };

        // Annotations were resolved during collection (deka#494); reuse them
        // so unknown types are reported exactly once.
        let param_types = info.param_types.clone();

        // The receiver-bound parameters (`fn (s Signal<T>)`, rfd#56 dsc#101)
        // and then the method's own type parameters (`fn (s Signal) set<T>(…)`,
        // rfd#56 phase 1) are in scope for the body, the same as a plain
        // generic function's. Declaring both is rejected during collection.
        let struct_params: &[ast::TypeParam<'a>] = self
            .structs
            .get(receiver_type)
            .map_or(&[], |info| info.type_params);
        let effective = effective_receiver_params(struct_params, receiver_type_args);
        self.push_receiver_params(&effective);
        self.push_type_params(type_params);

        let explicit_ret = info.resolved_return.clone();
        let (body_expected_ret, final_ret) =
            self.function_return_context(is_async, explicit_ret.clone(), _span);

        self.push_value_scope();
        self.mutables.push(HashSet::new());

        // Bind the receiver name to the receiver type inside the method body.
        // A method on a generic struct sees the receiver as the struct
        // instantiated at its own type parameters: `fn (s Signal<T>) get(…)`
        // and the phase-1 `fn (s Signal) set<T>(…)` both bind `s` to
        // `Signal<T>`, so `s.value` has type `T` and obeys the
        // unbounded-parameter capability rule like any other `T`. A method
        // that declares no parameters still sees the struct's declared
        // parameters, as fresh opaque parameters.
        let receiver_binding_type = if let Some(ty) = self.opaques.get(receiver_type) {
            ty.clone()
        } else if let Some(info) = self.newtypes.get(receiver_type) {
            Type::Newtype {
                name: receiver_type,
                repr: info.repr,
            }
        } else if super::is_primitive_receiver_name(receiver_type) {
            Type::Named {
                name: receiver_type,
            }
        } else if let Some(struct_info) = self.structs.get(receiver_type) {
            if struct_info.type_params.is_empty() {
                Type::Struct {
                    name: receiver_type,
                }
            } else {
                let names: Vec<&'a str> = if !receiver_type_args.is_empty() {
                    receiver_type_args.iter().map(|p| p.name).collect()
                } else if !type_params.is_empty() {
                    type_params.iter().map(|p| p.name).collect()
                } else {
                    struct_info.type_params.iter().map(|p| p.name).collect()
                };
                Type::Generic {
                    base: receiver_type,
                    args: names
                        .into_iter()
                        .map(|name| Type::Param { name })
                        .collect(),
                }
            }
        } else {
            Type::Struct {
                name: receiver_type,
            }
        };
        if receiver_mutable {
            self.declare_mutable_var(receiver_name, receiver_binding_type);
        } else {
            self.declare_var(receiver_name, receiver_binding_type);
        }

        for (p, t) in params.iter().zip(param_types.iter()) {
            if let Some(default) = &p.default_value {
                let actual = self.check_exception_use(
                    default,
                    super::exceptions::Use::Value,
                    Some(t.clone()),
                );
                if !self.is_assignable(t, &actual) {
                    self.error_at_expr(
                        default,
                        format!("expected default type `{t}`, found type `{actual}`"),
                    );
                }
            }
            self.declare_var_class(p.name, t.clone(), Self::param_capture_class(t));
        }

        let saved_in_function = self.in_function;
        let saved_in_async = self.in_async_function;
        let saved_return_type = self.return_type.clone();
        let saved_catches = std::mem::take(&mut self.exception_catches);
        let hook_frame = self.push_hook_frame(Some(name), Self::is_component_return(&explicit_ret));
        self.in_function = true;
        self.in_async_function = is_async;
        self.return_type = body_expected_ret.clone();

        for stmt in body {
            self.check_statement(stmt);
        }

        let _body_called_hook = self.pop_hook_frame(hook_frame);
        self.in_function = saved_in_function;
        self.in_async_function = saved_in_async;
        self.return_type = saved_return_type;
        self.exception_catches = saved_catches;
        self.pop_value_scope();
        self.mutables.pop();

        self.pop_type_params();
        self.pop_type_params();

        // Update the stored signature, preserving the annotation resolutions
        // made during collection (deka#494).
        self.receiver_methods.insert(
            (receiver_type, name),
            super::MethodInfo {
                params,
                receiver_type_args,
                type_params,
                return_type: _return_type.map(|t| t.clone()),
                mutable: receiver_mutable,
                param_types: param_types.clone(),
                resolved_return: explicit_ret.clone(),
            },
        );

        let _ = final_ret; // signature already uses the declared return type
    }

    pub(super) fn check_return(&mut self, value: Option<&ast::Expr<'a>>, span: ast::Span) {
        if !self.in_function {
            self.error_span(span, "return outside of function");
            return;
        }

        let value_type = match value {
            Some(expr) => self.check_exception_use(
                expr,
                super::exceptions::Use::Return,
                self.return_type.clone(),
            ),
            None => Type::Generic {
                base: "Option",
                args: vec![Type::Never],
            },
        };

        if let Some(expected) = self.return_type.clone() {
            if !self.is_assignable(&expected, &value_type) {
                self.error_span(
                    span,
                    super::with_union_narrowing_hint(
                        format!("expected return type `{expected}`, found type `{value_type}`"),
                        &expected,
                        &value_type,
                    ),
                );
            }
        } else {
            self.return_type = Some(value_type);
        }
        self.hook_seen_return = true;
    }
}

/// Whether a declared return type obliges the body to produce a value.
///
/// `void` is the annotation that explicitly says "returns nothing", and a
/// `never` function is expected to diverge rather than return. Types that are
/// already broken or unresolved are skipped so a missing-return diagnostic
/// never stacks on top of the error that caused it.
fn return_type_requires_value(ty: &Type<'_>) -> bool {
    match ty {
        Type::Named { name: "void" } => false,
        Type::Never | Type::Error | Type::Infer | Type::Var => false,
        // An async function annotated `Promise<void>` is the async spelling of
        // the same "returns nothing" contract.
        Type::Generic {
            base: "Promise",
            args,
        } if args.len() == 1 => return_type_requires_value(&args[0]),
        _ => true,
    }
}

/// Whether a statement list returns on every path through it.
///
/// Deliberately conservative: it answers `true` only for shapes where the
/// return is certain. A construct it does not understand is treated as
/// falling through, which is the safe direction — the analysis can fail to
/// report a genuinely missing return, but it can never flag a function that
/// does return (deka#476).
fn body_always_returns(body: &[ast::Stmt<'_>]) -> bool {
    body.iter().any(stmt_always_returns)
}

fn stmt_always_returns(stmt: &ast::Stmt<'_>) -> bool {
    match stmt {
        ast::Stmt::Return { .. } => true,
        ast::Stmt::Expr {
            expr:
                ast::Expr::EnumConstructor {
                    enum_name: "Exception",
                    case_name: "Throw",
                    ..
                },
            ..
        } => true,
        ast::Stmt::Try {
            body, catch_body, ..
        } => body_always_returns(body) && body_always_returns(catch_body),
        ast::Stmt::Block { body, .. } => body_always_returns(body),
        // Only an `if` with an `else` where *both* sides return is a
        // guaranteed return; an `if` with no `else` always leaves a path that
        // falls through. `else_body` is an empty slice when there is no else.
        ast::Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            !else_body.is_empty()
                && body_always_returns(then_body)
                && body_always_returns(else_body)
        }
        // Loops are not treated as diverging even when they cannot exit: the
        // body may never run, and mis-reporting a real function is worse than
        // missing an exotic one.
        _ => false,
    }
}
