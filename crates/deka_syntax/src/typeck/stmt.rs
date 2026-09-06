//! Statement typechecking.

use std::collections::{HashMap, HashSet};
use super::expr::Coverage;

use crate::ast;

use super::types::Type;
use super::Checker;

impl<'a> Checker<'a> {
    pub(super) fn check_program(&mut self) {
        self.collect_declarations();
        self.collect_module_value_bindings();
        self.collect_function_signatures();
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
                } => self.check_function(name, type_params, params, return_type.as_ref(), body, *is_async, *span),
                ast::Stmt::ReceiverMethod {
                    receiver_type,
                    receiver_name,
                    receiver_mutable,
                    name,
                    params,
                    return_type,
                    body,
                    span,
                    is_async,
                    ..
                } => self.check_receiver_method(
                    receiver_type,
                    receiver_name,
                    *receiver_mutable,
                    name,
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
        // `Type` is the builtin first-class type descriptor (rfd#41,
        // deka#529). Reserving the name keeps user declarations from
        // colliding with the builtin in annotations; restriction-first,
        // relaxable later.
        const BUILTIN_TYPE_DIAGNOSTIC: &str = "`Type` is a builtin type";
        for stmt in self.program.statements {
            if let ast::Stmt::TypeAlias { name, value, span, .. } = stmt {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self.aliases.insert(name, value.clone()).is_some() {
                    self.error_span(*span, format!("duplicate type alias `{name}`"));
                }
            }
            if let ast::Stmt::Enum { name, cases, type_params, is_super, span } = stmt {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self
                    .enums
                    .insert(name, super::EnumInfo { cases, type_params, is_super: *is_super })
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
            if let ast::Stmt::Struct { name, fields, embeds, type_params, is_super, span, .. } = stmt {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self.structs.insert(name, super::StructInfo { fields, embeds, type_params, is_super: *is_super }).is_some() {
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
                            format!(
                                "Duplicate field '{}' in struct '{}'.",
                                field.name, name
                            ),
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
                            format!(
                                "Duplicate embedded struct '{}'",
                                embed.name
                            ),
                        );
                    }
                }
            }
            if let ast::Stmt::Interface { name, members, span, .. } = stmt {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self.interfaces.insert(name, super::InterfaceInfo { members, span: *span }).is_some() {
                    self.error_span(*span, format!("duplicate interface definition `{name}`"));
                }
            }
            if let ast::Stmt::Newtype { name, repr, span } = stmt {
                if *name == "Type" {
                    self.error_span(*span, BUILTIN_TYPE_DIAGNOSTIC);
                }
                if self.newtypes.insert(name, super::NewtypeInfo { repr: *repr }).is_some() {
                    self.error_span(*span, format!("duplicate newtype definition `{name}`"));
                    continue;
                }
                if self.aliases.contains_key(name) || self.structs.contains_key(name) || self.enums.contains_key(name) || self.interfaces.contains_key(name) {
                    self.error_span(*span, format!("`{name}` conflicts with an existing type declaration"));
                }
            }
        }

        // Super declarations (rfd#41, deka#561 PR B): after every type
        // declaration is collected, compute the transitive super marking and
        // build/validate descriptor trees so both this pass and later
        // `Name.type()` call sites see them.
        self.collect_super_declarations();
    }

    /// Eagerly resolve interface member types so that unknown types and other
    /// annotation errors are reported even when the interface is not used.
    fn validate_interface_declarations(&mut self) {
        let interfaces: Vec<_> = self.interfaces.iter().map(|(n, i)| (*n, i.clone())).collect();
        for (name, info) in interfaces {
            if info.members.is_empty() {
                self.error_span(
                    info.span,
                    format!("interface `{name}` must declare at least one member"),
                );
                continue;
            }
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
        }
    }

    pub(super) fn push_type_params(&mut self, type_params: &'a [ast::TypeParam<'a>]) {
        if type_params.is_empty() {
            return;
        }
        let mut scope = HashMap::new();
        for param in type_params {
            scope.insert(param.name, Type::Param { name: param.name });
        }
        self.type_scopes.push(scope);
    }

    pub(super) fn pop_type_params(&mut self) {
        self.type_scopes.pop();
    }

    fn collect_receiver_methods(&mut self) {
        for stmt in self.program.statements {
            if let ast::Stmt::ReceiverMethod {
                receiver_type,
                receiver_mutable,
                name,
                params,
                return_type,
                span,
                ..
            } = stmt
            {
                if !self.structs.contains_key(receiver_type)
                    && !self.newtypes.contains_key(receiver_type)
                    && !super::is_primitive_receiver_name(receiver_type)
                {
                    self.error_span(*span, format!("unknown receiver type `{receiver_type}`"));
                    continue;
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
                if self.receiver_methods.insert(key, super::MethodInfo {
                    params,
                    return_type: return_type.clone(),
                    mutable: *receiver_mutable,
                    param_types,
                    resolved_return,
                }).is_some()
                {
                    self.error_span(*span, format!(
                        "duplicate receiver method `{name}` on type `{receiver_type}`"
                    ));
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
            let (name, ty, mutable) = match stmt {
                ast::Stmt::Const { name, ty, .. } => (*name, ty.as_ref(), false),
                ast::Stmt::Let { name, ty, .. } => (*name, ty.as_ref(), true),
                ast::Stmt::Export {
                    decl: ast::ExportDecl::Const { name, ty, .. },
                    ..
                } => (*name, ty.as_ref(), false),
                _ => continue,
            };
            let seed = ty
                .map(|annot| self.resolve_ast_type(annot))
                .unwrap_or(Type::Infer);
            self.scopes[0].insert(name, seed);
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
                    } if return_type.is_none() => {
                        Some((*name, *type_params, *params, *body, *is_async, *span))
                    }
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
                    } if return_type.is_none() => {
                        Some((*name, *type_params, *params, *body, *is_async, *span))
                    }
                    _ => None,
                };
                if let Some((name, type_params, params, body, is_async, span)) = info {
                    pending.push((name, span));
                    let prev = self.globals.get(name).cloned();
                    self.check_function(
                        name,
                        type_params,
                        params,
                        None,
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
                if let Some(Type::Function { ret, .. }) = self.globals.get(name) {
                    if matches!(**ret, Type::Infer) {
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
        self.reset_lowering_state();
    }

    pub(super) fn check_statement(&mut self, stmt: &ast::Stmt<'a>) {
        match stmt {
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
                self.check_function(name, type_params, params, return_type.as_ref(), body, *is_async, *span);
            }
            ast::Stmt::Export { decl, .. } => match decl {
                ast::ExportDecl::Const {
                    name,
                    ty,
                    value,
                } => {
                    self.check_binding(name, ty.as_ref(), value, false, value.span());
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
                self.check_expr(expr);
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
                self.scopes.push(HashMap::new());
                self.mutables.push(HashSet::new());
                for s in then_body.iter() {
                    self.check_statement(s);
                }
                self.scopes.pop();
                self.mutables.pop();
                self.scopes.push(HashMap::new());
                self.mutables.push(HashSet::new());
                for s in else_body.iter() {
                    self.check_statement(s);
                }
                self.scopes.pop();
                self.mutables.pop();
            }
            ast::Stmt::Block { body, .. } => {
                self.scopes.push(HashMap::new());
                self.mutables.push(HashSet::new());
                for s in body.iter() {
                    self.check_statement(s);
                }
                self.scopes.pop();
                self.mutables.pop();
            }
            ast::Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                self.scopes.push(HashMap::new());
                self.mutables.push(HashSet::new());
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
                self.loop_depth += 1;
                for s in body.iter() {
                    self.check_statement(s);
                }
                self.loop_depth -= 1;
                self.scopes.pop();
                self.mutables.pop();
            }
            ast::Stmt::ForOf {
                name,
                is_const,
                iterable,
                body,
                ..
            } => {
                let iterable_type = self.check_expr(iterable);
                let element_type = iterable_type.collection_element();
                self.scopes.push(HashMap::new());
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
                self.scopes.pop();
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
            ast::Stmt::Enum { name: _, cases, type_params, is_super: _, span: _ } => {
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
            | ast::Stmt::Newtype { .. }
            | ast::Stmt::Interface { .. }
            | ast::Stmt::ReceiverMethod { .. } => {
                // Already collected and validated lazily at use sites (or no-op).
            }
            ast::Stmt::Import { specifiers, .. } => {
                // Without a resolved module graph, imported bindings are treated
                // as externally provided. They are assigned the infer sentinel
                // so uses of them typecheck generically; a real module resolver
                // will supply concrete types later.
                //
                // When a module graph has already seeded concrete types for this
                // import source (via `Checker::seed_imports`), do not overwrite
                // them with the Infer placeholder.
                for spec in specifiers.iter() {
                    let already_known = self.scopes.first().map_or(false, |scope| scope.contains_key(spec.local));
                    if !already_known {
                        self.declare_var(spec.local, Type::Infer);
                    }
                }
            }
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
            self.check_function(name, type_params, params, return_type.as_ref(), body, *is_async, *span);
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
            Type::Generic { base: "Result", args } if args.len() == 2 => args[0].clone(),
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
            if !self.is_assignable(declared, &bound)
                && !matches!(bound, Type::Infer | Type::Error)
            {
                self.error_span(
                    span,
                    format!("binding declared as `{declared}`, but `unwrap` yields `{bound}`"),
                );
            }
        }

        // The alternative is checked in its own scope, then the binding is
        // declared -- it cannot see the name it is providing.
        self.scopes.push(HashMap::new());
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
        self.scopes.pop();

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
            self.scopes.push(HashMap::new());
            self.mutables.push(HashSet::new());
            self.check_pattern(&arm.pattern, scrutinee_type);
            let arm_type = self.check_expr(&arm.body);
            self.mutables.pop();
            self.scopes.pop();

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
        _span: ast::Span,
    ) {
        let value_type = self.check_expr(value);
        let final_type = if let Some(annot) = ty {
            let expected = self.resolve_ast_type(annot);
            if let Type::Option { inner } = &expected {
                // Explicit `Option<T>` bindings must be initialized with
                // `Some(...)` or `none`; the struct-field sugar that accepts a
                // concrete `T` does not apply here.
                if !value_type.is_error()
                    && !matches!(
                        value_type,
                        Type::Option { .. } | Type::None | Type::Infer
                    )
                {
                    self.error_at_expr(
                        value,
                        format!(
                            "`{name}` is declared Option<{inner}> but the initializer is {value_type}"
                        ),
                    );
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
            expected
        } else {
            value_type
        };
        if mutable {
            self.declare_mutable_var(name, final_type);
        } else {
            self.declare_var(name, final_type);
        }
        // A declaration at module scope activates its seed for module-level
        // lookups; nested declarations never touch module pending state.
        if self.scopes.len() == 1 {
            self.pending_module_bindings.remove(name);
        }
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
        // Use the previously collected signature for parameter types so that
        // errors about missing annotations are reported exactly once.
        let (param_types, collected_ret, optional) = match self.globals.get(name).cloned() {
            Some(Type::Function { params, ret, optional }) => (params, Some(*ret), optional),
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

        self.scopes.push(HashMap::new());
        self.mutables.push(HashSet::new());

        // Make the function available to its own body for recursion. Use the
        // signature collected earlier; the return type will be refined after
        // the body is checked.
        let self_type = Type::Function {
            params: param_types.clone(),
            ret: Box::new(final_ret.clone()),
            optional,
        };
        self.declare_var(name, self_type);

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
                self.return_type
                    .take()
                    .unwrap_or(Type::Generic {
                        base: "Option",
                        args: vec![Type::Never],
                    })
            })
        };

        self.in_function = saved_in_function;
        self.return_type = saved_return_type;
        self.scopes.pop();
        self.mutables.pop();

        self.pop_type_params();

        // Update the global function type with the final (possibly inferred)
        // return type.
        self.globals.insert(
            name,
            Type::Function {
                params: param_types,
                ret: Box::new(final_ret),
                optional,
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
            Some(Type::Generic { base: "Promise", args }) if args.len() == 1 => {
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
        receiver_name: &'a str,
        receiver_mutable: bool,
        name: &'a str,
        params: &'a [ast::Param<'a>],
        // The annotation itself is no longer read here — its resolved form
        // comes from the MethodInfo collected earlier (deka#494).
        _return_type: Option<&ast::Type<'a>>,
        body: &'a [ast::Stmt<'a>],
        is_async: bool,
        _span: ast::Span,
    ) {
        if receiver_mutable && self.newtypes.contains_key(receiver_type) {
            self.error_span(
                _span,
                format!(
                    "mutable receiver methods are not allowed on newtype `{receiver_type}`"
                ),
            );
        }
        if receiver_mutable && super::is_primitive_receiver_name(receiver_type) {
            // Primitives are immutable values; there is no mutable location
            // to receive (deka#527).
            self.error_span(
                _span,
                format!(
                    "mutable receiver methods are not allowed on primitive `{receiver_type}`"
                ),
            );
        }

        let info = match self.receiver_methods.get(&(receiver_type, name)) {
            Some(i) => i.clone(),
            None => return,
        };

        // Annotations were resolved during collection (deka#494); reuse them
        // so unknown types are reported exactly once.
        let param_types = info.param_types.clone();

        let explicit_ret = info.resolved_return.clone();
        let (body_expected_ret, final_ret) =
            self.function_return_context(is_async, explicit_ret.clone(), _span);

        self.scopes.push(HashMap::new());
        self.mutables.push(HashSet::new());

        // Bind the receiver name to the receiver type inside the method body.
        let receiver_binding_type = if let Some(info) = self.newtypes.get(receiver_type) {
            Type::Newtype {
                name: receiver_type,
                repr: info.repr,
            }
        } else if super::is_primitive_receiver_name(receiver_type) {
            Type::Named { name: receiver_type }
        } else {
            Type::Struct { name: receiver_type }
        };
        if receiver_mutable {
            self.declare_mutable_var(receiver_name, receiver_binding_type);
        } else {
            self.declare_var(receiver_name, receiver_binding_type);
        }

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

        self.in_function = saved_in_function;
        self.in_async_function = saved_in_async;
        self.return_type = saved_return_type;
        self.scopes.pop();
        self.mutables.pop();

        // Update the stored signature, preserving the annotation resolutions
        // made during collection (deka#494).
        self.receiver_methods.insert(
            (receiver_type, name),
            super::MethodInfo {
                params,
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
            Some(expr) => self.check_expr(expr),
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
        Type::Generic { base: "Promise", args } if args.len() == 1 => {
            return_type_requires_value(&args[0])
        }
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
