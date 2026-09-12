//! Draft summon-block scaffolder (rfd#39 amendment, authoring tier-3).
//!
//! `total` is emitted only where visible analysis of this module proves there
//! are no uncaught `throw` sites, no known-throwing intrinsics, and no calls
//! into unseen code. Otherwise the pessimistic default is
//! `Exception<T, JsError>`. The draft never emits `total` silently: even a
//! proved-total signature is a claim the author must own before committing.
use super::{collect, module_spec, Callable, ExportFunction};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use swc_ecma_ast as js;
use swc_ecma_visit::{Visit, VisitWith};

const DRAFT_HEADER: &str = "\
// DRAFT — review before committing
//
// Rule: `total` is emitted only where visible analysis of this module proves
// there are no throw sites and no calls into unseen code. Uncaught `throw`,
// known-throwing intrinsics (JSON.parse, decodeURI*), and calls the walker
// cannot see all force the pessimistic default Exception<T, JsError>.
// `total` is a claim the author must own; the draft never emits it silently.
";

/// Scaffold a DRAFT summon declaration from vendored `.mjs` source.
///
/// `module_spec` is the `from "..."` path written into the block and must be a
/// literal relative `.mjs` specifier. Parameter types that cannot be named
/// without `JsValue` become opaque-candidate placeholders listed in the header.
pub fn infer_draft(source: &str, module_spec_str: &str) -> Result<String, String> {
    let spec = module_spec(module_spec_str)?;
    let module = crate::catalog::parse_js(source)?;
    Ok(render_draft(&module, &spec))
}

/// Read a vendored `.mjs` path and scaffold a DRAFT summon declaration.
///
/// The `from` specifier is `./` plus the file name, so the draft is ready to
/// sit beside the module as `summon.d.ds`. Pass [`infer_draft`] a different
/// specifier when the caller has a better relative path.
pub fn infer_draft_from_path(path: &Path) -> Result<String, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("module path is not valid UTF-8: {}", path.display()))?;
    if !name.ends_with(".mjs") {
        return Err(format!(
            "summon infer requires a .mjs module, got {}",
            path.display()
        ));
    }
    infer_draft(&source, &format!("./{name}"))
}

fn render_draft(module: &js::Module, spec: &str) -> String {
    let collected = collect(module);
    let mut opaques: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut skipped = Vec::new();
    let mut signatures = Vec::new();

    for (name, export) in &collected.exports {
        let Some(function) = export else {
            skipped.push(name.clone());
            continue;
        };
        if !is_ds_ident(name) {
            skipped.push(name.clone());
            continue;
        }
        let mut used_param_names = HashSet::new();
        let params = param_shapes(function.callable, &mut used_param_names);
        let returns = infer_return(function.callable, &params);
        let total = callable_is_total(Some(name), function.callable, &collected.bindings);
        let signature = render_signature(name, function, &params, &returns, total, &mut opaques);
        signatures.push(signature);
    }

    let mut out = String::from(DRAFT_HEADER);
    if !opaques.is_empty() {
        out.push_str("//\n// opaque-candidates:\n");
        for (ty, reasons) in &opaques {
            out.push_str("//   ");
            out.push_str(ty);
            out.push_str(" — ");
            out.push_str(&reasons.join("; "));
            out.push('\n');
        }
    }
    if !skipped.is_empty() {
        out.push_str("//\n// skipped (not a statically verifiable function): ");
        out.push_str(&skipped.join(", "));
        out.push('\n');
    }
    out.push('\n');
    for ty in opaques.keys() {
        out.push_str("opaque type ");
        out.push_str(ty);
        out.push('\n');
    }
    if !opaques.is_empty() {
        out.push('\n');
    }
    out.push_str("summon {\n");
    for (i, signature) in signatures.iter().enumerate() {
        out.push_str("  ");
        out.push_str(signature);
        if i + 1 < signatures.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("} from \"");
    out.push_str(spec);
    out.push_str("\"\n");
    out
}

fn render_signature(
    name: &str,
    function: &ExportFunction<'_>,
    params: &[ParamShape],
    returns: &InferredType,
    total: bool,
    opaques: &mut BTreeMap<String, Vec<String>>,
) -> String {
    let mut sig = String::new();
    if total {
        sig.push_str("total ");
    }
    sig.push_str(name);
    sig.push('(');
    for (i, param) in params.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        sig.push_str(&param.name);
        sig.push_str(": ");
        let ty = param_ds_type(name, param, opaques);
        sig.push_str(&ty);
    }
    sig.push_str("): ");
    let inner = return_ds_type(name, returns, opaques);
    let inner = if function.arity.asynchronous {
        if total {
            format!("Promise<{inner}>")
        } else {
            format!("Promise<Exception<{inner}, JsError>>")
        }
    } else if total {
        inner
    } else {
        format!("Exception<{inner}, JsError>")
    };
    sig.push_str(&inner);
    sig
}

fn param_ds_type(
    export: &str,
    param: &ParamShape,
    opaques: &mut BTreeMap<String, Vec<String>>,
) -> String {
    let elem = match &param.ty {
        InferredType::Number => "number".to_string(),
        InferredType::String => "string".to_string(),
        InferredType::Boolean => "boolean".to_string(),
        other => {
            let ty = opaque_name(&param.name);
            record_opaque(
                opaques,
                &ty,
                format!(
                    "parameter `{}` of {export} has no visible JS type",
                    param.name
                ),
            );
            render_inferred(other, &ty)
        }
    };
    if param.rest {
        format!("Array<{elem}>")
    } else {
        elem
    }
}

fn return_ds_type(
    export: &str,
    returns: &InferredType,
    opaques: &mut BTreeMap<String, Vec<String>>,
) -> String {
    match returns {
        InferredType::Void => "void".to_string(),
        InferredType::Number => "number".to_string(),
        InferredType::String => "string".to_string(),
        InferredType::Boolean => "boolean".to_string(),
        InferredType::Option(inner) => {
            format!("Option<{}>", return_ds_type(export, inner, opaques))
        }
        InferredType::Array(inner) => {
            format!("Array<{}>", return_ds_type(export, inner, opaques))
        }
        InferredType::Object | InferredType::Unknown | InferredType::Nullish => {
            let ty = opaque_name(export);
            let reason = match returns {
                InferredType::Object => format!("object returned by {export}"),
                InferredType::Nullish => format!("nullish return of {export}"),
                _ => format!("return of {export} has no visible JS type"),
            };
            record_opaque(opaques, &ty, reason);
            ty
        }
    }
}

fn render_inferred(ty: &InferredType, opaque: &str) -> String {
    match ty {
        InferredType::Void => "void".to_string(),
        InferredType::Number => "number".to_string(),
        InferredType::String => "string".to_string(),
        InferredType::Boolean => "boolean".to_string(),
        InferredType::Option(inner) => format!("Option<{}>", render_inferred(inner, opaque)),
        InferredType::Array(inner) => format!("Array<{}>", render_inferred(inner, opaque)),
        InferredType::Object | InferredType::Unknown | InferredType::Nullish => opaque.to_string(),
    }
}

fn record_opaque(opaques: &mut BTreeMap<String, Vec<String>>, name: &str, reason: String) {
    let reasons = opaques.entry(name.to_string()).or_default();
    if !reasons.iter().any(|existing| existing == &reason) {
        reasons.push(reason);
    }
}

const RESERVED_TYPES: &[&str] = &[
    "Array",
    "Exception",
    "JsError",
    "Option",
    "Promise",
    "boolean",
    "number",
    "string",
    "void",
];

fn opaque_name(seed: &str) -> String {
    let mut name = pascal_case(seed);
    if name.is_empty() {
        return "Opaque".to_string();
    }
    if RESERVED_TYPES.contains(&name.as_str()) || !is_ds_ident(&name) {
        name.push_str("Handle");
    }
    name
}

fn pascal_case(name: &str) -> String {
    let mut out = String::new();
    let mut cap = true;
    for c in name.chars() {
        if c == '_' || c == '-' {
            cap = true;
        } else if cap {
            for upper in c.to_uppercase() {
                out.push(upper);
            }
            cap = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn is_ds_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

#[derive(Clone, Debug)]
struct ParamShape {
    name: String,
    rest: bool,
    ty: InferredType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InferredType {
    Void,
    Number,
    String,
    Boolean,
    Nullish,
    Object,
    Array(Box<InferredType>),
    Option(Box<InferredType>),
    Unknown,
}

fn param_shapes(callable: Callable<'_>, used: &mut HashSet<String>) -> Vec<ParamShape> {
    let pats: Vec<&js::Pat> = match callable {
        Callable::Fn(f) => f.params.iter().map(|p| &p.pat).collect(),
        Callable::Arrow(a) => a.params.iter().collect(),
    };
    pats.into_iter()
        .map(|pat| {
            let (mut name, rest, ty) = pat_shape(pat);
            if name.is_empty() || !is_ds_ident(&name) {
                name = "arg".to_string();
            }
            let mut candidate = name.clone();
            let mut n = 2;
            while used.contains(&candidate) {
                candidate = format!("{name}{n}");
                n += 1;
            }
            used.insert(candidate.clone());
            ParamShape {
                name: candidate,
                rest,
                ty,
            }
        })
        .collect()
}

fn pat_shape(pat: &js::Pat) -> (String, bool, InferredType) {
    match pat {
        js::Pat::Ident(i) => (i.id.sym.to_string(), false, InferredType::Unknown),
        js::Pat::Assign(a) => {
            let (name, rest, _) = pat_shape(&a.left);
            (name, rest, infer_expr(&a.right, &[]))
        }
        js::Pat::Rest(r) => {
            let (name, _, ty) = pat_shape(&r.arg);
            (name, true, ty)
        }
        js::Pat::Object(_) => ("props".to_string(), false, InferredType::Object),
        js::Pat::Array(_) => ("items".to_string(), false, InferredType::Unknown),
        _ => ("arg".to_string(), false, InferredType::Unknown),
    }
}

fn infer_return(callable: Callable<'_>, params: &[ParamShape]) -> InferredType {
    let mut scan = ReturnScan {
        params,
        values: Vec::new(),
        empty_return: false,
        saw_return: false,
    };
    match callable {
        Callable::Fn(f) => {
            if let Some(body) = &f.body {
                body.visit_with(&mut scan);
            }
        }
        Callable::Arrow(a) => match &*a.body {
            js::BlockStmtOrExpr::BlockStmt(body) => body.visit_with(&mut scan),
            js::BlockStmtOrExpr::Expr(expr) => {
                scan.values.push(infer_expr(expr, params));
                scan.saw_return = true;
            }
        },
    }
    if !scan.saw_return {
        return InferredType::Void;
    }
    if scan.values.is_empty() {
        return InferredType::Void;
    }
    let merged = scan
        .values
        .into_iter()
        .reduce(merge_types)
        .unwrap_or(InferredType::Unknown);
    if scan.empty_return {
        option_of(merged)
    } else {
        merged
    }
}

struct ReturnScan<'a> {
    params: &'a [ParamShape],
    values: Vec<InferredType>,
    empty_return: bool,
    saw_return: bool,
}

impl Visit for ReturnScan<'_> {
    fn visit_function(&mut self, _n: &js::Function) {}
    fn visit_arrow_expr(&mut self, _n: &js::ArrowExpr) {}
    fn visit_class(&mut self, _n: &js::Class) {}
    fn visit_return_stmt(&mut self, stmt: &js::ReturnStmt) {
        self.saw_return = true;
        match &stmt.arg {
            None => self.empty_return = true,
            Some(expr) => self.values.push(infer_expr(expr, self.params)),
        }
    }
}

fn infer_expr(expr: &js::Expr, params: &[ParamShape]) -> InferredType {
    match expr {
        js::Expr::Lit(js::Lit::Num(_)) => InferredType::Number,
        js::Expr::Lit(js::Lit::Str(_)) => InferredType::String,
        js::Expr::Lit(js::Lit::Bool(_)) => InferredType::Boolean,
        js::Expr::Lit(js::Lit::Null(_)) => InferredType::Nullish,
        js::Expr::Ident(id) if id.sym == "undefined" => InferredType::Nullish,
        js::Expr::Ident(id) => params
            .iter()
            .find(|p| p.name == id.sym.as_str())
            .map(|p| p.ty.clone())
            .unwrap_or(InferredType::Unknown),
        js::Expr::Unary(u) if matches!(u.op, js::UnaryOp::Minus | js::UnaryOp::Plus) => {
            InferredType::Number
        }
        js::Expr::Paren(p) => infer_expr(&p.expr, params),
        js::Expr::Object(_) => InferredType::Object,
        js::Expr::Array(a) => {
            let mut elem = InferredType::Unknown;
            let mut saw = false;
            for el in a.elems.iter().flatten() {
                if el.spread.is_some() {
                    continue;
                }
                let next = infer_expr(&el.expr, params);
                elem = if saw { merge_types(elem, next) } else { next };
                saw = true;
            }
            InferredType::Array(Box::new(if saw { elem } else { InferredType::Unknown }))
        }
        js::Expr::Member(m) if member_is(m, "length") => InferredType::Number,
        _ => InferredType::Unknown,
    }
}

fn member_is(member: &js::MemberExpr, name: &str) -> bool {
    matches!(&member.prop, js::MemberProp::Ident(id) if id.sym == name)
}

fn merge_types(a: InferredType, b: InferredType) -> InferredType {
    if a == b {
        return a;
    }
    match (a, b) {
        (InferredType::Nullish, other) | (other, InferredType::Nullish) => option_of(other),
        (InferredType::Option(inner), other) | (other, InferredType::Option(inner)) => {
            option_of(merge_types(*inner, other))
        }
        (InferredType::Array(a), InferredType::Array(b)) => {
            InferredType::Array(Box::new(merge_types(*a, *b)))
        }
        (InferredType::Void, other) | (other, InferredType::Void) => other,
        _ => InferredType::Unknown,
    }
}

fn option_of(ty: InferredType) -> InferredType {
    match ty {
        InferredType::Option(_) => ty,
        InferredType::Nullish | InferredType::Void | InferredType::Unknown => {
            InferredType::Option(Box::new(InferredType::Unknown))
        }
        other => InferredType::Option(Box::new(other)),
    }
}

fn callable_is_total(
    name: Option<&str>,
    callable: Callable<'_>,
    bindings: &HashMap<String, Option<ExportFunction<'_>>>,
) -> bool {
    let mut visiting = HashSet::new();
    let mut cache = HashMap::new();
    analyze_callable(name, callable, bindings, &mut visiting, &mut cache)
}

fn analyze_callable(
    name: Option<&str>,
    callable: Callable<'_>,
    bindings: &HashMap<String, Option<ExportFunction<'_>>>,
    visiting: &mut HashSet<String>,
    cache: &mut HashMap<String, bool>,
) -> bool {
    if let Some(name) = name {
        if let Some(&cached) = cache.get(name) {
            return cached;
        }
        if !visiting.insert(name.to_string()) {
            return true;
        }
    }
    let findings = walk_findings(callable);
    let mut total = !findings.throw && !findings.unseen;
    if total {
        for callee in &findings.ident_calls {
            if is_known_throwing_ident(callee) {
                total = false;
                break;
            }
            match bindings.get(callee) {
                Some(Some(function)) => {
                    if !analyze_callable(Some(callee), function.callable, bindings, visiting, cache)
                    {
                        total = false;
                        break;
                    }
                }
                _ => {
                    total = false;
                    break;
                }
            }
        }
    }
    if let Some(name) = name {
        visiting.remove(name);
        cache.insert(name.to_string(), total);
    }
    total
}

struct Findings {
    throw: bool,
    unseen: bool,
    ident_calls: Vec<String>,
}

fn walk_findings(callable: Callable<'_>) -> Findings {
    let mut scan = ThrowScan {
        throw: false,
        unseen: false,
        ident_calls: Vec::new(),
        try_depth: 0,
    };
    walk_callable(callable, &mut scan);
    Findings {
        throw: scan.throw,
        unseen: scan.unseen,
        ident_calls: scan.ident_calls,
    }
}

fn walk_callable(callable: Callable<'_>, scan: &mut ThrowScan) {
    match callable {
        Callable::Fn(f) => {
            for param in &f.params {
                param.pat.visit_with(scan);
            }
            if let Some(body) = &f.body {
                body.visit_with(scan);
            }
        }
        Callable::Arrow(a) => {
            for param in &a.params {
                param.visit_with(scan);
            }
            match &*a.body {
                js::BlockStmtOrExpr::BlockStmt(body) => body.visit_with(scan),
                js::BlockStmtOrExpr::Expr(expr) => expr.visit_with(scan),
            }
        }
    }
}

struct ThrowScan {
    throw: bool,
    unseen: bool,
    ident_calls: Vec<String>,
    try_depth: usize,
}

impl Visit for ThrowScan {
    fn visit_function(&mut self, _n: &js::Function) {}
    fn visit_arrow_expr(&mut self, _n: &js::ArrowExpr) {}
    fn visit_class(&mut self, _n: &js::Class) {}

    fn visit_throw_stmt(&mut self, stmt: &js::ThrowStmt) {
        if self.try_depth == 0 {
            self.throw = true;
        }
        stmt.arg.visit_with(self);
    }

    fn visit_try_stmt(&mut self, stmt: &js::TryStmt) {
        if stmt.handler.is_some() {
            self.try_depth += 1;
            stmt.block.visit_with(self);
            self.try_depth -= 1;
            stmt.handler.visit_with(self);
        } else {
            stmt.block.visit_with(self);
        }
        stmt.finalizer.visit_with(self);
    }

    fn visit_call_expr(&mut self, call: &js::CallExpr) {
        match &call.callee {
            js::Callee::Expr(expr) => match unwrap_paren(expr) {
                js::Expr::Ident(id) => {
                    let name = id.sym.to_string();
                    if is_known_throwing_ident(&name) {
                        self.throw = true;
                    } else if !self.ident_calls.iter().any(|existing| existing == &name) {
                        self.ident_calls.push(name);
                    }
                }
                js::Expr::Member(member) if is_known_throwing_member(member) => {
                    self.throw = true;
                }
                js::Expr::Fn(f) => {
                    if let Some(body) = &f.function.body {
                        body.visit_with(self);
                    }
                }
                js::Expr::Arrow(a) => match &*a.body {
                    js::BlockStmtOrExpr::BlockStmt(body) => body.visit_with(self),
                    js::BlockStmtOrExpr::Expr(expr) => expr.visit_with(self),
                },
                _ => self.unseen = true,
            },
            _ => self.unseen = true,
        }
        for arg in &call.args {
            arg.visit_with(self);
        }
    }

    fn visit_new_expr(&mut self, expr: &js::NewExpr) {
        if !is_error_constructor(&expr.callee) {
            self.unseen = true;
        }
        expr.callee.visit_with(self);
        if let Some(args) = &expr.args {
            for arg in args {
                arg.visit_with(self);
            }
        }
    }

    fn visit_tagged_tpl(&mut self, tpl: &js::TaggedTpl) {
        self.unseen = true;
        tpl.visit_children_with(self);
    }
}

fn unwrap_paren(expr: &js::Expr) -> &js::Expr {
    match expr {
        js::Expr::Paren(p) => unwrap_paren(&p.expr),
        other => other,
    }
}

fn is_known_throwing_ident(name: &str) -> bool {
    matches!(name, "eval" | "decodeURI" | "decodeURIComponent")
}

fn is_error_constructor(expr: &js::Expr) -> bool {
    matches!(
        unwrap_paren(expr),
        js::Expr::Ident(id)
            if matches!(
                id.sym.as_str(),
                "Error"
                    | "TypeError"
                    | "RangeError"
                    | "SyntaxError"
                    | "URIError"
                    | "EvalError"
                    | "ReferenceError"
                    | "AggregateError"
            )
    )
}

fn is_known_throwing_member(member: &js::MemberExpr) -> bool {
    let js::Expr::Ident(obj) = unwrap_paren(&member.obj) else {
        return false;
    };
    let js::MemberProp::Ident(prop) = &member.prop else {
        return false;
    };
    obj.sym == "JSON" && matches!(prop.sym.as_str(), "parse" | "stringify")
}
