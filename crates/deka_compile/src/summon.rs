//! Tier-1 verification for rfd#39. No fetching and no totality inference.
use deka_syntax::{Diagnostic, Program, Stmt};
use std::{collections::HashMap, path::Path};
use swc_ecma_ast as js;
use swc_ecma_visit::{Visit, VisitWith};

pub fn module_spec(source: &str) -> Result<String, String> {
    if !(source.starts_with("./") || source.starts_with("../"))
        || !source.ends_with(".mjs")
        || source.contains('\\')
    {
        return Err("summon requires a literal relative .mjs module path".into());
    }
    Ok(source.to_owned())
}

#[derive(Clone, Copy)]
struct Arity {
    min: usize,
    max: Option<usize>,
    asynchronous: bool,
}
fn arity<'a>(params: impl Iterator<Item = &'a js::Pat>, asynchronous: bool) -> Arity {
    let params: Vec<_> = params.collect();
    let min = params
        .iter()
        .rposition(|p| !matches!(p, js::Pat::Assign(_) | js::Pat::Rest(_)))
        .map_or(0, |i| i + 1);
    let rest = params.iter().any(|p| matches!(p, js::Pat::Rest(_)));
    Arity {
        min,
        max: if rest { None } else { Some(params.len()) },
        asynchronous,
    }
}
fn function(f: &js::Function) -> Option<Arity> {
    if f.is_generator {
        return None;
    }
    Some(arity(f.params.iter().map(|p| &p.pat), f.is_async))
}
fn expr_function(expr: &js::Expr) -> Option<Arity> {
    match expr {
        js::Expr::Fn(f) => function(&f.function),
        js::Expr::Arrow(f) => Some(arity(f.params.iter(), f.is_async)),
        js::Expr::Paren(p) => expr_function(&p.expr),
        _ => None,
    }
}
fn declared(decl: &js::Decl, bindings: &mut HashMap<String, Option<Arity>>) {
    match decl {
        js::Decl::Fn(f) => {
            bindings.insert(f.ident.sym.to_string(), function(&f.function));
        }
        js::Decl::Var(v) => {
            for d in &v.decls {
                if let js::Pat::Ident(i) = &d.name {
                    // Mutable aliases cannot prove a stable callable export in tier 1.
                    bindings.insert(
                        i.id.sym.to_string(),
                        if v.kind == js::VarDeclKind::Const {
                            d.init.as_deref().and_then(expr_function)
                        } else {
                            None
                        },
                    );
                }
            }
        }
        js::Decl::Class(c) => {
            bindings.insert(c.ident.sym.to_string(), None);
        }
        _ => {}
    }
}
fn export_name(name: &js::ModuleExportName) -> String {
    match name {
        js::ModuleExportName::Ident(i) => i.sym.to_string(),
        js::ModuleExportName::Str(s) => s.value.to_string_lossy().into_owned(),
    }
}
fn exports(module: &js::Module) -> HashMap<String, Option<Arity>> {
    let mut bindings = HashMap::new();
    for item in &module.body {
        match item {
            js::ModuleItem::Stmt(js::Stmt::Decl(d)) => declared(d, &mut bindings),
            js::ModuleItem::ModuleDecl(js::ModuleDecl::ExportDecl(d)) => {
                declared(&d.decl, &mut bindings)
            }
            _ => {}
        }
    }
    let mut exports = HashMap::new();
    for item in &module.body {
        match item {
            js::ModuleItem::ModuleDecl(js::ModuleDecl::ExportDecl(d)) => {
                declared(&d.decl, &mut exports)
            }
            js::ModuleItem::ModuleDecl(js::ModuleDecl::ExportNamed(e)) if e.src.is_none() => {
                for spec in &e.specifiers {
                    if let js::ExportSpecifier::Named(n) = spec {
                        let original = export_name(&n.orig);
                        let external = n
                            .exported
                            .as_ref()
                            .map(export_name)
                            .unwrap_or_else(|| original.clone());
                        exports.insert(external, bindings.get(&original).copied().flatten());
                    }
                }
            }
            _ => {}
        }
    }
    // A declaration proves nothing about a binding that is subsequently
    // reassigned, including from a closure in the same module.
    #[derive(Default)]
    struct Writes(std::collections::HashSet<String>);
    struct TargetWrites<'a>(&'a mut std::collections::HashSet<String>);
    impl Visit for TargetWrites<'_> {
        fn visit_binding_ident(&mut self, ident: &js::BindingIdent) {
            self.0.insert(ident.id.sym.to_string());
        }
    }
    impl Visit for Writes {
        fn visit_assign_expr(&mut self, expr: &js::AssignExpr) {
            expr.left.visit_with(&mut TargetWrites(&mut self.0));
            expr.visit_children_with(self);
        }
        fn visit_for_of_stmt(&mut self, stmt: &js::ForOfStmt) {
            stmt.left.visit_with(&mut TargetWrites(&mut self.0));
            stmt.visit_children_with(self);
        }
        fn visit_for_in_stmt(&mut self, stmt: &js::ForInStmt) {
            stmt.left.visit_with(&mut TargetWrites(&mut self.0));
            stmt.visit_children_with(self);
        }
        fn visit_update_expr(&mut self, expr: &js::UpdateExpr) {
            if let js::Expr::Ident(i) = &*expr.arg {
                self.0.insert(i.sym.to_string());
            }
            expr.visit_children_with(self);
        }
    }
    let mut writes = Writes::default();
    module.visit_with(&mut writes);
    for (name, value) in &mut exports {
        if writes.0.contains(name) {
            *value = None;
        }
    }
    for item in &module.body {
        if let js::ModuleItem::ModuleDecl(js::ModuleDecl::ExportNamed(e)) = item {
            for spec in &e.specifiers {
                if let js::ExportSpecifier::Named(n) = spec {
                    if writes.0.contains(&export_name(&n.orig)) {
                        exports.insert(
                            n.exported
                                .as_ref()
                                .map(export_name)
                                .unwrap_or_else(|| export_name(&n.orig)),
                            None,
                        );
                    }
                }
            }
        }
    }
    exports
}

fn is_promise(
    program: &Program<'_>,
    ty: &deka_syntax::Type<'_>,
    seen: &mut std::collections::HashSet<String>,
) -> bool {
    match ty {
        deka_syntax::Type::Generic { base: "Promise", .. } => true,
        deka_syntax::Type::Named { name, .. } if seen.insert(name.to_string()) => {
            program.statements.iter().any(|stmt| matches!(stmt, Stmt::TypeAlias { name: alias, value, .. } if alias == name && is_promise(program, value, seen)))
        }
        _ => false,
    }
}

pub fn validate(
    program: &Program<'_>,
    file_path: &str,
    virtual_modules: &HashMap<String, String>,
) -> Vec<Diagnostic> {
    let mut errors = Vec::new();
    for stmt in program.statements {
        let Stmt::Summon {
            functions,
            source,
            span,
        } = stmt
        else {
            continue;
        };
        let parsed = (|| {
            let spec = module_spec(source)?;
            let bytes = if let Some(bytes) = virtual_modules.get(&spec) {
                bytes.clone()
            } else {
                std::fs::read_to_string(
                    Path::new(file_path)
                        .parent()
                        .unwrap_or(Path::new("."))
                        .join(&spec),
                )
                .map_err(|e| e.to_string())?
            };
            crate::catalog::parse_js(&bytes)
        })();
        match parsed {
            Err(message) => errors.push(Diagnostic::error(
                span.start.line,
                span.start.column,
                format!("cannot verify summoned module `{source}`: {message}"),
            )),
            Ok(module) => {
                let exports = exports(&module);
                for f in *functions {
                    let problem = match exports.get(f.name) {
                        None => {
                            Some("export does not exist or is an unresolved re-export".to_string())
                        }
                        Some(None) => {
                            Some("export is not a statically verifiable function".to_string())
                        }
                        Some(Some(a))
                            if a.asynchronous
                                && !is_promise(
                                    program,
                                    &f.return_type,
                                    &mut std::collections::HashSet::new(),
                                ) =>
                        {
                            Some("async export requires a Promise<T> signature".to_string())
                        }
                        Some(Some(a))
                            if f.params.len() < a.min
                                || a.max.is_some_and(|max| f.params.len() > max) =>
                        {
                            Some(format!(
                                "incompatible arity: declaration has {}, JavaScript accepts {}..{}",
                                f.params.len(),
                                a.min,
                                a.max.map_or("unbounded".to_string(), |n| n.to_string())
                            ))
                        }
                        _ => None,
                    };
                    if let Some(problem) = problem {
                        errors.push(Diagnostic::error(
                            f.span.start.line,
                            f.span.start.column,
                            format!("summoned module `{source}`, export `{}`: {problem}", f.name),
                        ));
                    }
                }
            }
        }
    }
    errors
}
