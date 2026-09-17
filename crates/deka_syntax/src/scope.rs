//! Names visible at a byte offset in a parsed program.
//!
//! This is the scope query behind LSP completion and hover: given the parsed
//! AST and a cursor offset, it answers "which locals, params, top-level items
//! and imported names are in scope here" so every consumer (native LSP, wasm
//! worker) shares one scope implementation instead of re-scanning source text.
//!
//! [`names_in_scope_at_offset`] returns the bare binding set for completion;
//! [`declarations_in_scope_at_offset`] additionally carries each binding's
//! declaration span and its declaration rendered as source (e.g.
//! `fn greeting(name: string) string`), which is what hover shows.
//!
//! Position semantics mirror the typechecker: module-level functions and type
//! declarations are visible throughout the module, `const`/`let` bindings are
//! visible after their declaration within the enclosing block, and params,
//! loop bindings, catch bindings and match-pattern bindings are visible inside
//! their construct's body.

use crate::ast::{
    Expr, ExportDecl, ForInit, MatchArm, NewtypeRepr, Param, ParamBinding, Pattern, Program, Span,
    Stmt, Type, TypeParam, UnwrapAlternative,
};
use std::collections::HashSet;

/// What kind of binding a [`ScopeItem`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeItemKind {
    Function,
    Const,
    Variable,
    Param,
    Import,
    Struct,
    Enum,
    /// Type aliases, newtypes, interfaces, opaque handles.
    Type,
}

/// One name in scope, borrowed from the program's arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeItem<'a> {
    pub name: &'a str,
    pub kind: ScopeItemKind,
    /// Byte span of the declaration node — the statement, parameter, import
    /// specifier or pattern that introduces the binding — so consumers can
    /// resolve a use back to its declaration.
    pub span: Span,
}

/// A name in scope together with its declaration rendered as source, e.g.
/// `fn greeting(name: string) string` or `(parameter) name: string`. This is
/// the hover payload; [`ScopeItem`] is the completion payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeDeclaration<'a> {
    pub name: &'a str,
    pub kind: ScopeItemKind,
    pub span: Span,
    pub detail: String,
}

/// Every name visible at byte `offset` into the source `program` was parsed
/// from. Offsets past the end answer with the end-of-file scope.
pub fn names_in_scope_at_offset<'a>(
    program: &'a Program<'a>,
    offset: usize,
) -> Vec<ScopeItem<'a>> {
    declarations_in_scope_at_offset(program, offset)
        .into_iter()
        .map(|decl| ScopeItem {
            name: decl.name,
            kind: decl.kind,
            span: decl.span,
        })
        .collect()
}

/// Every name visible at byte `offset`, each with its declaration span and
/// rendered declaration detail.
pub fn declarations_in_scope_at_offset<'a>(
    program: &'a Program<'a>,
    offset: usize,
) -> Vec<ScopeDeclaration<'a>> {
    let mut collector = Collector::default();
    for stmt in program.statements.iter() {
        collect_module_item(stmt, &mut collector);
    }
    descend_stmts(program.statements, offset, &mut collector);
    collector.items
}

/// True when byte `offset` sits inside a JSX opening tag name (between the
/// `<` and the end of the tag identifier) or directly on the `<` that opens
/// an element or fragment — the positions where completing a component name
/// makes sense. Returns the tag prefix typed so far (the source between `<`
/// and the offset, identifier characters only).
pub fn jsx_tag_prefix_at<'p, 's>(
    program: &'p Program<'p>,
    source: &'s str,
    offset: usize,
) -> Option<&'s str> {
    let mut found: Option<(usize, usize)> = None;
    for stmt in program.statements.iter() {
        crate::visit::walk_stmt(stmt, &mut |expr| {
            let span = expr.span();
            match expr {
                Expr::JsxElement { element, .. } => {
                    let tag_len = element.tag.len();
                    let name_start = span.byte_start + 1; // skip `<`
                    let name_end = name_start + tag_len;
                    if offset >= span.byte_start && offset <= name_end {
                        found = Some((name_start, offset.clamp(name_start, name_end)));
                    }
                }
                // `<>` opens a fragment; only the bare `<` position counts.
                Expr::JsxFragment { .. }
                    if offset == span.byte_start || offset == span.byte_start + 1 =>
                {
                    found = Some((span.byte_start + 1, offset));
                }
                _ => {}
            }
        });
        if found.is_some() {
            break;
        }
    }
    let (start, end) = found?;
    if start > end || end > source.len() {
        return None;
    }
    let prefix = &source[start..end];
    if prefix
        .chars()
        .all(|ch| ch == '_' || ch == '.' || ch.is_ascii_alphanumeric())
    {
        Some(prefix)
    } else {
        None
    }
}

#[derive(Default)]
struct Collector<'a> {
    items: Vec<ScopeDeclaration<'a>>,
    seen: HashSet<&'a str>,
}

impl<'a> Collector<'a> {
    fn push(&mut self, name: &'a str, kind: ScopeItemKind, span: Span, detail: String) {
        if name.is_empty() || !self.seen.insert(name) {
            return;
        }
        self.items.push(ScopeDeclaration {
            name,
            kind,
            span,
            detail,
        });
    }

    fn push_params(&mut self, params: &'a [Param<'a>]) {
        for param in params {
            match &param.binding {
                ParamBinding::Identifier(name) => {
                    let detail = match &param.ty {
                        Some(ty) => format!("(parameter) {name}: {}", render_type(ty)),
                        None => format!("(parameter) {name}"),
                    };
                    self.push(name, ScopeItemKind::Param, param.span, detail);
                }
                ParamBinding::Tuple(names) => {
                    for name in names.iter() {
                        self.push(
                            name,
                            ScopeItemKind::Param,
                            param.span,
                            format!("(parameter) {name}"),
                        );
                    }
                }
            }
        }
    }
}

/// Render a type annotation the way it is spelled in source.
fn render_type(ty: &Type<'_>) -> String {
    match ty {
        Type::Named { name, .. } => (*name).to_string(),
        Type::Generic { base, args, .. } => format!(
            "{base}<{}>",
            args.iter()
                .map(render_type)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Function { params, ret, .. } => format!(
            "fn({}) {}",
            params.iter().map(render_type).collect::<Vec<_>>().join(", "),
            render_type(ret)
        ),
        Type::Option { inner, .. } => format!("Option<{}>", render_type(inner)),
        Type::Tuple { elements, .. } => format!(
            "[{}]",
            elements
                .iter()
                .map(render_type)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Record { fields, .. } => format!(
            "{{ {} }}",
            fields
                .iter()
                .map(|field| format!("{}: {}", field.name, render_type(&field.ty)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Union { members, .. } => members
            .iter()
            .map(render_type)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

fn render_type_params(type_params: &[TypeParam<'_>]) -> String {
    if type_params.is_empty() {
        return String::new();
    }
    let rendered = type_params
        .iter()
        .map(|param| match &param.bound {
            Some(bound) => format!("{}: {}", param.name, render_type(bound)),
            None => param.name.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("<{rendered}>")
}

/// `fn greeting(name: string) string` — a function declaration's signature
/// line, without the body.
fn function_signature(
    name: &str,
    type_params: &[TypeParam<'_>],
    params: &[Param<'_>],
    return_type: Option<&Type<'_>>,
    is_async: bool,
) -> String {
    let rendered_params = params
        .iter()
        .map(|param| match &param.ty {
            Some(ty) => format!("{}: {}", param.binding, render_type(ty)),
            None => param.binding.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut signature = String::new();
    if is_async {
        signature.push_str("async ");
    }
    signature.push_str("fn ");
    signature.push_str(name);
    signature.push_str(&render_type_params(type_params));
    signature.push('(');
    signature.push_str(&rendered_params);
    signature.push(')');
    if let Some(return_type) = return_type {
        signature.push(' ');
        signature.push_str(&render_type(return_type));
    }
    signature
}

/// `const total: int` — a binding declaration line, with the type annotation
/// when the declaration carries one.
fn binding_detail(keyword: &str, name: &str, ty: Option<&Type<'_>>) -> String {
    match ty {
        Some(ty) => format!("{keyword} {name}: {}", render_type(ty)),
        None => format!("{keyword} {name}"),
    }
}

/// The value-declaring statements (`fn`/`const`/`let` and their tuple and
/// unwrap forms) share one collection path at module level and inside blocks.
fn push_value_decl<'a>(stmt: &'a Stmt<'a>, collector: &mut Collector<'a>) {
    match stmt {
        Stmt::Function {
            name,
            type_params,
            params,
            return_type,
            is_async,
            span,
            ..
        } => collector.push(
            name,
            ScopeItemKind::Function,
            *span,
            function_signature(name, type_params, params, return_type.as_ref(), *is_async),
        ),
        Stmt::Const {
            name, ty, span, ..
        } => collector.push(
            name,
            ScopeItemKind::Const,
            *span,
            binding_detail("const", name, ty.as_ref()),
        ),
        Stmt::Let {
            name, ty, span, ..
        } => collector.push(
            name,
            ScopeItemKind::Variable,
            *span,
            binding_detail("let", name, ty.as_ref()),
        ),
        Stmt::TupleBinding {
            names,
            is_const,
            span,
            ..
        } => {
            let (kind, keyword) = if *is_const {
                (ScopeItemKind::Const, "const")
            } else {
                (ScopeItemKind::Variable, "let")
            };
            for name in names.iter() {
                collector.push(name, kind, *span, format!("{keyword} {name}"));
            }
        }
        Stmt::UnwrapLet {
            name,
            ty,
            is_const,
            span,
            ..
        } => collector.push(
            name,
            if *is_const {
                ScopeItemKind::Const
            } else {
                ScopeItemKind::Variable
            },
            *span,
            binding_detail(if *is_const { "const" } else { "let" }, name, ty.as_ref()),
        ),
        _ => {}
    }
}

/// Module-level items are visible everywhere in the module (the typechecker
/// rejects forward *references* at module scope but resolves every module
/// binding before checking function bodies; for completion the whole module
/// surface is the useful answer).
fn collect_module_item<'a>(stmt: &'a Stmt<'a>, collector: &mut Collector<'a>) {
    match stmt {
        Stmt::Function { .. }
        | Stmt::Const { .. }
        | Stmt::Let { .. }
        | Stmt::TupleBinding { .. }
        | Stmt::UnwrapLet { .. } => push_value_decl(stmt, collector),
        Stmt::ReceiverMethod { .. } => {}
        Stmt::Import {
            specifiers, source, ..
        } => {
            for spec in specifiers.iter() {
                let type_prefix = if spec.is_type_only { "type " } else { "" };
                let detail = if spec.imported == spec.local {
                    format!("import {{ {type_prefix}{} }} from '{source}'", spec.imported)
                } else {
                    format!(
                        "import {{ {type_prefix}{} as {} }} from '{source}'",
                        spec.imported, spec.local
                    )
                };
                collector.push(spec.local, ScopeItemKind::Import, spec.span, detail);
            }
        }
        Stmt::Export { decl, span } => match decl {
            ExportDecl::Const { name, ty, .. } => collector.push(
                name,
                ScopeItemKind::Const,
                *span,
                binding_detail("const", name, ty.as_ref()),
            ),
            ExportDecl::Function {
                name,
                type_params,
                params,
                return_type,
                is_async,
                ..
            } => collector.push(
                name,
                ScopeItemKind::Function,
                *span,
                function_signature(name, type_params, params, return_type.as_ref(), *is_async),
            ),
            // `export { a, b }` re-exports names already declared (and thus
            // already collected); `export { a } from "./m"` names live in the
            // other module.
            ExportDecl::NamedGroup { .. } => {}
        },
        Stmt::Struct {
            name,
            type_params,
            span,
            ..
        } => collector.push(
            name,
            ScopeItemKind::Struct,
            *span,
            format!("struct {name}{}", render_type_params(type_params)),
        ),
        Stmt::Enum {
            name,
            type_params,
            span,
            ..
        } => collector.push(
            name,
            ScopeItemKind::Enum,
            *span,
            format!("enum {name}{}", render_type_params(type_params)),
        ),
        Stmt::TypeAlias {
            name,
            type_params,
            value,
            span,
        } => collector.push(
            name,
            ScopeItemKind::Type,
            *span,
            format!(
                "type {name}{} = {}",
                render_type_params(type_params),
                render_type(value)
            ),
        ),
        Stmt::Newtype { name, repr, span } => {
            let repr = match repr {
                NewtypeRepr::Number => "number",
                NewtypeRepr::String => "string",
                NewtypeRepr::Bool => "bool",
            };
            collector.push(
                name,
                ScopeItemKind::Type,
                *span,
                format!("type {name} {repr}"),
            );
        }
        Stmt::Interface {
            name,
            type_params,
            span,
            ..
        } => collector.push(
            name,
            ScopeItemKind::Type,
            *span,
            format!("interface {name}{}", render_type_params(type_params)),
        ),
        Stmt::Opaque { name, span } => {
            collector.push(name, ScopeItemKind::Type, *span, format!("opaque {name}"));
        }
        Stmt::Summon { functions, .. } => {
            for function in functions.iter() {
                collector.push(
                    function.name,
                    ScopeItemKind::Function,
                    function.span,
                    function_signature(
                        function.name,
                        &[],
                        function.params,
                        Some(&function.return_type),
                        false,
                    ),
                );
            }
        }
        _ => {}
    }
}

fn contains(span: crate::ast::Span, offset: usize) -> bool {
    span.byte_start <= offset && offset <= span.byte_end
}

/// Walk a block: collect the locals declared before the offset at this level
/// (functions hoist within their block, other bindings are positional), then
/// descend into the one statement whose span holds the offset.
fn descend_stmts<'a>(stmts: &'a [Stmt<'a>], offset: usize, collector: &mut Collector<'a>) {
    for stmt in stmts.iter() {
        if contains(stmt_span(stmt), offset) {
            descend_stmt(stmt, offset, collector);
            continue;
        }
        if stmt_span(stmt).byte_end > offset {
            continue;
        }
        push_value_decl(stmt, collector);
    }
}

fn stmt_span(stmt: &Stmt<'_>) -> crate::ast::Span {
    match stmt {
        Stmt::Opaque { span, .. }
        | Stmt::Summon { span, .. }
        | Stmt::BridgeDecl { span, .. }
        | Stmt::Export { span, .. }
        | Stmt::Import { span, .. }
        | Stmt::Const { span, .. }
        | Stmt::TupleBinding { span, .. }
        | Stmt::Let { span, .. }
        | Stmt::UnwrapLet { span, .. }
        | Stmt::Function { span, .. }
        | Stmt::ReceiverMethod { span, .. }
        | Stmt::Struct { span, .. }
        | Stmt::Enum { span, .. }
        | Stmt::TypeAlias { span, .. }
        | Stmt::Newtype { span, .. }
        | Stmt::Interface { span, .. }
        | Stmt::Expr { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::If { span, .. }
        | Stmt::Try { span, .. }
        | Stmt::Block { span, .. }
        | Stmt::Empty { span }
        | Stmt::For { span, .. }
        | Stmt::ForOf { span, .. }
        | Stmt::Break { span }
        | Stmt::Continue { span } => *span,
    }
}

fn descend_stmt<'a>(stmt: &'a Stmt<'a>, offset: usize, collector: &mut Collector<'a>) {
    match stmt {
        Stmt::Function { params, body, .. } => {
            collector.push_params(params);
            descend_stmts(body, offset, collector);
        }
        Stmt::ReceiverMethod {
            receiver_type,
            receiver_type_args,
            receiver_name,
            params,
            body,
            span,
            ..
        } => {
            let type_args = if receiver_type_args.is_empty() {
                String::new()
            } else {
                format!(
                    "<{}>",
                    receiver_type_args
                        .iter()
                        .map(|arg| arg.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            collector.push(
                receiver_name,
                ScopeItemKind::Variable,
                *span,
                format!("(receiver) {receiver_name}: {receiver_type}{type_args}"),
            );
            collector.push_params(params);
            descend_stmts(body, offset, collector);
        }
        Stmt::Export { decl, .. } => match decl {
            ExportDecl::Function { params, body, .. } => {
                collector.push_params(params);
                descend_stmts(body, offset, collector);
            }
            ExportDecl::Const { value, .. } => descend_expr(value, offset, collector),
            ExportDecl::NamedGroup { .. } => {}
        },
        Stmt::Block { body, .. } => descend_stmts(body, offset, collector),
        Stmt::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            descend_expr(condition, offset, collector);
            descend_stmts(then_body, offset, collector);
            descend_stmts(else_body, offset, collector);
        }
        Stmt::Try {
            body,
            catch_name,
            catch_type,
            catch_body,
            span,
            ..
        } => {
            descend_stmts(body, offset, collector);
            if catch_body.iter().any(|s| contains(stmt_span(s), offset)) {
                let detail = match catch_type {
                    Some(ty) => format!("(variable) {catch_name}: {}", render_type(ty)),
                    None => format!("(variable) {catch_name}"),
                };
                collector.push(catch_name, ScopeItemKind::Variable, *span, detail);
                descend_stmts(catch_body, offset, collector);
            }
        }
        Stmt::For {
            init,
            condition,
            step,
            body,
            span,
            ..
        } => {
            if let Some(init) = init {
                match init {
                    ForInit::Const { name, value } => {
                        collector.push(
                            name,
                            ScopeItemKind::Const,
                            *span,
                            format!("const {name}"),
                        );
                        descend_expr(value, offset, collector);
                    }
                    ForInit::Let { name, value } => {
                        collector.push(name, ScopeItemKind::Variable, *span, format!("let {name}"));
                        descend_expr(value, offset, collector);
                    }
                    ForInit::Expr(expr) => descend_expr(expr, offset, collector),
                }
            }
            if let Some(condition) = condition {
                descend_expr(condition, offset, collector);
            }
            if let Some(step) = step {
                descend_expr(step, offset, collector);
            }
            descend_stmts(body, offset, collector);
        }
        Stmt::ForOf {
            name,
            iterable,
            body,
            span,
            ..
        } => {
            descend_expr(iterable, offset, collector);
            collector.push(
                name,
                ScopeItemKind::Variable,
                *span,
                format!("(variable) {name}"),
            );
            descend_stmts(body, offset, collector);
        }
        Stmt::Const { value, .. } | Stmt::Let { value, .. } => {
            descend_expr(value, offset, collector)
        }
        Stmt::TupleBinding { value, .. } => descend_expr(value, offset, collector),
        Stmt::UnwrapLet {
            scrutinee,
            alternative,
            ..
        } => {
            descend_expr(scrutinee, offset, collector);
            match alternative {
                UnwrapAlternative::Block(body) => descend_stmts(body, offset, collector),
                UnwrapAlternative::Match(arms) => descend_arms(arms, offset, collector),
            }
        }
        Stmt::Expr { expr, .. } => descend_expr(expr, offset, collector),
        Stmt::Return {
            value: Some(value),
            ..
        } => descend_expr(value, offset, collector),
        _ => {}
    }
}

fn descend_arms<'a>(arms: &'a [MatchArm<'a>], offset: usize, collector: &mut Collector<'a>) {
    for arm in arms.iter() {
        if !contains(arm.span, offset) {
            continue;
        }
        collect_pattern_names(&arm.pattern, collector);
        if let Some(guard) = &arm.guard {
            descend_expr(guard, offset, collector);
        }
        descend_expr(&arm.body, offset, collector);
    }
}

fn collect_pattern_names<'a>(pattern: &'a Pattern<'a>, collector: &mut Collector<'a>) {
    match pattern {
        Pattern::Identifier { name, span } => collector.push(
            name,
            ScopeItemKind::Variable,
            *span,
            format!("(variable) {name}"),
        ),
        Pattern::Constructor {
            payload: Some(payload),
            ..
        } => collect_pattern_names(payload, collector),
        Pattern::Struct { fields, .. } => {
            for field in fields.iter() {
                collect_pattern_names(&field.pattern, collector);
            }
        }
        Pattern::Tuple { elements, .. } => {
            for element in elements.iter() {
                collect_pattern_names(element, collector);
            }
        }
        _ => {}
    }
}

/// Descend into the expression subtree that holds the offset, collecting
/// closure params and match-arm bindings on the way in.
fn descend_expr<'a>(expr: &'a Expr<'a>, offset: usize, collector: &mut Collector<'a>) {
    if !contains(expr.span(), offset) {
        return;
    }
    match expr {
        Expr::Function { params, body, .. } => {
            collector.push_params(params);
            descend_stmts(body, offset, collector);
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            descend_expr(scrutinee, offset, collector);
            descend_arms(arms, offset, collector);
        }
        Expr::Build { body, .. } => descend_stmts(body, offset, collector),
        Expr::Binary { left, right, .. } => {
            descend_expr(left, offset, collector);
            descend_expr(right, offset, collector);
        }
        Expr::Unary { operand, .. } => descend_expr(operand, offset, collector),
        Expr::Call { callee, args, .. } => {
            descend_expr(callee, offset, collector);
            for arg in args.iter() {
                descend_expr(arg, offset, collector);
            }
        }
        Expr::FieldAccess { object, .. } => descend_expr(object, offset, collector),
        Expr::IndexAccess { object, index, .. } => {
            descend_expr(object, offset, collector);
            descend_expr(index, offset, collector);
        }
        Expr::StructLiteral { fields, .. } => {
            for field in fields.iter() {
                descend_expr(&field.value, offset, collector);
            }
        }
        Expr::EnumConstructor {
            payload: Some(payload),
            ..
        } => descend_expr(payload, offset, collector),
        Expr::Safe { expr, .. } => descend_expr(expr, offset, collector),
        Expr::Bridge { args, .. } => {
            for arg in args.iter() {
                descend_expr(arg, offset, collector);
            }
        }
        Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            descend_expr(condition, offset, collector);
            descend_expr(then_branch, offset, collector);
            descend_expr(else_branch, offset, collector);
        }
        Expr::Await { expr, .. } => descend_expr(expr, offset, collector),
        Expr::JsxElement { element, .. } => {
            for attribute in element.attributes.iter() {
                if let Some(value) = &attribute.value {
                    descend_expr(value, offset, collector);
                }
            }
            for child in element.children.iter() {
                descend_expr(child, offset, collector);
            }
        }
        Expr::JsxFragment { children, .. } => {
            for child in children.iter() {
                descend_expr(child, offset, collector);
            }
        }
        Expr::Array { elements, .. } => {
            for element in elements.iter() {
                descend_expr(element, offset, collector);
            }
        }
        Expr::Object { fields, .. } => {
            for field in fields.iter() {
                descend_expr(&field.value, offset, collector);
            }
        }
        Expr::Spread { expr, .. } => descend_expr(expr, offset, collector),
        Expr::Paren { expr, .. } => descend_expr(expr, offset, collector),
        Expr::TemplateLiteral { parts, .. } => {
            for part in parts.iter() {
                if let crate::ast::TemplatePart::Expr(expr) = part {
                    descend_expr(expr, offset, collector);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bumpalo::Bump;

    fn scope_names(source: &str, marker: &str) -> Vec<(String, ScopeItemKind)> {
        let offset = source.find(marker).expect("marker present");
        let arena = Bump::new();
        let program = crate::parse(source, &arena)
            .program
            .expect("fixture parses");
        names_in_scope_at_offset(&program, offset)
            .into_iter()
            .map(|item| (item.name.to_string(), item.kind))
            .collect()
    }

    fn scope_declarations(
        source: &str,
        marker: &str,
    ) -> Vec<(String, ScopeItemKind, Span, String)> {
        let offset = source.find(marker).expect("marker present");
        let arena = Bump::new();
        let program = crate::parse(source, &arena)
            .program
            .expect("fixture parses");
        declarations_in_scope_at_offset(&program, offset)
            .into_iter()
            .map(|decl| (decl.name.to_string(), decl.kind, decl.span, decl.detail))
            .collect()
    }

    #[test]
    fn function_body_sees_params_locals_and_module_items() {
        let source = "import { query } from 'db'\n\
const VERSION = 1;\n\
fn helper() {}\n\
fn greeting(name: string) string {\n\
    const shout = name;\n\
    return /*cursor*/ shout;\n\
}\n";
        let names = scope_names(source, "/*cursor*/");
        let has = |needle: &str, kind: ScopeItemKind| {
            names.iter().any(|(n, k)| n == needle && *k == kind)
        };
        assert!(has("name", ScopeItemKind::Param), "names={names:?}");
        assert!(has("shout", ScopeItemKind::Const), "names={names:?}");
        assert!(has("greeting", ScopeItemKind::Function), "names={names:?}");
        assert!(has("helper", ScopeItemKind::Function), "names={names:?}");
        assert!(has("VERSION", ScopeItemKind::Const), "names={names:?}");
        assert!(has("query", ScopeItemKind::Import), "names={names:?}");
    }

    #[test]
    fn declarations_carry_signature_detail_and_declaration_span() {
        let source = "import { query } from 'db'\n\
const VERSION: int = 1;\n\
fn greeting(name: string) string {\n\
    const shout = name;\n\
    return /*cursor*/ shout;\n\
}\n";
        let decls = scope_declarations(source, "/*cursor*/");
        let detail = |needle: &str| {
            decls.iter()
                .find(|(name, _, _, _)| name == needle)
                .unwrap_or_else(|| panic!("{needle} in scope: {decls:?}"))
                .3
                .clone()
        };
        assert_eq!(detail("greeting"), "fn greeting(name: string) string");
        assert_eq!(detail("name"), "(parameter) name: string");
        assert_eq!(detail("shout"), "const shout");
        assert_eq!(detail("VERSION"), "const VERSION: int");
        assert_eq!(detail("query"), "import { query } from 'db'");
    }

    #[test]
    fn declaration_spans_point_at_the_declaring_node() {
        let source = "fn greeting(name: string) string {\n    return /*cursor*/ name;\n}\n";
        let decls = scope_declarations(source, "/*cursor*/");
        let greeting = decls
            .iter()
            .find(|(name, _, _, _)| name == "greeting")
            .expect("greeting in scope");
        assert_eq!(
            &source[greeting.2.byte_start..greeting.2.byte_start + "fn greeting".len()],
            "fn greeting",
            "the function's declaration span starts at its declaration"
        );
        let name = decls
            .iter()
            .find(|(name, _, _, _)| name == "name")
            .expect("name in scope");
        assert_eq!(
            &source[name.2.byte_start..name.2.byte_end],
            "name: string",
            "the parameter's span covers its declaration"
        );
    }

    #[test]
    fn locals_after_the_cursor_are_not_in_scope() {
        let source = "fn f() {\n    /*cursor*/\n    const later = 1;\n}\n";
        let names = scope_names(source, "/*cursor*/");
        assert!(
            !names.iter().any(|(n, _)| n == "later"),
            "later must not leak above its declaration: {names:?}"
        );
    }

    #[test]
    fn block_locals_do_not_leak_outward() {
        let source = "fn f() {\n    if (true) {\n        const inner = 1;\n    }\n    /*cursor*/\n}\n";
        let names = scope_names(source, "/*cursor*/");
        assert!(
            !names.iter().any(|(n, _)| n == "inner"),
            "block-local must not leak: {names:?}"
        );
    }

    #[test]
    fn closure_params_and_match_bindings_are_in_scope() {
        let source = "fn f(items: array) {\n\
    items.map((row) => {\n\
        return match (row) {\n\
            Some(value) => /*cursor*/ value,\n\
            None => 0,\n\
        };\n\
    });\n\
}\n";
        let names = scope_names(source, "/*cursor*/");
        let has = |needle: &str| names.iter().any(|(n, _)| n == needle);
        assert!(has("row"), "names={names:?}");
        assert!(has("value"), "names={names:?}");
        assert!(has("items"), "names={names:?}");
    }

    #[test]
    fn for_of_binding_is_in_scope_in_body() {
        let source = "fn f(xs: array) {\n    for (const x of xs) {\n        /*cursor*/\n    }\n}\n";
        let names = scope_names(source, "/*cursor*/");
        assert!(names.iter().any(|(n, _)| n == "x"), "names={names:?}");
    }

    #[test]
    fn jsx_tag_prefix_inside_tag_name() {
        let source = "fn Page() { return <Counter /> }\nconst page = <P />;\n";
        let arena = Bump::new();
        let program = crate::parse(source, &arena)
            .program
            .expect("fixture parses");
        let offset = source.find("Counter").expect("tag") + 3;
        assert_eq!(jsx_tag_prefix_at(&program, source, offset), Some("Cou"));
        // Immediately before the element: empty prefix at the `<`.
        let at_open = source.find("<P").expect("open");
        assert_eq!(jsx_tag_prefix_at(&program, source, at_open), Some(""));
        // Inside an attribute expression is not a tag position.
        let source2 = "const page = <img src={url} />;\n";
        let arena2 = Bump::new();
        let program2 = crate::parse(source2, &arena2)
            .program
            .expect("fixture parses");
        let url = source2.find("url").expect("url") + 1;
        assert_eq!(jsx_tag_prefix_at(&program2, source2, url), None);
    }
}
