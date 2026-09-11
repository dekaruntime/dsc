//! Standalone-module optimization: the restricted SWC minify configuration
//! shared by bundle minification (`crate::bundler`) and the module-preserving
//! dist client-asset path (`optimize_emitted_module`, deka#750).

use std::path::{Path, PathBuf};

use swc_common::sync::Lrc;
use swc_common::{FileName, GLOBALS, Globals, Mark, SourceMap};
use swc_atoms::Atom;
use swc_common::{DUMMY_SP, SyntaxContext};
use swc_ecma_ast::op;
use swc_ecma_ast::{
    AssignExpr, AssignTarget, BindingIdent, BlockStmt, BlockStmtOrExpr, Callee, CallExpr,
    CatchClause, Decl, EsVersion, Expr, ExprStmt, Ident, Lit, MemberProp, Module, ModuleItem, Pat, Pass,
    Program, Prop, PropName, PropOrSpread, SimpleAssignTarget, Stmt, TryStmt, VarDecl, VarDeclKind,
    VarDeclarator,
};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};
use swc_ecma_codegen::{Emitter, text_writer::JsWriter};
use swc_ecma_minifier::optimize;
use swc_ecma_minifier::option::{CompressOptions, MangleOptions, MinifyOptions};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};
use swc_ecma_transforms_base::resolver;

/// Optimize already-emitted ESM without resolving or bundling imports.
///
/// Contract: source and output are ESM; relative specifiers are preserved.
/// This is the same safe SWC configuration used for `BundleOptions::minify`
/// and exists for the CLI's module-preserving `--treeshake` mode.
pub fn optimize_emitted_module(source: &str, path: &Path) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        FileName::Real(path.to_path_buf()).into(),
        source.to_string(),
    );
    let syntax = Syntax::Es(EsSyntax {
        jsx: false,
        export_default_from: true,
        import_attributes: true,
        ..Default::default()
    });
    let lexer = Lexer::new(syntax, EsVersion::Es2022, StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);
    let module = parser
        .parse_module()
        .map_err(|err| format!("failed to parse emitted JavaScript: {err:?}"))?;
    let globals = Globals::new();
    let module = GLOBALS.set(&globals, || {
        // Mangling requires resolver hygiene marks: without them shadowed
        // bindings collide under renaming (e.g. a param and a same-named
        // `let` in its body). Resolve with the marks the minifier sees.
        let top_level_mark = Mark::new();
        let unresolved_mark = Mark::new();
        let mut program = Program::Module(module);
        resolver(unresolved_mark, top_level_mark, false).process(&mut program);
        let mut module = match program {
            Program::Module(module) => module,
            Program::Script(_) => unreachable!("emitted module parsed as a script"),
        };
        simplify_dsc_match_patterns(&mut module);
        minify_module(module, cm.clone(), true, unresolved_mark, top_level_mark)
    });
    emit_module_compact(&module, cm)
}

pub(crate) fn minify_module(
    module: Module,
    cm: Lrc<SourceMap>,
    mangle_locals: bool,
    unresolved_mark: Mark,
    top_level_mark: Mark,
) -> Module {
    // These restrictions guard known SWC output bugs: conditionals/bools can
    // emit invalid assignment expressions, sequences can corrupt for-of heads,
    // inline can merge module-local bindings, and if_return can lose ternary
    // parentheses. Keep this shared configuration in sync for bundling and
    // module-preserving transpile optimization.
    let mut compress = CompressOptions::default();
    compress.conditionals = false;
    compress.bools = false;
    compress.sequences = 0;
    compress.inline = 0;
    compress.if_return = false;
    // Local mangling (top-level names untouched, so exports and their string
    // identities survive) is enabled only for the standalone-module path used
    // by dist client assets; bundle output keeps the historical no-mangle.
    let mangle = mangle_locals.then(|| MangleOptions {
        top_level: Some(false),
        ..Default::default()
    });
    let minify_options = MinifyOptions {
        compress: Some(compress),
        mangle,
        ..Default::default()
    };
    match optimize(
        Program::Module(module),
        cm,
        None,
        None,
        &minify_options,
        &swc_ecma_minifier::option::ExtraOptions {
            unresolved_mark,
            top_level_mark,
            mangle_name_cache: Default::default(),
        },
    ) {
        Program::Module(module) => module,
        Program::Script(_) => unreachable!("module optimization returned a script"),
    }
}

/// Minify a standalone JS module source with the same restricted SWC
/// configuration as bundle minification (see `minify_module`).
pub(crate) fn minify_source_text(name: &str, source: &str) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(FileName::Real(PathBuf::from(name)).into(), source.to_string());
    let syntax = Syntax::Es(EsSyntax {
        jsx: false,
        export_default_from: true,
        import_attributes: true,
        ..Default::default()
    });
    let lexer = Lexer::new(syntax, EsVersion::Es2022, StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);
    let module = parser
        .parse_module()
        .map_err(|err| format!("failed to parse prelude for minification: {err:?}"))?;
    let globals = Globals::new();
    let module = GLOBALS.set(&globals, || {
        minify_module(module, cm.clone(), false, Mark::new(), Mark::new())
    });
    emit_module(&module, cm)
}

fn emit_module(module: &Module, cm: Lrc<SourceMap>) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut emitter = Emitter {
        cfg: swc_ecma_codegen::Config::default(),
        comments: None,
        cm: cm.clone(),
        wr: JsWriter::new(cm, "\n", &mut buf, None),
    };
    emitter
        .emit_module(module)
        .map_err(|err| format!("failed to emit optimized JavaScript: {err}"))?;
    String::from_utf8(buf).map_err(|err| format!("optimized JavaScript was not UTF-8: {err}"))
}

/// Compact emission (no pretty whitespace) for minified standalone modules;
/// the bundle emitter keeps its historical formatting.
fn emit_module_compact(module: &Module, cm: Lrc<SourceMap>) -> Result<String, String> {
    let mut cfg = swc_ecma_codegen::Config::default();
    cfg.minify = true;
    let mut buf = Vec::new();
    let mut emitter = Emitter {
        cfg,
        comments: None,
        cm: cm.clone(),
        wr: JsWriter::new(cm, "", &mut buf, None),
    };
    emitter
        .emit_module(module)
        .map_err(|err| format!("failed to emit optimized JavaScript: {err}"))?;
    String::from_utf8(buf).map_err(|err| format!("optimized JavaScript was not UTF-8: {err}"))
}


/// Rewrite the dsc-emitted `match (unsafe { .. })` machinery into the
/// semantically identical direct form before minification.
///
/// dsc 0.8.0 emits every `unsafe` block as a try/catch IIFE that builds a
/// `Result`, plus a three-statement match state machine to extract it:
///
/// ```js
/// let __deka_match_result_1;
/// const __deka_match_scrutinee_1 = ((function () {
///   try { return Ok((function () { return (EXPR); })()); }
///   catch (err) { return Err(..)(..); }
/// })());
/// if (__deka_match_scrutinee_1.__case === "Ok") {
///   const b = __deka_match_scrutinee_1.value;
///   __deka_match_result_1 = b;
/// } else if (__deka_match_scrutinee_1.__case === "Err") {
///   __deka_match_result_1 = FALLBACK;
/// } else { throw new Error("non-exhaustive match"); }
/// ```
///
/// Evaluating the `Ok` wrapper cannot throw on its own, so this is exactly
/// `try { r = EXPR } catch { r = FALLBACK }`. The rewrite matters twice over:
/// the IIFE form is ~3x larger after minification (dist client budget,
/// deka#750/#771), and it triggers miscompiles in the restricted SWC
/// minifier (`collapse_vars` assigns `[].push(x)`'s length; the for-body
/// `continue` inversion corrupts `||` guards of calls). Only the exact
/// generated shapes with the `__deka_match_*` names are rewritten; anything
/// else passes through untouched.
pub(crate) fn simplify_dsc_match_patterns(module: &mut Module) {
    module.visit_mut_with(&mut DscSimplify);
    drop_dead_prelude(module);
}

/// Remove dsc prelude bindings (`Result`, `Option`, and the `Ok`/`Err`/`Some`/
/// `None` aliases) that are no longer referenced after the match rewrite.
/// The prelude is module-local, never exported, so an unreferenced binding is
/// observationally inert — the only side effect lost is `Object.freeze` on a
/// private object nobody can reach.
fn drop_dead_prelude(module: &mut Module) {
    const PRELUDE_NAMES: [&str; 6] = ["Result", "Option", "Ok", "Err", "Some", "None"];
    // Count references (uses) of each prelude name across the module, then
    // drop declarators whose count is zero beyond their own declaration.
    let mut body: Vec<ModuleItem> = std::mem::take(&mut module.body);
    // Collect per-name use counts from every statement that is NOT the
    // prelude declaration itself.
    let mut counts: std::collections::HashMap<Atom, usize> = std::collections::HashMap::new();
    for (idx, item) in body.iter().enumerate() {
        let is_prelude_decl = matches!(item, ModuleItem::Stmt(Stmt::Decl(Decl::Var(var)))
            if var.decls.iter().any(|d| matches!(&d.name, Pat::Ident(b) if PRELUDE_NAMES.contains(&b.id.sym.as_str()))));
        if is_prelude_decl {
            continue;
        }
        let mut collector = IdentCollector { syms: Vec::new() };
        item.visit_with(&mut collector);
        for sym in collector.syms {
            if PRELUDE_NAMES.contains(&sym.as_str()) {
                *counts.entry(sym).or_default() += 1;
            }
        }
        let _ = idx;
    }
    for item in body.iter_mut() {
        let ModuleItem::Stmt(Stmt::Decl(Decl::Var(var))) = item else { continue };
        var.decls.retain(|decl| {
            let Pat::Ident(binding) = &decl.name else { return true };
            if PRELUDE_NAMES.contains(&binding.id.sym.as_str())
                && counts.get(&binding.id.sym).copied().unwrap_or(0) == 0
            {
                return false;
            }
            true
        });
    }
    module.body = body.into_iter().filter(|item| {
        !matches!(item, ModuleItem::Stmt(Stmt::Decl(Decl::Var(var))) if var.decls.is_empty())
    }).collect();
}

/// Source-level entry point: parse an emitted ESM module, rewrite the dsc
/// match machinery in place, and emit the simplified module. Used by the
/// dist client-asset pipeline BEFORE export pruning so the `Result` half of
/// the dsc prelude becomes unreachable and is dropped by the pruner.
pub fn simplify_emitted_module(source: &str, path: &Path) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(
        FileName::Real(path.to_path_buf()).into(),
        source.to_string(),
    );
    let syntax = Syntax::Es(EsSyntax {
        jsx: false,
        export_default_from: true,
        import_attributes: true,
        ..Default::default()
    });
    let lexer = Lexer::new(syntax, EsVersion::Es2022, StringInput::from(&*fm), None);
    let mut parser = Parser::new_from(lexer);
    let mut module = parser
        .parse_module()
        .map_err(|err| format!("failed to parse emitted JavaScript: {err:?}"))?;
    simplify_dsc_match_patterns(&mut module);
    emit_module(&module, cm)
}

struct DscSimplify;

#[derive(Clone, Copy, PartialEq)]
enum DscResultCase {
    Ok,
    Err,
}

fn strip_parens(mut expr: &Expr) -> &Expr {
    while let Expr::Paren(paren) = expr {
        expr = &paren.expr;
    }
    expr
}

fn dsc_result_ctor(expr: &Expr, case: DscResultCase) -> bool {
    let Expr::Arrow(arrow) = strip_parens(expr) else { return false };
    let BlockStmtOrExpr::Expr(body) = &*arrow.body else { return false };
    let Expr::Object(obj) = strip_parens(body) else { return false };
    let mut saw_enum = false;
    let mut saw_case = false;
    for prop in &obj.props {
        let PropOrSpread::Prop(prop) = prop else { return false };
        // The payload field is emitted as an ES6 shorthand (`value` / `error`).
        if let Prop::Shorthand(ident) = &**prop {
            let want = match case {
                DscResultCase::Ok => "value",
                DscResultCase::Err => "error",
            };
            if ident.sym == *want {
                continue;
            }
            return false;
        }
        let Prop::KeyValue(kv) = &**prop else { return false };
        let PropName::Ident(key) = &kv.key else { return false };
        let Expr::Lit(Lit::Str(value)) = &*kv.value else { return false };
        let value = value.value.as_str();
        if key.sym == "__enum" && value == Some("Result") {
            saw_enum = true;
        } else if key.sym == "__case" && value == Some("Ok") && case == DscResultCase::Ok {
            saw_case = true;
        } else if key.sym == "__case" && value == Some("Err") && case == DscResultCase::Err {
            saw_case = true;
        } else if key.sym == "name"
            && ((value == Some("Ok") && case == DscResultCase::Ok)
                || (value == Some("Err") && case == DscResultCase::Err))
        {
        } else {
            return false;
        }
    }
    saw_enum && saw_case
}

/// Extract `EXPR` from `(function () { return (EXPR); })()`.
fn dsc_inner_iife_expr(call: &CallExpr) -> Option<&Expr> {
    if !call.args.is_empty() {
        return None;
    }
    let Callee::Expr(callee) = &call.callee else { return None };
    let Expr::Fn(fn_expr) = strip_parens(callee) else { return None };
    let body = fn_expr.function.body.as_ref()?;
    if body.stmts.len() != 1 {
        return None;
    }
    let Stmt::Return(ret) = &body.stmts[0] else { return None };
    Some(strip_parens(ret.arg.as_deref()?))
}

/// Extract `EXPR` from the scrutinee form
/// `((function () { try { return Ok((function () { return (EXPR); })()); }
/// catch (err) { return Err(..); } })())`.
fn dsc_unsafe_scrutinee(expr: &Expr) -> Option<&Expr> {
    let Expr::Call(call) = strip_parens(expr) else { return None };
    if !call.args.is_empty() {
        return None;
    }
    let Callee::Expr(callee) = &call.callee else { return None };
    let Expr::Fn(fn_expr) = strip_parens(callee) else { return None };
    let body = fn_expr.function.body.as_ref()?;
    if body.stmts.len() != 1 {
        return None;
    }
    let Stmt::Try(try_stmt) = &body.stmts[0] else { return None };
    if try_stmt.finalizer.is_some() || try_stmt.block.stmts.len() != 1 {
        return None;
    }
    let handler = try_stmt.handler.as_ref()?;
    if handler.body.stmts.len() != 1 {
        return None;
    }
    // try { return Ok((function () { return (EXPR); })()); }
    let Stmt::Return(ok_ret) = &try_stmt.block.stmts[0] else { return None };
    let ok_call = ok_ret.arg.as_deref()?;
    let Expr::Call(ok_call) = ok_call else { return None };
    if !dsc_result_ctor(ok_call.callee.as_expr()?, DscResultCase::Ok) {
        return None;
    }
    if ok_call.args.len() != 1 {
        return None;
    }
    let Expr::Call(inner) = strip_parens(&ok_call.args[0].expr) else { return None };
    let extracted = dsc_inner_iife_expr(inner)?;
    // catch (err) { return Err(err instanceof Error ? err : new Error(..)); }
    let Stmt::Return(err_ret) = &handler.body.stmts[0] else { return None };
    let err_call = err_ret.arg.as_deref()?;
    let Expr::Call(err_call) = err_call else { return None };
    if !dsc_result_ctor(err_call.callee.as_expr()?, DscResultCase::Err) {
        return None;
    }
    Some(extracted)
}

fn dsc_case_test(test: &Expr, scrut: &str, case: DscResultCase) -> bool {
    let Expr::Bin(bin) = test else { return false };
    if !matches!(bin.op, op!("===")) {
        return false;
    }
    let want = match case {
        DscResultCase::Ok => "Ok",
        DscResultCase::Err => "Err",
    };
    // `scrut.__case === "Ok"`
    let Expr::Member(member) = &*bin.left else { return false };
    let Expr::Ident(obj) = &*member.obj else { return false };
    if obj.sym != *scrut {
        return false;
    }
    let MemberProp::Ident(prop) = &member.prop else { return false };
    if prop.sym != "__case" {
        return false;
    }
    matches!(&*bin.right, Expr::Lit(Lit::Str(s)) if &*s.value == want)
}

fn dsc_ident(expr: &Expr) -> Option<Atom> {
    match expr {
        Expr::Ident(ident) => Some(ident.sym.clone()),
        _ => None,
    }
}

/// `const b = scrut.value;` binding name, when the statement has exactly that
/// shape.
fn dsc_ok_binding(stmt: &Stmt, scrut: &str) -> Option<Atom> {
    let Stmt::Decl(Decl::Var(var)) = stmt else { return None };
    if var.decls.len() != 1 {
        return None;
    }
    let Pat::Ident(binding) = &var.decls[0].name else { return None };
    let init = var.decls[0].init.as_deref()?;
    let Expr::Member(member) = init else { return None };
    let Expr::Ident(obj) = &*member.obj else { return None };
    if obj.sym != *scrut {
        return None;
    }
    let MemberProp::Ident(prop) = &member.prop else { return None };
    if prop.sym != "value" {
        return None;
    }
    Some(binding.id.sym.clone())
}

/// `target = rhs` expression statement; returns rhs when target is a plain
/// identifier.
fn dsc_assign_rhs<'a>(stmt: &'a Stmt, target: &str) -> Option<&'a Expr> {
    let Stmt::Expr(expr_stmt) = stmt else { return None };
    let Expr::Assign(assign) = &*expr_stmt.expr else { return None };
    if !matches!(assign.op, op!("=")) {
        return None;
    }
    let AssignTarget::Simple(SimpleAssignTarget::Ident(target_ident)) = &assign.left else {
        return None;
    };
    if target_ident.id.sym != *target {
        return None;
    }
    Some(&assign.right)
}

fn dsc_non_exhaustive_throw(stmt: &Stmt) -> bool {
    let Stmt::Throw(throw) = stmt else { return false };
    let Expr::New(new) = &*throw.arg else { return false };
    let Expr::Ident(callee) = &*new.callee else { return false };
    let Some(args) = &new.args else { return false };
    if callee.sym != *"Error" || args.len() != 1 {
        return false;
    }
    matches!(&*args[0].expr, Expr::Lit(Lit::Str(s)) if s.value.as_str() == Some("non-exhaustive match"))
}

/// Collect every identifier symbol inside `expr`.
struct IdentCollector {
    syms: Vec<Atom>,
}

impl Visit for IdentCollector {
    fn visit_ident(&mut self, ident: &Ident) {
        self.syms.push(ident.sym.clone());
    }
}

fn contains_idents(expr: &Expr, names: &[Atom]) -> bool {
    let mut collector = IdentCollector { syms: Vec::new() };
    expr.visit_with(&mut collector);
    collector.syms.iter().any(|sym| names.contains(sym))
}

/// Conservative "cannot throw" analysis for the extracted unsafe expression.
/// Only pure expression forms and a tiny allowlist of spec-total builtins
/// (`Array.isArray`, `Symbol.for`) qualify: anything with a member access can
/// dereference null, any other call may be user code that throws. When the
/// expression qualifies, the `try/catch` the match rewrite would emit is dead
/// and is dropped (`r = EXPR;` instead of `try { r = EXPR } catch { r = FB }`).
fn dsc_cannot_throw(expr: &Expr) -> bool {
    match expr {
        Expr::Ident(..) | Expr::Lit(..) | Expr::This(..) => true,
        Expr::Paren(p) => dsc_cannot_throw(&p.expr),
        Expr::Bin(b) => dsc_cannot_throw(&b.left) && dsc_cannot_throw(&b.right),
        Expr::Unary(u) => dsc_cannot_throw(&u.arg),
        Expr::Cond(c) => {
            dsc_cannot_throw(&c.test) && dsc_cannot_throw(&c.cons) && dsc_cannot_throw(&c.alt)
        }
        Expr::Object(o) => o.props.iter().all(|prop| match prop {
            PropOrSpread::Spread(spread) => dsc_cannot_throw(&spread.expr),
            PropOrSpread::Prop(prop) => match &**prop {
                Prop::KeyValue(kv) => dsc_cannot_throw(&kv.value),
                Prop::Shorthand(..) => true,
                _ => false,
            },
        }),
        Expr::Array(a) => a
            .elems
            .iter()
            .all(|elem| elem.as_ref().is_none_or(|e| dsc_cannot_throw(&e.expr))),
        Expr::Call(c) => {
            let callee = c.callee.as_expr().map(|e| strip_parens(e));
            let allowed = match callee {
                Some(Expr::Ident(ident)) => {
                    matches!(ident.sym.as_str(), "Array.isArray" | "Symbol.for")
                }
                _ => false,
            };
            allowed && c.args.iter().all(|arg| dsc_cannot_throw(&arg.expr))
        }
        _ => false,
    }
}

/// Build the replacement statements for a rewritten match. With `declare`
/// (statement form, where the original `let R;` was consumed) the binding is
/// re-declared; without it (assignment form, where dsc already declared the
/// target earlier in scope) plain assignments are emitted — a `let` there
/// would shadow and silently drop the value. Cannot-throw expressions emit
/// an initialized declaration / plain assignment (the Err fallback is
/// unreachable); otherwise `let R; try { R = EXPR } catch { R = FB }` resp.
/// `try { R = EXPR } catch { R = FB }`. Declarations are kept explicit: the
/// chunks are ES modules (strict mode) and a bare assignment to an undeclared
/// binding throws.
fn build_result_rewrite(
    result_binding: Ident,
    extracted: &Expr,
    fallback: &Expr,
    declare: bool,
) -> Vec<Stmt> {
    let declare_stmt = |init: Option<Expr>| {
        Stmt::Decl(Decl::Var(Box::new(VarDecl {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            kind: VarDeclKind::Let,
            declare: false,
            decls: vec![VarDeclarator {
                span: DUMMY_SP,
                name: Pat::Ident(BindingIdent {
                    id: result_binding.clone(),
                    type_ann: None,
                }),
                init: init.map(Box::new),
                definite: false,
            }],
        })))
    };
    let assign = |rhs: Expr| {
        Stmt::Expr(ExprStmt {
            span: DUMMY_SP,
            expr: Box::new(Expr::Assign(AssignExpr {
                span: DUMMY_SP,
                op: op!("="),
                left: AssignTarget::Simple(SimpleAssignTarget::Ident(BindingIdent {
                    id: result_binding.clone(),
                    type_ann: None,
                })),
                right: Box::new(rhs),
            })),
        })
    };
    if dsc_cannot_throw(extracted) {
        return vec![if declare {
            declare_stmt(Some(extracted.clone()))
        } else {
            assign(extracted.clone())
        }];
    }
    let mut stmts = Vec::with_capacity(2);
    if declare {
        stmts.push(declare_stmt(None));
    }
    stmts.push(Stmt::Try(Box::new(TryStmt {
        span: DUMMY_SP,
        block: BlockStmt {
            span: DUMMY_SP,
            ctxt: SyntaxContext::empty(),
            stmts: vec![assign(extracted.clone())],
        },
        // No catch binding: the emitted Err arms all use `Err(_)`, and a
        // binding here would be textually captured by the minifier's
        // local renaming (a shadowed `tag` param would receive the error
        // object in the fallback recursion).
        handler: Some(CatchClause {
            span: DUMMY_SP,
            param: None,
            body: BlockStmt {
                span: DUMMY_SP,
                ctxt: SyntaxContext::empty(),
                stmts: vec![assign(fallback.clone())],
            },
        }),
        finalizer: None,
    })));
    stmts
}

impl DscSimplify {
    /// Try to rewrite `stmts[i..i+3]` (the match state machine) into
    /// replacement statements. Returns the replacement when the exact
    /// generated shape is present; the replacement re-declares the result
    /// binding (the original `let R;` is consumed as part of the group).
    fn rewrite_match_group(&self, stmts: &[Stmt]) -> Option<Vec<Stmt>> {
        if stmts.len() < 3 {
            return None;
        }
        // let __deka_match_result_N;
        let Stmt::Decl(Decl::Var(result_decl)) = &stmts[0] else { return None };
        if !matches!(result_decl.kind, VarDeclKind::Let) || result_decl.decls.len() != 1 {
            return None;
        }
        let Pat::Ident(result_binding) = &result_decl.decls[0].name else { return None };
        if result_decl.decls[0].init.is_some() {
            return None;
        }
        let result_name = result_binding.id.sym.clone();
        if !result_name.starts_with("__deka_match_result") {
            return None;
        }
        // const __deka_match_scrutinee_N = <unsafe IIFE>;
        let Stmt::Decl(Decl::Var(scrut_decl)) = &stmts[1] else { return None };
        if !matches!(scrut_decl.kind, VarDeclKind::Const) || scrut_decl.decls.len() != 1 {
            return None;
        }
        let Pat::Ident(scrut_binding) = &scrut_decl.decls[0].name else { return None };
        let scrut_init = scrut_decl.decls[0].init.as_deref()?;
        let scrut_name = scrut_binding.id.sym.clone();
        if !scrut_name.starts_with("__deka_match_scrutinee") {
            return None;
        }
        let extracted = dsc_unsafe_scrutinee(scrut_init)?;
        // if (scrut.__case === "Ok") { const b = scrut.value; R = b; }
        let Stmt::If(ok_if) = &stmts[2] else { return None };
        if !dsc_case_test(&ok_if.test, &scrut_name, DscResultCase::Ok) {
            return None;
        }
        let ok_block = match &*ok_if.cons {
            Stmt::Block(block) => block,
            _ => return None,
        };
        if ok_block.stmts.len() != 2 {
            return None;
        }
        let ok_binding = dsc_ok_binding(&ok_block.stmts[0], &scrut_name)?;
        let ok_rhs = dsc_assign_rhs(&ok_block.stmts[1], &result_name)?;
        if dsc_ident(ok_rhs) != Some(ok_binding.clone()) {
            return None;
        }
        // } else if (scrut.__case === "Err") { R = FALLBACK; }
        let else_if = ok_if.alt.as_deref()?;
        let Stmt::If(err_if) = else_if else { return None };
        if !dsc_case_test(&err_if.test, &scrut_name, DscResultCase::Err) {
            return None;
        }
        let err_block = match &*err_if.cons {
            Stmt::Block(block) => block,
            _ => return None,
        };
        if err_block.stmts.len() != 1 {
            return None;
        }
        let fallback = dsc_assign_rhs(&err_block.stmts[0], &result_name)?;
        // } else { throw new Error("non-exhaustive match"); }
        let else_stmt = err_if.alt.as_deref()?;
        let else_stmt = match else_stmt {
            Stmt::Block(block) if block.stmts.len() == 1 => &block.stmts[0],
            other => other,
        };
        if !dsc_non_exhaustive_throw(else_stmt) {
            return None;
        }
        // The fallback must not observe the scrutinee or the Ok binding.
        let observed = [scrut_name.clone(), ok_binding.clone()];
        if contains_idents(fallback, &observed) || contains_idents(extracted, &observed) {
            return None;
        }
        let result_binding = result_binding.id.clone();
        Some(build_result_rewrite(result_binding, extracted, fallback, true))
    }

    /// Rewrite `TARGET = ((scrut) => { if Ok { const b = scrut.value; return
    /// b; } if Err { return FB; } throw })((<unsafe scrutinee>))` — the
    /// expression form dsc emits for `x = match (unsafe { .. }) { .. }` —
    /// into the same `let TARGET; try/catch` shape as the statement form.
    fn rewrite_expr_match_assign(&self, stmt: &Stmt) -> Option<Vec<Stmt>> {
        let Stmt::Expr(expr_stmt) = stmt else { return None };
        let Expr::Assign(assign) = &*expr_stmt.expr else { return None };
        if !matches!(assign.op, op!("=")) {
            return None;
        }
        let AssignTarget::Simple(SimpleAssignTarget::Ident(target)) = &assign.left else {
            return None;
        };
        let Expr::Call(call) = strip_parens(&assign.right) else { return None };
        if call.args.len() != 1 {
            return None;
        }
        let callee = call.callee.as_expr().map(|e| strip_parens(e))?;
        let Expr::Arrow(arrow) = callee else { return None };
        if arrow.is_async || arrow.is_generator || arrow.params.len() != 1 {
            return None;
        }
        let Pat::Ident(scrut_param) = &arrow.params[0] else { return None };
        let scrut_name = scrut_param.id.sym.clone();
        if !scrut_name.starts_with("__deka_scrutinee") {
            return None;
        }
        let BlockStmtOrExpr::BlockStmt(body) = &*arrow.body else { return None };
        if body.stmts.len() != 3 {
            return None;
        }
        // if (scrut.__case === "Ok") { const b = scrut.value; return b; }
        let Stmt::If(ok_if) = &body.stmts[0] else { return None };
        if ok_if.alt.is_some() || !dsc_case_test(&ok_if.test, &scrut_name, DscResultCase::Ok) {
            return None;
        }
        let ok_block = match &*ok_if.cons {
            Stmt::Block(block) => block,
            _ => return None,
        };
        if ok_block.stmts.len() != 2 {
            return None;
        }
        let ok_binding = dsc_ok_binding(&ok_block.stmts[0], &scrut_name)?;
        let Stmt::Return(ok_ret) = &ok_block.stmts[1] else { return None };
        if dsc_ident(ok_ret.arg.as_deref()?) != Some(ok_binding.clone()) {
            return None;
        }
        // if (scrut.__case === "Err") { return FB; }
        let Stmt::If(err_if) = &body.stmts[1] else { return None };
        if err_if.alt.is_some() || !dsc_case_test(&err_if.test, &scrut_name, DscResultCase::Err) {
            return None;
        }
        let err_block = match &*err_if.cons {
            Stmt::Block(block) => block,
            _ => return None,
        };
        if err_block.stmts.len() != 1 {
            return None;
        }
        let Stmt::Return(err_ret) = &err_block.stmts[0] else { return None };
        let fallback = err_ret.arg.as_deref()?;
        // throw new Error("non-exhaustive match");
        if !dsc_non_exhaustive_throw(&body.stmts[2]) {
            return None;
        }
        let extracted = dsc_unsafe_scrutinee(&call.args[0].expr)?;
        // The fallback must not observe the scrutinee or the Ok binding.
        let observed = [scrut_name, ok_binding];
        if contains_idents(fallback, &observed) || contains_idents(extracted, &observed) {
            return None;
        }
        Some(build_result_rewrite(target.id.clone(), extracted, fallback, false))
    }

    /// Rewrite a discarded `<unsafe IIFE>;` expression statement into
    /// `try { EXPR; } catch (_) {}`.
    fn rewrite_discarded_unsafe(&self, stmt: &Stmt) -> Option<Stmt> {
        let Stmt::Expr(expr_stmt) = stmt else { return None };
        let extracted = dsc_unsafe_scrutinee(&expr_stmt.expr)?;
        Some(Stmt::Try(Box::new(TryStmt {
            span: DUMMY_SP,
            block: BlockStmt {
                span: DUMMY_SP,
                ctxt: SyntaxContext::empty(),
                stmts: vec![Stmt::Expr(ExprStmt {
                    span: DUMMY_SP,
                    expr: Box::new(extracted.clone()),
                })],
            },
            handler: Some(CatchClause {
                span: DUMMY_SP,
                param: None,
                body: BlockStmt {
                    span: DUMMY_SP,
                    ctxt: SyntaxContext::empty(),
                    stmts: Vec::new(),
                },
            }),
            finalizer: None,
        })))
    }
}

impl VisitMut for DscSimplify {
    fn visit_mut_block_stmt(&mut self, block: &mut BlockStmt) {
        self.transform_stmts(&mut block.stmts);
        block.visit_mut_children_with(self);
    }

    fn visit_mut_module(&mut self, module: &mut Module) {
        // Module-level statements arrive wrapped in ModuleItem; unwrap,
        // transform, and rewrap.
        let items = std::mem::take(&mut module.body);
        let mut stmts: Vec<Stmt> = Vec::with_capacity(items.len());
        let mut other: Vec<ModuleItem> = Vec::new();
        for item in items {
            match item {
                ModuleItem::Stmt(stmt) => stmts.push(stmt),
                item => other.push(item),
            }
        }
        self.transform_stmts(&mut stmts);
        let mut body: Vec<ModuleItem> = stmts.into_iter().map(ModuleItem::Stmt).collect();
        body.extend(other);
        module.body = body;
        module.visit_mut_children_with(self);
    }
}

impl DscSimplify {
    fn transform_stmts(&self, stmts: &mut Vec<Stmt>) {
        let taken = std::mem::take(stmts);
        let mut out: Vec<Stmt> = Vec::with_capacity(taken.len());
        let mut window: Vec<Stmt> = Vec::with_capacity(3);
        let mut iter = taken.into_iter();
        while let Some(stmt) = iter.next() {
            window.clear();
            window.push(stmt);
            // Gather the lookahead needed to recognize a match group.
            if let Some(next) = iter.next() {
                window.push(next);
                if let Some(third) = iter.next() {
                    window.push(third);
                }
            }
            if window.len() == 3 {
                if let Some(rewritten) = self.rewrite_match_group(&window) {
                    out.extend(rewritten);
                    continue;
                }
                // Not a match group: push the first stmt and put the
                // lookahead back for single-statement handling.
                let third = window.pop().expect("three");
                let second = window.pop().expect("two");
                let first = window.pop().expect("one");
                out.extend(self.rewrite_one(first));
                out.extend(self.rewrite_one(second));
                out.extend(self.rewrite_one(third));
                continue;
            }
            // Tail of one or two statements.
            for stmt in window.drain(..) {
                out.extend(self.rewrite_one(stmt));
            }
        }
        *stmts = out;
    }
}

impl DscSimplify {
    fn rewrite_one(&self, stmt: Stmt) -> Vec<Stmt> {
        if let Some(rewritten) = self.rewrite_expr_match_assign(&stmt) {
            return rewritten;
        }
        match self.rewrite_discarded_unsafe(&stmt) {
            Some(rewritten) => vec![rewritten],
            None => vec![stmt],
        }
    }
}

#[cfg(test)]
mod dsc_simplify_tests {
    use super::*;

    fn optimize(source: &str) -> String {
        optimize_emitted_module(source, Path::new("test.js")).expect("optimize")
    }

    #[test]
    fn rewrites_match_state_machine_to_try_catch() {
        let source = r##"
function someValue(children) {
let __deka_match_result_10;
const __deka_match_scrutinee_10 = ((function() { try { return ((value) => ({ __enum: "Result", __case: "Ok", name: "Ok", value }))((function() { return (
children != null && children.__case === "Some" ? children.value : children
); })()); } catch (err) { return ((error) => ({ __enum: "Result", __case: "Err", name: "Err", error }))(err instanceof Error ? err : new Error(String(err))); } })());
if (__deka_match_scrutinee_10.__case === "Ok") {
 const v = __deka_match_scrutinee_10.value;
 __deka_match_result_10 = v;
}
else if (__deka_match_scrutinee_10.__case === "Err") {
 __deka_match_result_10 = someValue(children);
}
else { throw new Error("non-exhaustive match"); }
return __deka_match_result_10;
}
"##;
        let out = optimize(source);
        assert!(
            !out.contains("__deka_match_scrutinee"),
            "match state machine must go:\n{out}"
        );
        assert!(out.contains("try"), "rewrite must produce try/catch:\n{out}");
        assert!(out.contains("someValue("), "fallback recursion must survive:\n{out}");
        assert!(
            out.contains("let ") && out.contains("try"),
            "rewrite must declare the result binding (strict-mode chunks):\n{out}"
        );
    }

    #[test]
    fn rewrites_expr_position_match_assign() {
        let source = r##"
export function jsx(tag, props, children) {
let nodeProps = props;
if (children) {
} else {
nodeProps = ((__deka_scrutinee) => {
  if (__deka_scrutinee.__case === "Ok") {
    const v = __deka_scrutinee.value;
    return v;
  }
  if (__deka_scrutinee.__case === "Err") {
    return props;
  }
  throw new Error("non-exhaustive match");
})(((function() { try { return ((value) => ({ __enum: "Result", __case: "Ok", name: "Ok", value }))((function() { return (
props ?? {}
); })()); } catch (err) { return ((error) => ({ __enum: "Result", __case: "Err", name: "Err", error }))(err instanceof Error ? err : new Error(String(err))); } })()));
}
return nodeProps;
}
"##;
        let out = optimize(source);
        assert!(
            !out.contains("__deka_scrutinee"),
            "expression match must go:\n{out}"
        );
        assert!(out.contains("??"), "unsafe expression must survive:\n{out}");
        assert_eq!(
            out.matches("let ").count(),
            1,
            "rewrite must assign, not shadow-declare (the target is declared earlier in scope):\n{out}"
        );
    }

    #[test]
    fn rewrites_discarded_top_level_unsafe() {
        let source = r##"
export function Suspense(props) { return props ? props.children : null; }
(function() { try { return ((value) => ({ __enum: "Result", __case: "Ok", name: "Ok", value }))((function() { return (
Suspense.__dekaSuspense = true
); })()); } catch (err) { return ((error) => ({ __enum: "Result", __case: "Err", name: "Err", error }))(err instanceof Error ? err : new Error(String(err))); } })();
"##;
        let out = optimize(source);
        assert!(
            !out.contains("__enum"),
            "discarded unsafe must not build a Result:\n{out}"
        );
        assert!(out.contains("__dekaSuspense=true"), "flag assignment must survive:\n{out}");
    }

    #[test]
    fn leaves_unrelated_code_untouched() {
        let source = r##"
const Result = Object.freeze({
  Ok: (value) => ({ __enum: "Result", __case: "Ok", name: "Ok", value }),
});
export function mine(children) {
  if (children == null) return [];
  return children;
}
"##;
        let out = optimize(source);
        assert!(out.contains("mine"), "plain code survives:\n{out}");
    }

    /// The pinned dsc 0.8.0 emit of the ported ui modules must optimize
    /// without retaining the match machinery or hitting the known SWC
    /// miscompiles (`[].push(x)` length assignment, corrupted `||` guards).
    #[test]
    fn pinned_ui_emit_optimizes_clean() {
        for (name, source) in [
            ("jsx", deka_ui::JSX),
            ("router", deka_ui::ROUTER),
            ("form", deka_ui::FORM),
            ("suspense", deka_ui::SUSPENSE),
        ] {
            let out = optimize(source);
            assert!(
                !out.contains("__deka_match_scrutinee"),
                "{name}: match state machine must go:\n{out}"
            );
            assert!(!out.contains("[].push("), "{name}: collapse_vars miscompile:\n{out}");
        }
    }
}

