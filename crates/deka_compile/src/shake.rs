//! Graph-level purity and used-export shaking (RFD 24 phase 3).
//!
//! A module is **pure** when its top level is only declarations, imports, and
//! exports, and the module contains no `eval` / `require` / dynamic `import()`.
//! From an entry, unused exports of pure modules are dropped, then modules
//! that nothing live imports. Impure modules stay whole. A client entry that
//! can reach `ui/server` is a build failure.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use deka_syntax::{ExportDecl, Expr, Program, Stmt};

/// Compiler-provided UI specifiers. These are not `.ds` files.
pub const UI_SPECIFIERS: &[&str] = &[
    "ui/jsx",
    "ui/reactive",
    "ui/client",
    "ui/server",
    "ui/form",
    "ui/suspense",
    "ui/router",
];

pub fn normalize_ui_specifier(spec: &str) -> Option<String> {
    let trimmed = spec.trim().trim_end_matches(".js").trim_end_matches(".mjs");
    UI_SPECIFIERS
        .iter()
        .copied()
        .find(|known| *known == trimmed)
        .map(str::to_string)
}

pub fn is_ui_server(spec: &str) -> bool {
    matches!(normalize_ui_specifier(spec).as_deref(), Some("ui/server"))
}

/// A module as seen by the shaker: file dependencies plus virtual UI imports.
#[derive(Debug, Clone)]
pub struct ShakeModule {
    pub dependencies: HashMap<String, PathBuf>,
    pub virtual_imports: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ShakePlan {
    /// Modules that remain after reachability (entry + live imports + impure
    /// side-effect imports).
    pub keep: HashSet<PathBuf>,
    /// Live top-level names per kept module. `None` means keep every name
    /// (impure module, or analysis unavailable).
    pub live: HashMap<PathBuf, Option<HashSet<String>>>,
    /// True when any module on the import graph (pre-shake) imports `ui/server`.
    pub reaches_ui_server: bool,
}

pub fn module_is_pure(program: &Program<'_>) -> bool {
    if program_has_dynamic_loading(program) {
        return false;
    }
    program.statements.iter().all(stmt_is_pure_top_level)
}

fn stmt_is_pure_top_level(stmt: &Stmt<'_>) -> bool {
    matches!(
        stmt,
        Stmt::Import { .. }
            | Stmt::Export { .. }
            | Stmt::Const { .. }
            | Stmt::Let { .. }
            | Stmt::Function { .. }
            | Stmt::ReceiverMethod { .. }
            | Stmt::Struct { .. }
            | Stmt::Enum { .. }
            | Stmt::TypeAlias { .. }
            | Stmt::Newtype { .. }
            | Stmt::Interface { .. }
            | Stmt::Empty { .. }
    )
}

fn program_has_dynamic_loading(program: &Program<'_>) -> bool {
    let mut found = false;
    for stmt in program.statements.iter() {
        walk_stmt(stmt, &mut |expr| {
            if expr_is_dynamic_loading(expr) {
                found = true;
            }
        });
        if found {
            return true;
        }
    }
    false
}

fn expr_is_dynamic_loading(expr: &Expr<'_>) -> bool {
    match expr {
        Expr::Call { callee, .. } => match callee {
            Expr::Identifier { name, .. } => *name == "eval" || *name == "require",
            _ => false,
        },
        Expr::Unsafe { source, .. } => {
            source.contains("eval(") || source.contains("require(") || source.contains("import(")
        }
        _ => false,
    }
}

/// Live top-level names for a module given the exports importers still need.
///
/// `is_entry` seeds from top-level runtime statements (the program runs).
/// Impure modules keep every declared name.
pub fn live_names(
    program: &Program<'_>,
    used_exports: &HashSet<String>,
    is_entry: bool,
    pure: bool,
) -> Option<HashSet<String>> {
    if !pure {
        return None;
    }

    let mut live: HashSet<String> = used_exports.clone();
    if is_entry {
        for stmt in program.statements.iter() {
            for name in exported_names(stmt) {
                live.insert(name.to_string());
            }
            if is_entry_seed(stmt) {
                collect_stmt_idents(stmt, &mut live);
                for name in declared_names(stmt) {
                    live.insert(name.into_owned());
                }
            }
        }
    }

    // Named re-exports: if the exported alias is live, the local name is live.
    for stmt in program.statements.iter() {
        if let Stmt::Export {
            decl: ExportDecl::NamedGroup { names, .. },
            ..
        } = stmt
        {
            for name in names.iter() {
                let exported = name.alias.unwrap_or(name.name);
                if live.contains(exported) {
                    live.insert(name.name.to_string());
                }
            }
        }
    }

    // Primitive extension calls are rewritten to `method$receiver` only after
    // typechecking, which runs after shaking, so the mangled name never occurs
    // as an identifier the graph could see. Bridge the gap: a field access
    // matching a declared extension keeps that extension live (deka#527).
    // Struct and newtype methods need no bridge — their receiver type name
    // appears at construction sites.
    let extensions: Vec<(String, String)> = program
        .statements
        .iter()
        .filter_map(|stmt| {
            if let Stmt::ReceiverMethod {
                receiver_type,
                name,
                ..
            } = stmt
            {
                if matches!(*receiver_type, "string" | "number" | "boolean") {
                    return Some((name.to_string(), format!("{name}${receiver_type}")));
                }
            }
            None
        })
        .collect();
    if is_entry {
        for stmt in program.statements.iter() {
            if is_entry_seed(stmt) {
                collect_primitive_extension_uses(stmt, &extensions, &mut live);
            }
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for stmt in program.statements.iter() {
            let names = declared_names(stmt);
            if names.iter().any(|n| live.contains(n.as_ref())) {
                let before = live.len();
                collect_stmt_idents(stmt, &mut live);
                collect_primitive_extension_uses(stmt, &extensions, &mut live);
                if live.len() > before {
                    changed = true;
                }
            }
        }
    }
    Some(live)
}

/// Insert `method$receiver` into `live` for every declared primitive
/// extension whose method name appears as a field access in `stmt`. Call
/// sites keep their pre-typecheck shape (`s.slugify()`), so the mangled name
/// the emitter liveness-gates on is bridged here from the field name
/// (deka#527). `extensions` pairs `(method_name, mangled_name)`.
fn collect_primitive_extension_uses(
    stmt: &Stmt<'_>,
    extensions: &[(String, String)],
    live: &mut HashSet<String>,
) {
    if extensions.is_empty() {
        return;
    }
    let mut accessed = HashSet::new();
    walk_stmt(stmt, &mut |expr| {
        if let Expr::FieldAccess { field, .. } = expr {
            accessed.insert((*field).to_string());
        }
    });
    for (method, mangled) in extensions {
        if accessed.contains(method) {
            live.insert(mangled.clone());
        }
    }
}

fn is_entry_seed(stmt: &Stmt<'_>) -> bool {
    match stmt {
        Stmt::Const { .. }
        | Stmt::Let { .. }
        | Stmt::Expr { .. }
        | Stmt::If { .. }
        | Stmt::Block { .. }
        | Stmt::For { .. }
        | Stmt::ForOf { .. }
        | Stmt::Return { .. } => true,
        Stmt::Export { decl, .. } => matches!(
            decl,
            ExportDecl::Const { .. } | ExportDecl::NamedGroup { .. }
        ),
        _ => false,
    }
}

fn exported_names<'a>(stmt: &'a Stmt<'a>) -> Vec<&'a str> {
    match stmt {
        Stmt::Export { decl, .. } => match decl {
            ExportDecl::Const { name, .. } | ExportDecl::Function { name, .. } => vec![*name],
            ExportDecl::NamedGroup { names, .. } => names
                .iter()
                .map(|n| n.alias.unwrap_or(n.name))
                .collect(),
        },
        _ => Vec::new(),
    }
}

fn declared_names<'a>(stmt: &'a Stmt<'a>) -> Vec<Cow<'a, str>> {
    match stmt {
        Stmt::Const { name, .. }
        | Stmt::Let { name, .. }
        | Stmt::Function { name, .. }
        | Stmt::Struct { name, .. }
        | Stmt::Enum { name, .. }
        | Stmt::TypeAlias { name, .. }
        | Stmt::Newtype { name, .. }
        | Stmt::Interface { name, .. } => vec![Cow::Borrowed(*name)],
        Stmt::ReceiverMethod {
            receiver_type,
            name,
            ..
        } => {
            // Primitive extensions are emitted (and thus live-keyed) under
            // their mangled `method$receiver` name; struct and newtype
            // methods ride the receiver type's name (deka#527).
            if matches!(*receiver_type, "string" | "number" | "boolean") {
                vec![Cow::Owned(format!("{name}${receiver_type}"))]
            } else {
                vec![Cow::Borrowed(*receiver_type)]
            }
        }
        Stmt::Export { decl, .. } => match decl {
            ExportDecl::Const { name, .. } | ExportDecl::Function { name, .. } => {
                vec![Cow::Borrowed(*name)]
            }
            ExportDecl::NamedGroup { names, .. } => names
                .iter()
                .flat_map(|n| [n.name, n.alias.unwrap_or(n.name)])
                .map(Cow::Borrowed)
                .collect(),
        },
        Stmt::Import { specifiers, .. } => specifiers
            .iter()
            .map(|s| Cow::Borrowed(s.local))
            .collect(),
        _ => Vec::new(),
    }
}

fn collect_stmt_idents(stmt: &Stmt<'_>, out: &mut HashSet<String>) {
    walk_stmt(stmt, &mut |expr| collect_expr_idents(expr, out));
}

fn collect_expr_idents(expr: &Expr<'_>, out: &mut HashSet<String>) {
    match expr {
        Expr::Identifier { name, .. } => {
            out.insert((*name).to_string());
        }
        Expr::StructLiteral { name, .. } => {
            out.insert((*name).to_string());
        }
        Expr::EnumConstructor { enum_name, .. } => {
            out.insert((*enum_name).to_string());
        }
        Expr::JsxElement { element, .. } => {
            if element
                .tag
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_uppercase())
            {
                out.insert(element.tag.to_string());
            }
        }
        // An `unsafe` body is raw JS spliced as text, so the walker sees no
        // sub-expressions and every name it uses was invisible here. Anything
        // imported and used *only* inside `unsafe` was therefore shaken out and
        // the emitted module referenced an undefined symbol.
        //
        // That is how `deka build` shipped a Cloudflare Worker whose api/
        // handlers were named but never defined (deka#437): the
        // generated router entry imports them and uses them only inside
        // `unsafe { runApiRouter(...) }`.
        //
        // Over-approximating is the safe direction for dead-code elimination --
        // keeping a name that turns out to be unused costs bytes, dropping one
        // that is used produces a ReferenceError at runtime. So this takes every
        // identifier-shaped token, including ones inside strings and comments.
        Expr::Unsafe { source, .. } => {
            collect_js_identifier_tokens(source, out);
        }
        _ => {}
    }
}

/// Every identifier-shaped token in a chunk of raw JavaScript.
///
/// Deliberately not a lexer: this feeds dead-code elimination, where a false
/// positive keeps a binding alive and a false negative breaks the program.
fn collect_js_identifier_tokens(source: &str, out: &mut HashSet<String>) {
    let mut current = String::new();
    for ch in source.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            current.push(ch);
        } else if !current.is_empty() {
            push_identifier(std::mem::take(&mut current), out);
        }
    }
    if !current.is_empty() {
        push_identifier(current, out);
    }
}

fn push_identifier(token: String, out: &mut HashSet<String>) {
    // A leading digit means it was a number, not a name.
    if token.starts_with(|ch: char| ch.is_ascii_digit()) {
        return;
    }
    out.insert(token);
}

fn walk_stmt(stmt: &Stmt<'_>, visit: &mut dyn FnMut(&Expr<'_>)) {
    match stmt {
        Stmt::Const { value, .. } | Stmt::Let { value, .. } | Stmt::Expr { expr: value, .. } => {
            walk_expr(value, visit);
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                walk_expr(value, visit);
            }
        }
        Stmt::Export { decl, .. } => match decl {
            ExportDecl::Const { value, .. } => walk_expr(value, visit),
            ExportDecl::Function { body, params, .. } => {
                for param in params.iter() {
                    if let Some(default) = &param.default_value {
                        walk_expr(default, visit);
                    }
                }
                for s in body.iter() {
                    walk_stmt(s, visit);
                }
            }
            ExportDecl::NamedGroup { .. } => {}
        },
        Stmt::Function { body, params, .. } | Stmt::ReceiverMethod { body, params, .. } => {
            for param in params.iter() {
                if let Some(default) = &param.default_value {
                    walk_expr(default, visit);
                }
            }
            for s in body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            walk_expr(condition, visit);
            for s in then_body.iter() {
                walk_stmt(s, visit);
            }
            for s in else_body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::Block { body, .. } => {
            for s in body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::For {
            init,
            condition,
            step,
            body,
            ..
        } => {
            match init {
                Some(deka_syntax::ForInit::Const { value, .. })
                | Some(deka_syntax::ForInit::Let { value, .. }) => walk_expr(value, visit),
                Some(deka_syntax::ForInit::Expr(value)) => walk_expr(value, visit),
                None => {}
            }
            if let Some(condition) = condition {
                walk_expr(condition, visit);
            }
            if let Some(step) = step {
                walk_expr(step, visit);
            }
            for s in body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::ForOf { iterable, body, .. } => {
            walk_expr(iterable, visit);
            for s in body.iter() {
                walk_stmt(s, visit);
            }
        }
        Stmt::Struct { fields, .. } => {
            for field in fields.iter() {
                if let Some(default) = &field.default_value {
                    walk_expr(default, visit);
                }
            }
        }
        _ => {}
    }
}

fn walk_expr(expr: &Expr<'_>, visit: &mut dyn FnMut(&Expr<'_>)) {
    visit(expr);
    match expr {
        Expr::Binary { left, right, .. } => {
            walk_expr(left, visit);
            walk_expr(right, visit);
        }
        Expr::Unary { operand, .. } => walk_expr(operand, visit),
        Expr::Call { callee, args, .. } => {
            walk_expr(callee, visit);
            for arg in args.iter() {
                walk_expr(arg, visit);
            }
        }
        Expr::FieldAccess { object, .. }
        | Expr::Await { expr: object, .. }
        | Expr::Paren { expr: object, .. }
        | Expr::Spread { expr: object, .. } => walk_expr(object, visit),
        Expr::IndexAccess { object, index, .. } => {
            walk_expr(object, visit);
            walk_expr(index, visit);
        }
        Expr::StructLiteral { fields, .. } => {
            for field in fields.iter() {
                walk_expr(&field.value, visit);
            }
        }
        Expr::EnumConstructor { payload, .. } => {
            if let Some(payload) = payload {
                walk_expr(payload, visit);
            }
        }
        Expr::Match { scrutinee, arms, .. } => {
            walk_expr(scrutinee, visit);
            for arm in arms.iter() {
                if let Some(guard) = &arm.guard {
                    walk_expr(guard, visit);
                }
                walk_expr(&arm.body, visit);
            }
        }
        Expr::Ternary {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            walk_expr(condition, visit);
            walk_expr(then_branch, visit);
            walk_expr(else_branch, visit);
        }
        Expr::Array { elements, .. } => {
            for element in elements.iter() {
                walk_expr(element, visit);
            }
        }
        Expr::Object { fields, .. } => {
            for field in fields.iter() {
                walk_expr(&field.value, visit);
            }
        }
        Expr::TemplateLiteral { parts, .. } => {
            for part in parts.iter() {
                if let deka_syntax::TemplatePart::Expr(inner) = part {
                    walk_expr(inner, visit);
                }
            }
        }
        Expr::Function { body, params, .. } => {
            for param in params.iter() {
                if let Some(default) = &param.default_value {
                    walk_expr(default, visit);
                }
            }
            for stmt in body.iter() {
                walk_stmt(stmt, visit);
            }
        }
        Expr::JsxElement { element, .. } => {
            for attr in element.attributes.iter() {
                if let Some(value) = &attr.value {
                    walk_expr(value, visit);
                }
            }
            for child in element.children.iter() {
                walk_expr(child, visit);
            }
        }
        Expr::JsxFragment { children, .. } => {
            for child in children.iter() {
                walk_expr(child, visit);
            }
        }
        _ => {}
    }
}

/// Compute keep-set and live names from the entry.
pub fn shake_graph(
    entry: &PathBuf,
    modules: &HashMap<PathBuf, ShakeModule>,
    programs: &HashMap<PathBuf, Program<'_>>,
) -> ShakePlan {
    let reaches_ui_server = import_graph_reaches_ui_server(entry, modules);

    let purity: HashMap<PathBuf, bool> = programs
        .iter()
        .map(|(path, program)| (path.clone(), module_is_pure(program)))
        .collect();

    let mut used_exports: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut keep: HashSet<PathBuf> = HashSet::new();
    let mut side_effect: HashSet<PathBuf> = HashSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    keep.insert(entry.clone());
    queue.push_back(entry.clone());

    while let Some(path) = queue.pop_front() {
        let Some(program) = programs.get(&path) else {
            continue;
        };
        let Some(module) = modules.get(&path) else {
            continue;
        };
        let pure = purity.get(&path).copied().unwrap_or(false);
        let is_entry = path == *entry || side_effect.contains(&path);
        let exports = used_exports.entry(path.clone()).or_default().clone();
        let live = live_names(program, &exports, is_entry, pure);

        // Side-effect imports always keep the target.
        for stmt in program.statements.iter() {
            if let Stmt::Import {
                specifiers, source, ..
            } = stmt
            {
                if specifiers.is_empty() {
                    if let Some(dep) = module.dependencies.get(*source) {
                        side_effect.insert(dep.clone());
                        if keep.insert(dep.clone()) {
                            queue.push_back(dep.clone());
                        } else if !queue.contains(dep) {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }

        let Some(live) = live else {
            // Impure: keep every import.
            for dep in module.dependencies.values() {
                if keep.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
            if let Some(program) = programs.get(&path) {
                for stmt in program.statements.iter() {
                    if let Stmt::Import {
                        specifiers, source, ..
                    } = stmt
                    {
                        if let Some(dep) = module.dependencies.get(*source) {
                            let used = used_exports.entry(dep.clone()).or_default();
                            let before = used.len();
                            for spec in specifiers.iter() {
                                used.insert(spec.imported.to_string());
                            }
                            if used.len() > before && !queue.contains(dep) {
                                queue.push_back(dep.clone());
                            }
                        }
                    }
                }
            }
            continue;
        };

        for stmt in program.statements.iter() {
            if let Stmt::Import {
                specifiers, source, ..
            } = stmt
            {
                let Some(dep) = module.dependencies.get(*source) else {
                    continue;
                };
                let mut needed = false;
                for spec in specifiers.iter() {
                    if live.contains(spec.local) {
                        needed = true;
                        let used = used_exports.entry(dep.clone()).or_default();
                        if used.insert(spec.imported.to_string()) && !queue.contains(dep) {
                            queue.push_back(dep.clone());
                        }
                    }
                }
                if needed && keep.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }

        // An `export { x } from "./module"` has no local import binding, so
        // carry its liveness directly to the re-export source.
        for stmt in program.statements.iter() {
            if let Stmt::Export {
                decl: ExportDecl::NamedGroup { names, source: Some(source) },
                ..
            } = stmt
            {
                let Some(dep) = module.dependencies.get(*source) else { continue };
                let mut needed = false;
                for spec in names.iter() {
                    if live.contains(spec.alias.unwrap_or(spec.name)) {
                        needed = true;
                        let used = used_exports.entry(dep.clone()).or_default();
                        if used.insert(spec.name.to_string()) && !queue.contains(dep) {
                            queue.push_back(dep.clone());
                        }
                    }
                }
                if needed && keep.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    let mut live_map = HashMap::new();
    for path in keep.iter() {
        let Some(program) = programs.get(path) else {
            live_map.insert(path.clone(), None);
            continue;
        };
        let pure = purity.get(path).copied().unwrap_or(false);
        let is_entry = path == entry || side_effect.contains(path);
        let exports = used_exports.get(path).cloned().unwrap_or_default();
        live_map.insert(
            path.clone(),
            live_names(program, &exports, is_entry, pure),
        );
    }

    ShakePlan {
        keep,
        live: live_map,
        reaches_ui_server,
    }
}

fn import_graph_reaches_ui_server(
    entry: &PathBuf,
    modules: &HashMap<PathBuf, ShakeModule>,
) -> bool {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(entry.clone());
    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(module) = modules.get(&path) else {
            continue;
        };
        if module.virtual_imports.iter().any(|spec| is_ui_server(spec)) {
            return true;
        }
        for dep in module.dependencies.values() {
            queue.push_back(dep.clone());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use bumpalo::Bump;
    use deka_syntax::parse;

    fn parse_program<'a>(arena: &'a Bump, source: &'a str) -> Program<'a> {
        let result = parse(source, arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        result.program.expect("program")
    }

    #[test]
    fn pure_module_is_declarations_only() {
        let arena = Bump::new();
        let program = parse_program(
            &arena,
            "export fn keep() { return 1; }\nexport fn drop() { return 2; }",
        );
        assert!(module_is_pure(&program));
    }

    #[test]
    fn top_level_call_is_impure() {
        let arena = Bump::new();
        let program = parse_program(&arena, "echo(\"hi\");\nexport fn keep() { return 1; }");
        assert!(!module_is_pure(&program));
    }

    #[test]
    fn eval_makes_module_impure() {
        let arena = Bump::new();
        let program = parse_program(
            &arena,
            "export fn keep() { return eval(\"1\"); }",
        );
        assert!(!module_is_pure(&program));
    }

    #[test]
    fn unused_export_is_not_live() {
        let arena = Bump::new();
        let program = parse_program(
            &arena,
            "export fn keep() { return \"KEEP_ME\"; }\nexport fn drop() { return \"DROP_ME_UNIQUE\"; }",
        );
        let mut used = HashSet::new();
        used.insert("keep".to_string());
        let live = live_names(&program, &used, false, true).expect("pure");
        assert!(live.contains("keep"));
        assert!(!live.contains("drop"));
    }

    #[test]
    fn used_primitive_extension_is_live_by_mangled_name() {
        let arena = Bump::new();
        let program = parse_program(
            &arena,
            "fn (s string) slugify() string { return s; }\nfn (s string) unused_ext() string { return s; }\nconst title = \"Hello\";\necho(title.slugify());",
        );
        let live = live_names(&program, &HashSet::new(), true, true).expect("pure");
        assert!(live.contains("slugify$string"));
        assert!(!live.contains("unused_ext$string"));
    }

    #[test]
    fn primitive_extension_in_dead_export_is_not_live() {
        let arena = Bump::new();
        let program = parse_program(
            &arena,
            "export fn keep() string { return \"x\".slugify(); }\nexport fn drop() string { return \"y\".unused_ext(); }\nfn (s string) slugify() string { return s; }\nfn (s string) unused_ext() string { return s; }",
        );
        let mut used = HashSet::new();
        used.insert("keep".to_string());
        let live = live_names(&program, &used, false, true).expect("pure");
        assert!(live.contains("slugify$string"));
        assert!(!live.contains("unused_ext$string"));
    }

    #[test]
    fn unused_primitive_extension_is_absent_from_emitted_output() {
        // End to end: liveness keyed by the mangled `method$receiver` name
        // must drop the unused extension while its used sibling survives
        // (deka#527). Route used exports through live_names so the bridge is
        // exercised, and assert on the FUNCTION DEFINITION: the rewritten
        // call site alone also contains the string "slugify$string", so a
        // bare substring assertion on it can pass with the extension missing.
        let source = "fn (s string) slugify() string { return s; }\nfn (s string) unused_ext() string { return \"UNUSED_EXT_UNIQUE\"; }\nexport fn keep() string { return \"x\".slugify(); }";
        let arena = Bump::new();
        let program = parse_program(&arena, source);
        let mut used = HashSet::new();
        used.insert("keep".to_string());
        let live = live_names(&program, &used, false, true).expect("pure");
        let result = crate::compile_to_js_with_options(
            source,
            "module.ds",
            crate::CompileOptions {
                used_exports: Some(live),
                ..Default::default()
            },
        )
        .expect("compile failed");
        assert!(result.js.contains("function slugify$string"));
        assert!(!result.js.contains("function unused_ext$string"));
        assert!(!result.js.contains("UNUSED_EXT_UNIQUE"));
    }
}
