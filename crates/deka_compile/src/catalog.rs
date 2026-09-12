//! Compiler gate and module-private emission for the closed RFD 21 catalog.
use deka_syntax::deka_catalog::{self, Safety};
use deka_syntax::{Diagnostic, Expr, Program, Span};
use std::collections::HashSet;
use std::path::Path;
use swc_common::BytePos;
use swc_ecma_ast as js;
use swc_ecma_parser::{lexer::Lexer, EsSyntax, Parser, StringInput, Syntax};
use swc_ecma_visit::{Visit, VisitWith};

/// The nearest manifest owns a source, including linked packages. Never infer
/// authority from a directory substring or an import spelling. Virtual loaders
/// can supply the same package identity through CompileOptions.
pub fn package_name(path: &str) -> Option<String> {
    let path = Path::new(path).canonicalize().ok()?;
    for dir in path.ancestors().skip(1) {
        // A dependency without a manifest must not inherit the official
        // identity of its consuming project across a package-store boundary.
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| matches!(n, "ds_modules" | "php_modules" | "deka-packages"))
        {
            return None;
        }
        let manifest = dir.join("deka.json");
        if manifest.is_file() {
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(manifest).ok()?).ok()?;
            return value.get("name")?.as_str().map(str::to_owned);
        }
    }
    None
}

fn diagnostic(span: Span, message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(span.start.line, span.start.column, message)
        .with_underline((span.byte_end - span.byte_start).max(1))
}

fn check(
    kind: &str,
    method: &str,
    argc: usize,
    spread: bool,
    safe: bool,
    official: bool,
) -> Result<(), String> {
    if !official {
        return Err(
            "the closed deka.* catalog is stdlib-only; user packages may not reference it".into(),
        );
    }
    let entry = deka_catalog::find_method(kind, method)
        .ok_or_else(|| format!("unknown catalog helper `deka.{kind}.{method}`"))?;
    if safe && entry.safety == Safety::Unsafe {
        return Err(format!(
            "`deka.{kind}.{method}` may throw and requires `unsafe {{ }}`"
        ));
    }
    if spread {
        return Err(
            "catalog calls require a statically known arity; spread arguments are not allowed"
                .into(),
        );
    }
    deka_catalog::check_arity(entry, argc)
}

fn head<'a>(expr: &'a Expr<'a>) -> Option<(&'a str, &'a str, Span)> {
    match expr {
        Expr::FieldAccess {
            object:
                Expr::FieldAccess {
                    object: Expr::Identifier { name: "deka", span },
                    field: kind,
                    ..
                },
            field: method,
            ..
        } => Some((kind, method, *span)),
        _ => None,
    }
}

pub fn validate(program: &Program<'_>, source: &str, official: bool) -> Vec<Diagnostic> {
    let mut errors = Vec::new();
    let mut safe_calls = HashSet::new();
    let mut allowed = HashSet::new();
    let mut references = Vec::new();
    for stmt in program.statements {
        deka_syntax::visit::walk_stmt(stmt, &mut |expr| {
            match expr {
                Expr::Safe { expr, span } => {
                    if let Expr::Call { callee, .. } = expr {
                        if head(callee).is_some() {
                            safe_calls.insert(expr.span().byte_start);
                            return;
                        }
                    }
                    errors.push(diagnostic(
                        *span,
                        "`safe { }` requires exactly one deka.<kind>.<method>(...) catalog call",
                    ));
                }
                Expr::Call {
                    callee, args, span, ..
                } => {
                    if let Some((kind, method, root)) = head(callee) {
                        // The pre-existing ui host capability and panic lang item
                        // are separate from RFD 21's helper catalog (as in pool's gate).
                        if kind == "ui" && !safe_calls.contains(&span.byte_start) {
                            allowed.insert(root.byte_start);
                            return;
                        }
                        allowed.insert(root.byte_start);
                        if !safe_calls.contains(&span.byte_start) {
                            errors.push(diagnostic(
                                *span,
                                "catalog calls require `safe { }` or `unsafe { }`",
                            ));
                        } else if let Err(message) = check(
                            kind,
                            method,
                            args.len(),
                            args.iter().any(|a| matches!(a, Expr::Spread { .. })),
                            true,
                            official,
                        ) {
                            errors.push(diagnostic(*span, message));
                        }
                    }
                }
                Expr::FieldAccess {
                    object: Expr::Identifier { name: "deka", span },
                    field,
                    ..
                } if *field == "panic" || *field == "ui" => {
                    allowed.insert(span.byte_start);
                }
                Expr::Identifier { name: "deka", span } => references.push(*span),
                Expr::Unsafe {
                    source: body, span, ..
                } => {
                    if !body.contains("deka") && !body.contains("\\u") {
                        return;
                    }
                    // The raw token ends immediately before the block's closing
                    // brace. Do not search: an earlier comment can repeat the body.
                    let offset = span.byte_end - 1 - body.len();
                    let prefix = "async function __catalog_check() {\n";
                    let wrapped = format!("{prefix}{body}\n}}");
                    match parse_js(&wrapped) {
                        Ok(module) => {
                            let mut scan = JsScan::default();
                            module.visit_with(&mut scan);
                            for (pos, len, message) in scan.errors(official) {
                                let at = offset + pos.saturating_sub(prefix.len());
                                let before = &source[..at.min(source.len())];
                                let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
                                let column =
                                    before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
                                errors.push(
                                    Diagnostic::error(line, column, message).with_underline(len),
                                );
                            }
                        }
                        Err(message) => errors.push(diagnostic(
                            *span,
                            format!("cannot validate catalog in unsafe JavaScript: {message}"),
                        )),
                    }
                }
                _ => {}
            }
        });
    }
    for span in references {
        if !allowed.contains(&span.byte_start) {
            errors.push(diagnostic(span, "catalog namespace cannot escape: use a direct deka.<kind>.<method>(...) call inside safe/unsafe"));
        }
    }
    errors
}

pub(crate) fn parse_js(source: &str) -> Result<js::Module, String> {
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        js::EsVersion::EsNext,
        StringInput::new(source, BytePos(1), BytePos(source.len() as u32 + 1)),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let module = parser
        .parse_module()
        .map_err(|e| format!("{:?}", e.kind()))?;
    if let Some(error) = parser.take_errors().first() {
        return Err(format!("{:?}", error.kind()));
    }
    Ok(module)
}

fn js_head(expr: &js::Expr) -> Option<(&str, &str, swc_common::Span)> {
    let js::Expr::Member(outer) = expr else {
        return None;
    };
    let js::MemberProp::Ident(method) = &outer.prop else {
        return None;
    };
    let js::Expr::Member(inner) = &*outer.obj else {
        return None;
    };
    let js::MemberProp::Ident(kind) = &inner.prop else {
        return None;
    };
    let js::Expr::Ident(root) = &*inner.obj else {
        return None;
    };
    (root.sym == "deka").then_some((&kind.sym, &method.sym, root.span))
}

#[derive(Default)]
struct JsScan {
    calls: Vec<(String, String, usize, bool, swc_common::Span)>,
    allowed: HashSet<u32>,
    refs: Vec<swc_common::Span>,
    replacements: Vec<(usize, usize)>,
}
impl Visit for JsScan {
    fn visit_call_expr(&mut self, call: &js::CallExpr) {
        if let js::Callee::Expr(callee) = &call.callee {
            if let Some((kind, method, root)) = js_head(callee) {
                self.allowed.insert(root.lo.0);
                if kind != "ui" {
                    self.calls.push((
                        kind.into(),
                        method.into(),
                        call.args.len(),
                        call.args.iter().any(|a| a.spread.is_some()),
                        call.span,
                    ));
                    self.replacements
                        .push((root.lo.0 as usize - 1, (root.hi.0 - root.lo.0) as usize));
                }
            }
        }
        call.visit_children_with(self);
    }
    fn visit_member_expr(&mut self, member: &js::MemberExpr) {
        if let (js::Expr::Ident(root), js::MemberProp::Ident(prop)) = (&*member.obj, &member.prop) {
            if root.sym == "deka" && (prop.sym == "panic" || prop.sym == "ui") {
                self.allowed.insert(root.span.lo.0);
            }
        }
        member.visit_children_with(self);
    }
    fn visit_ident(&mut self, ident: &js::Ident) {
        if ident.sym == "deka" {
            self.refs.push(ident.span);
        }
    }
}
impl JsScan {
    fn errors(&self, official: bool) -> Vec<(usize, usize, String)> {
        let mut errors = Vec::new();
        for (kind, method, argc, spread, span) in &self.calls {
            if let Err(message) = check(kind, method, *argc, *spread, false, official) {
                errors.push((
                    span.lo.0 as usize - 1,
                    (span.hi.0 - span.lo.0) as usize,
                    message,
                ));
            }
        }
        for span in &self.refs {
            if !self.allowed.contains(&span.lo.0) {
                errors.push((span.lo.0 as usize - 1, 4, "catalog namespace cannot escape: computed access, aliases, and indirect calls are forbidden".into()));
            }
        }
        errors
    }
}

/// Rebind validated calls in emitted JS (including raw unsafe bodies) without
/// touching strings/comments. SWC spans are used only for edits; codegen and
/// all existing source layout remain owned by deka_emit.
pub fn bundle_helpers(source: String) -> Result<String, String> {
    if !source.contains("deka") && !source.contains("\\u") {
        return Ok(source);
    }
    let module = parse_js(&source)?;
    let mut scan = JsScan::default();
    module.visit_with(&mut scan);
    if scan.replacements.is_empty() {
        return Ok(source);
    }
    let mut binding = "__dsc_catalog".to_owned();
    while source.contains(&binding) {
        binding.push('_');
    }
    let mut result = source;
    scan.replacements.sort_unstable();
    scan.replacements.dedup();
    for (at, len) in scan.replacements.into_iter().rev() {
        result.replace_range(at..at + len, &binding);
    }
    Ok(format!(
        "\"use strict\";\nconst {binding} = {};\n{result}",
        deka_catalog::CATALOG_HELPERS_JS
    ))
}
