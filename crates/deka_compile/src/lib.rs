//! DekaScript compiler orchestrator (Compiler v2).

pub mod module_graph;
pub mod shake;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use bumpalo::Bump;
use deka_emit::emit_js_module_with_options;
use deka_syntax::typeck::Type;
use deka_syntax::{
    check_program_with_imports, parse, resolve_imported_enum_constructors, Diagnostic, Expr,
    ModuleExports, Program, Stmt,
};

/// Bare specifiers that are treated as stdlib modules in the single-file WASM
/// compiler path. Imports from these modules are accepted with `Type::Infer`
/// so that tour and testsuite fixtures can compile without a full package graph.
fn file_allows_jsx(file_path: &str) -> bool {
    file_path.to_ascii_lowercase().ends_with(".dsx")
}

fn file_type_rule_error(
    file_path: &str,
    source: &str,
    program: &Program<'_>,
) -> Option<Diagnostic> {
    if !file_allows_jsx(file_path) && program_contains_jsx(program) {
        return Some(Diagnostic::error(
            1,
            1,
            "JSX is only allowed in `.dsx` files; rename this file from `.ds` to `.dsx`"
                .to_string(),
        ));
    }
    let meta = parse_source_module_meta(source);
    let is_dsx = file_allows_jsx(file_path);
    for import in &meta.imports {
        let spec = import.path.as_str();
        if !is_dsx && (spec.ends_with(".dsx") || spec.ends_with(".DSX")) {
            return Some(Diagnostic::error(
                1,
                1,
                format!("`.ds` files cannot import `.dsx` modules (`{spec}`)"),
            ));
        }
        if is_dsx && is_api_import(spec) {
            return Some(Diagnostic::error(
                1,
                1,
                format!("`.dsx` files cannot import the server `api/` tree (`{spec}`)"),
            ));
        }
    }
    None
}

fn is_api_import(spec: &str) -> bool {
    let trimmed = spec.trim();
    trimmed == "api"
        || trimmed.starts_with("api/")
        || trimmed.starts_with("@/api/")
        || trimmed.contains("/api/")
}

fn client_ui_server_error(source: &str) -> Option<Diagnostic> {
    let meta = parse_source_module_meta(source);
    for import in &meta.imports {
        if crate::shake::is_ui_server(&import.path) {
            return Some(Diagnostic::error(
                1,
                1,
                "client bundle cannot import ui/server".to_string(),
            ));
        }
    }
    None
}

fn program_contains_jsx(program: &Program<'_>) -> bool {
    fn expr_has_jsx(expr: &Expr<'_>) -> bool {
        match expr {
            Expr::JsxElement { .. } | Expr::JsxFragment { .. } => true,
            Expr::Call { callee, args, .. } => {
                expr_has_jsx(callee) || args.iter().any(expr_has_jsx)
            }
            Expr::Binary { left, right, .. } => expr_has_jsx(left) || expr_has_jsx(right),
            Expr::Unary { operand, .. } => expr_has_jsx(operand),
            Expr::Await { expr, .. } | Expr::Paren { expr, .. } | Expr::Spread { expr, .. } => {
                expr_has_jsx(expr)
            }
            Expr::Array { elements, .. } => elements.iter().any(expr_has_jsx),
            Expr::Object { fields, .. } => fields.iter().any(|f| expr_has_jsx(&f.value)),
            Expr::FieldAccess { object, .. } => expr_has_jsx(object),
            Expr::IndexAccess { object, index, .. } => expr_has_jsx(object) || expr_has_jsx(index),
            Expr::Ternary {
                condition,
                then_branch,
                else_branch,
                ..
            } => expr_has_jsx(condition) || expr_has_jsx(then_branch) || expr_has_jsx(else_branch),
            Expr::Function { body, .. } => body.iter().any(stmt_has_jsx),
            Expr::Match {
                scrutinee, arms, ..
            } => expr_has_jsx(scrutinee) || arms.iter().any(|arm| expr_has_jsx(&arm.body)),
            Expr::EnumConstructor { payload, .. } => payload.is_some_and(|p| expr_has_jsx(p)),
            Expr::StructLiteral { fields, .. } => fields.iter().any(|f| expr_has_jsx(&f.value)),
            Expr::TemplateLiteral { parts, .. } => parts.iter().any(|part| match part {
                deka_syntax::TemplatePart::Expr(e) => expr_has_jsx(e),
                _ => false,
            }),
            _ => false,
        }
    }
    fn stmt_has_jsx(stmt: &Stmt<'_>) -> bool {
        match stmt {
            Stmt::Const { value, .. }
            | Stmt::Let { value, .. }
            | Stmt::Expr { expr: value, .. } => expr_has_jsx(value),
            Stmt::Return { value, .. } => value.as_ref().is_some_and(expr_has_jsx),
            Stmt::Function { body, .. } | Stmt::ReceiverMethod { body, .. } => {
                body.iter().any(stmt_has_jsx)
            }
            Stmt::Export { decl, .. } => match decl {
                deka_syntax::ExportDecl::Const { value, .. } => expr_has_jsx(value),
                deka_syntax::ExportDecl::Function { body, .. } => body.iter().any(stmt_has_jsx),
                deka_syntax::ExportDecl::NamedGroup { .. } => false,
            },
            Stmt::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                expr_has_jsx(condition)
                    || then_body.iter().any(stmt_has_jsx)
                    || else_body.iter().any(stmt_has_jsx)
            }
            Stmt::Block { body, .. } => body.iter().any(stmt_has_jsx),
            Stmt::For {
                init,
                condition,
                step,
                body,
                ..
            } => {
                condition.as_ref().is_some_and(expr_has_jsx)
                    || step.as_ref().is_some_and(expr_has_jsx)
                    || body.iter().any(stmt_has_jsx)
                    || match init {
                        Some(deka_syntax::ForInit::Expr(e)) => expr_has_jsx(e),
                        Some(deka_syntax::ForInit::Let { value, .. })
                        | Some(deka_syntax::ForInit::Const { value, .. }) => expr_has_jsx(value),
                        None => false,
                    }
            }
            Stmt::ForOf { iterable, body, .. } => {
                expr_has_jsx(iterable) || body.iter().any(stmt_has_jsx)
            }
            _ => false,
        }
    }
    program.statements.iter().any(stmt_has_jsx)
}

pub(crate) fn is_stdlib_module_spec(spec: &str) -> bool {
    if spec.starts_with("@user/") {
        return false;
    }
    let bare = spec.strip_prefix("@deka/").unwrap_or(spec);
    matches!(
        bare,
        "json"
            | "postgres"
            | "mysql"
            | "sqlite"
            | "bytes"
            | "buffer"
            | "http"
            | "tcp"
            | "tls"
            | "fs"
            | "crypto"
            | "jwt"
            | "test"
            | "cookies"
            | "auth"
            | "db"
            | "time"
            | "io"
    ) || bare.starts_with("component/")
        || bare.starts_with("deka/")
        || bare.starts_with("encoding/")
        || bare.starts_with("db/")
        || bare == "ui"
        || bare.starts_with("ui/")
}

/// Names that the single-file compiler can otherwise reinterpret as language
/// prelude bindings when an import signature is unavailable.
fn is_prelude_binding_name(name: &str) -> bool {
    matches!(name, "Option" | "Result" | "Some" | "None" | "Ok" | "Err")
}

/// Build a map of imported module signatures for known stdlib bare specifiers.
///
/// Every imported name is typed as `Type::Infer` so the single-file compiler can
/// compile tour and testsuite fixtures that import stdlib functions. This is a
/// pragmatic bridge: the browser sandbox / native runtime supplies the actual
/// implementations, and the full module graph path collects real signatures.
pub fn infer_stdlib_imports_for_source<'a>(
    source: &'a str,
    arena: &'a Bump,
) -> HashMap<&'a str, ModuleExports<'a>> {
    let mut exports_by_spec: HashMap<&'a str, ModuleExports<'a>> = HashMap::new();
    let parse_result = parse(source, arena);
    let Some(program) = parse_result.program else {
        return exports_by_spec;
    };
    for stmt in program.statements.iter() {
        let deka_syntax::Stmt::Import {
            specifiers,
            source: spec,
            ..
        } = stmt
        else {
            continue;
        };
        if !is_stdlib_module_spec(spec) {
            continue;
        }
        let exports = exports_by_spec.entry(spec).or_default();
        for spec_item in specifiers.iter() {
            exports.values.insert(spec_item.imported, Type::Infer);
        }
    }
    exports_by_spec
}

/// Module metadata extracted from a DekaScript source file.
///
/// This is the v2 equivalent of the old `SourceModuleMeta` type. It is populated by
/// parsing import/export statements at the top level of a `.ds` file. There is
/// no frontmatter stage (RFD 24).
#[derive(Debug, Clone, Default)]
pub struct SourceModuleMeta {
    pub imports: Vec<ImportDecl>,
    pub exports: Vec<ExportDecl>,
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub path: String,
    pub specs: Vec<ImportSpec>,
}

#[derive(Debug, Clone)]
pub struct ImportSpec {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExportDecl {
    pub name: String,
}

/// Extract module metadata (imports and exports) from a `.ds` source file.
///
/// This performs a lightweight parse and walks the top-level statements to
/// collect import sources/specifiers and exported names. It does not
/// typecheck or emit. RFD 24: there is no frontmatter stage.
pub fn parse_source_module_meta(source: &str) -> SourceModuleMeta {
    let arena = Bump::new();
    let result = parse(source, &arena);
    let mut imports = Vec::new();
    let mut exports = Vec::new();

    let Some(program) = result.program else {
        return SourceModuleMeta { imports, exports };
    };

    for stmt in program.statements.iter() {
        match stmt {
            deka_syntax::Stmt::Import {
                specifiers,
                source: src,
                ..
            } => {
                let specs = specifiers
                    .iter()
                    .map(|spec| ImportSpec {
                        name: spec.imported.to_string(),
                        alias: if spec.imported == spec.local {
                            None
                        } else {
                            Some(spec.local.to_string())
                        },
                    })
                    .collect();
                imports.push(ImportDecl {
                    path: src.to_string(),
                    specs,
                });
            }
            deka_syntax::Stmt::Export { decl, .. } => match decl {
                deka_syntax::ExportDecl::Const { name, .. } => {
                    exports.push(ExportDecl {
                        name: name.to_string(),
                    });
                }
                deka_syntax::ExportDecl::Function { name, .. } => {
                    exports.push(ExportDecl {
                        name: name.to_string(),
                    });
                }
                deka_syntax::ExportDecl::NamedGroup { names, source } => {
                    for name in names.iter() {
                        exports.push(ExportDecl {
                            name: name.alias.unwrap_or(name.name).to_string(),
                        });
                    }
                    if let Some(source) = source {
                        imports.push(ImportDecl {
                            path: source.to_string(),
                            specs: names.iter().map(|name| ImportSpec {
                                name: name.name.to_string(),
                                alias: name.alias.map(str::to_string),
                            }).collect(),
                        });
                    }
                }
            },
            _ => {}
        }
    }

    SourceModuleMeta { imports, exports }
}

/// Successful result of compiling a DekaScript source file to JavaScript.
#[derive(Debug)]
pub struct CompileResult {
    pub js: String,
    pub diagnostics: Vec<Diagnostic>,
    /// This module's demand for shared runtime helpers (see
    /// [`CompileOptions::detached_prelude`]). Always populated; only
    /// meaningful when the prelude was detached.
    pub demand: deka_emit::prelude::PreludeDemand,
}

/// Options controlling compiler emission and module resolution.
#[derive(Debug, Default, Clone)]
pub struct CompileOptions {
    /// Base URL for bare module specifiers. When set, imports like
    /// `import { echo } from "io"` are emitted as
    /// `import { echo } from "<module_base>/io.mjs"`.
    pub module_base: Option<String>,
    /// Explicit project root used for module resolution. When set, bare
    /// stdlib imports are resolved against `<module_root>/ds_modules` before
    /// falling back to the current working directory. This removes the need
    /// for the process-global `DEKA_MODULE_ROOT` environment variable in the
    /// v2 compiler path.
    pub module_root: Option<PathBuf>,
    /// Live top-level names after graph shaking. `None` keeps every name.
    pub used_exports: Option<HashSet<String>>,
    /// When true, an import of `ui/server` is a compile error.
    pub client: bool,
    /// When true (module-graph compilation), the shared runtime prelude
    /// (`__deka_struct`, `__deka_type_of`, enum bindings, …) is NOT inlined
    /// into the emitted JS. The module graph unions every module's
    /// [`CompileResult::demand`] and synthesizes the prelude once per
    /// program (deka#595); bundlers prepend it to the single output scope.
    /// Single-module compilation leaves this false: the module stays
    /// self-contained.
    pub detached_prelude: bool,
}

/// Compile a DekaScript source to JavaScript using the v2 pipeline.
///
/// The pipeline is: parse -> typecheck -> emit.  If parsing or typechecking
/// produce errors they are returned directly.  Emit errors are converted to a
/// single diagnostic.
pub fn compile_to_js(source: &str, file_path: &str) -> Result<CompileResult, Vec<Diagnostic>> {
    compile_to_js_with_options(source, file_path, CompileOptions::default())
}

/// Compile a DekaScript source to JavaScript with full options.
pub fn compile_to_js_with_options(
    source: &str,
    file_path: &str,
    options: CompileOptions,
) -> Result<CompileResult, Vec<Diagnostic>> {
    let arena = Bump::new();
    let stdlib_exports = infer_stdlib_imports_for_source(source, &arena);
    let imports: HashMap<&str, &ModuleExports> =
        stdlib_exports.iter().map(|(k, v)| (*k, v)).collect();
    compile_to_js_with_imports_and_options(source, file_path, &arena, &imports, options)
}

/// Compile a DekaScript source to JavaScript with imported module signatures.
///
/// The `arena` must outlive any `ModuleExports` stored in `imports` because the
/// returned `CompileResult` does not own the AST.
pub fn compile_to_js_with_imports<'a>(
    source: &str,
    file_path: &str,
    arena: &'a Bump,
    imports: &HashMap<&str, &ModuleExports<'a>>,
) -> Result<CompileResult, Vec<Diagnostic>> {
    compile_to_js_with_imports_and_options(
        source,
        file_path,
        arena,
        imports,
        CompileOptions::default(),
    )
}

/// Compile a DekaScript source to JavaScript with imported module signatures
/// and emission options.
pub fn compile_to_js_with_imports_and_options<'a>(
    source: &str,
    file_path: &str,
    arena: &'a Bump,
    imports: &HashMap<&str, &ModuleExports<'a>>,
    options: CompileOptions,
) -> Result<CompileResult, Vec<Diagnostic>> {
    let parse_result = parse(source, arena);
    if !parse_result.errors.is_empty() {
        return Err(parse_result.errors);
    }

    let mut program = parse_result.program.ok_or_else(|| {
        vec![Diagnostic::error(
            0,
            0,
            format!("parse produced no program for {}", file_path),
        )]
    })?;

    if let Some(diagnostic) = file_type_rule_error(file_path, source, &program) {
        return Err(vec![diagnostic]);
    }

    // When a module base is configured (browser/WASM single-file mode), bare
    // stdlib imports are left virtual and rewritten to `<base>/<spec>.mjs`.
    // Any other bare specifier has no resolver, so fail early with the same
    // shape as the native module validator instead of emitting a bad import
    // that only fails at runtime (deka#497).
    if options.module_base.is_some() {
        let mut unknown = Vec::new();
        for stmt in program.statements.iter() {
            let deka_syntax::Stmt::Import {
                specifiers,
                source: import_source,
                ..
            } = stmt
            else {
                continue;
            };
            if import_source.starts_with('.')
                || import_source.starts_with('/')
                || import_source.contains(':')
                || crate::module_graph::is_compiler_ui_spec(import_source)
                || crate::is_stdlib_module_spec(import_source)
            {
                continue;
            }
            unknown.extend(
                specifiers
                    .iter()
                    .filter(|spec| is_prelude_binding_name(spec.imported))
                    .map(|spec| {
                        Diagnostic::error(
                            spec.span.start.line,
                            spec.span.start.column,
                            format!(
                                "cannot resolve imported name `{}` from `{}`",
                                spec.imported, import_source
                            ),
                        )
                    }),
            );
        }
        if !unknown.is_empty() {
            return Err(unknown);
        }
    }

    if options.client {
        if let Some(diagnostic) = client_ui_server_error(source) {
            return Err(vec![diagnostic]);
        }
    }

    // A package import with no resolved signature must not be allowed to fall
    // through to a prelude name (for example `Result` or `Option`). In the
    // single-file path the import map is only an inferred-signature map, not a
    // complete resolver, so leave ordinary package imports alone. Report only
    // the missing bindings that canonicalization could reinterpret as prelude
    // types or enum constructors.
    let unresolved_imports: Vec<Diagnostic> = program
        .statements
        .iter()
        .filter_map(|stmt| {
            let deka_syntax::Stmt::Import {
                specifiers, source, ..
            } = stmt
            else {
                return None;
            };
            if imports.contains_key(source)
                || source.starts_with('.')
                || source.starts_with('/')
                || source.starts_with("@/")
                || !source.contains('/')
            {
                return None;
            }
            let diagnostics = specifiers
                .iter()
                .filter(|spec| is_prelude_binding_name(spec.imported))
                .map(|spec| {
                    Diagnostic::error(
                        spec.span.start.line,
                        spec.span.start.column,
                        format!(
                            "cannot resolve imported name `{}` from `{}`",
                            spec.imported, source
                        ),
                    )
                })
                .collect::<Vec<_>>();
            (!diagnostics.is_empty()).then_some(diagnostics)
        })
        .flatten()
        .collect();
    if !unresolved_imports.is_empty() {
        return Err(unresolved_imports);
    }

    resolve_imported_enum_constructors(&mut program, arena, imports);

    let typeck_result = check_program_with_imports(&program, source, imports);
    if !typeck_result.errors.is_empty() {
        return Err(typeck_result.errors);
    }

    let emitted = emit_js_module_with_options(
        &program,
        source,
        imports,
        options.module_base,
        &typeck_result.unwrap_calls,
        &typeck_result.operator_rewrites,
        &typeck_result.method_calls,
        &typeck_result.type_of_calls,
        &typeck_result.signature_calls,
        &typeck_result.json_calls,
        &typeck_result.array_builtin_calls,
        &typeck_result.number_math_calls,
        &typeck_result.static_type_calls,
        &typeck_result.super_trees,
        &typeck_result.jsx_optional_props,
        &typeck_result.enum_case_patterns,
        &typeck_result.union_type_patterns,
        file_path,
        options.used_exports.as_ref(),
        options.detached_prelude,
    )
    .map_err(|message| vec![Diagnostic::error(0, 0, message)])?;

    Ok(CompileResult {
        js: emitted.js,
        diagnostics: typeck_result.warnings,
        demand: emitted.demand,
    })
}

/// Compile a DekaScript source to JavaScript, returning only the emitted JS.
///
/// This is a convenience wrapper around [`compile_to_js`] that formats any
/// diagnostics into a single string on failure.
pub fn compile(source: &str, path: &str) -> Result<String, String> {
    compile_to_js(source, path)
        .map(|result| result.js)
        .map_err(|diagnostics| format_diagnostics(&diagnostics))
}

/// Format a diagnostic in a stable, human-readable form.
pub fn format_diagnostic(diagnostic: &Diagnostic) -> String {
    format!(
        "{}:{}: {}",
        diagnostic.line, diagnostic.column, diagnostic.message
    )
}

/// Format a list of diagnostics into a single multi-line string.
pub fn format_diagnostics(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(format_diagnostic)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_const_number() {
        let result = compile_to_js("const x = 42;", "test.ds").expect("compile should succeed");
        assert!(
            result.js.contains("const x = 42;"),
            "expected emitted JS to contain 'const x = 42;', got:\n{}",
            result.js
        );
    }

    #[test]
    fn compile_function_and_call() {
        let result = compile_to_js(
            "fn add(a: number, b: number) number { return a + b; } const r = add(1, 2);",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("function add"));
        assert!(result.js.contains("add(1, 2)"));
    }

    #[test]
    fn compile_recursive_function() {
        let result = compile_to_js(
            "fn forever(n: number) number { return forever(n); }",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("function forever"));
    }

    #[test]
    fn compile_option_none() {
        let result = compile_to_js("const x: Option<number> = None;", "test.ds")
            .expect("compile should succeed");
        assert!(result.js.contains("const x"));
    }

    /// deka#595: `__deka_type_of` brand branches are gated by the type kinds
    /// this module can actually produce values of — unreachable branches are
    /// dead weight.
    #[test]
    fn compile_type_of_branches_gated_by_type_kinds() {
        let none = compile_to_js("const t = \"x\".getType().toString();", "test.ds")
            .expect("compile should succeed");
        assert!(
            none.js.contains("function __deka_type_of"),
            "got:\n{}",
            none.js
        );
        assert!(!none.js.contains("v.__deka_struct"), "got:\n{}", none.js);
        assert!(!none.js.contains("v.__enum"), "got:\n{}", none.js);
        assert!(!none.js.contains("v.__deka_newtype"), "got:\n{}", none.js);

        let with_struct = compile_to_js(
            "struct Point { x: number }\nconst p = Point { x: 1 };\nconst t = p.getType().toString();",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            with_struct.js.contains("v.__deka_struct"),
            "got:\n{}",
            with_struct.js
        );
        assert!(!with_struct.js.contains("v.__enum"), "got:\n{}", with_struct.js);
    }

    #[test]
    fn compile_type_error_returns_diagnostics() {
        let err =
            compile_to_js("const x: string = 42;", "test.ds").expect_err("compile should fail");
        assert!(
            err.iter()
                .any(|d| d.message.contains("string") && d.message.contains("number")),
            "expected type mismatch diagnostic, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_match_expression() {
        let result = compile_to_js(
            "const o = Some(5); const x = match o { Some(n) => n, None => 0 };",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("__case"));
        assert!(result.js.contains("Some"));
        assert!(result.js.contains("None"));
    }

    #[test]
    fn compile_struct_literal() {
        let result = compile_to_js(
            "struct Point { x: number\n  y: number }\nconst p = Point { x: 1, y: 2 };",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("x: 1"));
        assert!(result.js.contains("y: 2"));
    }

    #[test]
    fn compile_user_defined_enum_constructor() {
        let result = compile_to_js(
            "enum Color { Red, Green, Blue } const c: Color = Color.Red;",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("__case"));
        assert!(result.js.contains("Red"));
    }

    #[test]
    fn compile_union_match_type_patterns() {
        let result = compile_to_js(
            "struct Point { x: number; y: number }\nfn f(v: Point | string) number { return match (v) { Point(p) => p.x, string(s) => s.length }; }",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("__deka_match_scrutinee_1?.__deka_struct === \"Point\""),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("typeof __deka_match_scrutinee_1 === \"string\""),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_union_type_errors_are_diagnostics() {
        let err = compile_to_js("const v: string | string = \"a\";", "test.ds")
            .expect_err("compile should fail");
        assert!(
            err.iter().any(|d| d.message.contains("overlap")),
            "expected overlap diagnostic, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_receiver_method() {
        let result = compile_to_js(
            "struct Point { x: number\n  y: number }\nfn (p Point) distance(other: Point) number { return 0; }\nconst p1: Point = Point { x: 0, y: 0 };\nconst p2: Point = Point { x: 3, y: 4 };\nconst d: number = p1.distance(p2);",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("const Point = __deka_struct"), "got: {}", result.js);
        assert!(result.js.contains("Point.impl(\"distance\""), "got: {}", result.js);
        assert!(result.js.contains("p1.distance(p2)"), "got: {}", result.js);
    }

    #[test]
    fn compile_import_and_use() {
        let result = compile_to_js(
            "import { add } from \"./math.ds\"; const r: number = add(1, 2);",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("import { add } from \"./math.ds\";"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("add(1, 2)"), "got: {}", result.js);
    }

    #[test]
    fn unresolved_package_import_is_reported_at_import_site() {
        let err = compile_to_js(
            "import { Result } from \"@deka/core/result\";\nlet r = Result.Ok(1);",
            "app/page.ds",
        )
        .expect_err("unresolved package import must fail");
        assert_eq!(err.len(), 1, "got: {:?}", err);
        assert_eq!(err[0].line, 1);
        assert!(err[0].message.contains("imported name `Result`"));
        assert!(err[0].message.contains("@deka/core/result"));
        assert!(!err[0].message.contains("is_ok"));
    }

    #[test]
    fn compile_export_const() {
        let result = compile_to_js("export const x: number = 42;", "test.ds")
            .expect("compile should succeed");
        assert!(
            result.js.contains("export const x = 42;"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_export_function() {
        let result = compile_to_js(
            "export fn add(a: number, b: number) number { return a + b; }",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("export function add(a, b) {"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_export_named_group() {
        let result = compile_to_js("const answer = 42; export { answer };", "test.ds")
            .expect("compile should succeed");
        assert!(
            result.js.contains("export { answer };"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn typeck_uses_imported_function_signature() {
        use bumpalo::Bump;
        use deka_syntax::{check_program_with_imports, collect_module_exports, parse};
        use std::collections::HashMap;
        let arena = Bump::new();
        let crypto_src = "export fn random_bytes(n: number) Result<string, string> { return unsafe { String(n) } }";
        let crypto_parse = parse(crypto_src, &arena);
        let crypto_program = crypto_parse.program.unwrap();
        let crypto_exports = collect_module_exports(&crypto_program, &arena);

        let main_src = "import { random_bytes } from \"./crypto.ds\";\nconst r = match (random_bytes(32)) { Ok(v) => v, Err(e) => \"\" };";
        let main_parse = parse(main_src, &arena);
        let mut main_program = main_parse.program.unwrap();
        let mut imports: HashMap<&str, &deka_syntax::typeck::ModuleExports> = HashMap::new();
        imports.insert("./crypto.ds", &crypto_exports);
        deka_syntax::resolve_imported_enum_constructors(&mut main_program, &arena, &imports);
        let result = check_program_with_imports(&main_program, main_src, &imports);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    /// Fixture-first guard for PR B of deka#561 (super declarations,
    /// `User.type()`): the identity mechanism the feature rides on is the
    /// struct brand — a string id compared by value across module boundaries
    /// (deka#556), read at runtime by `__deka_type_of`. If a super-decl
    /// descriptor const ever stops agreeing with what `u.getType()` reports
    /// for an imported struct, the failure is silent, so this test pins the
    /// baseline BEFORE the feature lands and must stay green throughout.
    ///
    /// Compiled against unmodified main it proves: an importing module sees
    /// the imported struct's shape, constructs values through the imported
    /// factory binding, and `getType()` rewrites to the module-local
    /// `__deka_type_of` reading the `__deka_struct` tag.
    #[test]
    fn cross_module_struct_brand_identity_baseline() {
        use bumpalo::Bump;
        use deka_syntax::{collect_module_exports, parse};
        use std::collections::HashMap;
        let arena = Bump::new();
        let lib_src = "struct User { id: number; name: string }\nexport { User }";
        let lib_parse = parse(lib_src, &arena);
        assert!(lib_parse.errors.is_empty(), "{:?}", lib_parse.errors);
        let lib_program = lib_parse.program.unwrap();
        let lib_exports = collect_module_exports(&lib_program, &arena);
        assert!(
            lib_exports.structs.contains_key("User"),
            "lib must export the User struct"
        );

        let main_src = "import { User } from \"./lib.ds\";\nconst u = User { id: 1, name: \"D\" };\nconst t = u.getType().toString();";
        let result = compile_to_js_with_imports(main_src, "main.ds", &arena, &{
            let mut m: HashMap<&str, &deka_syntax::typeck::ModuleExports> = HashMap::new();
            m.insert("./lib.ds", &lib_exports);
            m
        })
        .expect("compile should succeed");
        // The importing module constructs through the imported binding — the
        // factory is declared once, in the declaring module.
        assert!(result.js.contains("User({ id: 1, name: \"D\" })"), "got:\n{}", result.js);
        // getType() rewrites to the module-local tag read; the brand id is
        // the struct name, so cross-module identity is by value, not object.
        assert!(result.js.contains("__deka_type_of(u)"), "got:\n{}", result.js);
        // The importing module must not re-instantiate the factory with its
        // own brand: it constructs through the imported `User` binding. (The
        // module-local `__deka_struct` HELPER is emitted per module by design,
        // deka#556 — only the brand-bearing instantiation must stay singular.)
        assert!(
            !result.js.contains("__deka_struct(\"User\""),
            "the importing module re-declared the User factory:\n{}",
            result.js
        );
    }

    /// The PR B feature case of the baseline above: `super struct` declared
    /// in lib, `User.type()` called from the importing module. The descriptor
    /// const must be emitted where the call is recorded (the importer), the
    /// call must rewrite to the const — not to a fresh structural literal per
    /// call site — and the importer must not re-instantiate the factory or
    /// touch globalThis.
    #[test]
    fn cross_module_super_type_call_uses_imported_struct() {
        use bumpalo::Bump;
        use deka_syntax::{collect_module_exports, parse};
        use std::collections::HashMap;
        let arena = Bump::new();
        let lib_src = "super struct User { id: number; name: string }\nexport { User }";
        let lib_parse = parse(lib_src, &arena);
        assert!(lib_parse.errors.is_empty(), "{:?}", lib_parse.errors);
        let lib_program = lib_parse.program.unwrap();
        let lib_exports = collect_module_exports(&lib_program, &arena);
        assert!(
            lib_exports.structs.get("User").map(|i| i.is_super).unwrap_or(false),
            "lib must export User as a super struct"
        );

        let main_src = "import { User } from \"./lib.ds\";\nconst u = User { id: 1, name: \"D\" };\nconst t = User.type();\nconst s = t.toString();";
        let result = compile_to_js_with_imports(main_src, "main.ds", &arena, &{
            let mut m: HashMap<&str, &deka_syntax::typeck::ModuleExports> = HashMap::new();
            m.insert("./lib.ds", &lib_exports);
            m
        })
        .expect("compile should succeed");
        // The importer references the descriptor const emitted for the super
        // group; a per-call-site structural literal would bloat N calls into
        // N copies and was the rejected design.
        assert!(result.js.contains("__deka_super_desc$User"), "got:\n{}", result.js);
        // The factory stays declared once, in lib; the importer constructs
        // through the imported binding (same invariant as the baseline).
        assert!(
            !result.js.contains("__deka_struct(\"User\""),
            "the importing module re-declared the User factory:\n{}",
            result.js
        );
        assert!(!result.js.contains("globalThis"), "got:\n{}", result.js);
    }

    #[test]
    fn collect_exports_preserves_function_signature() {
        use bumpalo::Bump;
        use deka_syntax::{collect_module_exports, parse, typeck::Type};
        let arena = Bump::new();
        let source = "export fn random_bytes(n: number) Result<bytes, string> { return unsafe { new Uint8Array(n) } }";
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.unwrap();
        let exports = collect_module_exports(&program, &arena);
        let ty = exports
            .values
            .get("random_bytes")
            .expect("random_bytes export");
        match ty {
            Type::Function { params, ret, .. } => {
                assert_eq!(params.len(), 1);
                assert!(matches!(params[0], Type::Named { name: "number" }));
                match ret.as_ref() {
                    Type::Generic {
                        base: "Result",
                        args,
                    } => {
                        assert_eq!(args.len(), 2);
                        assert!(matches!(args[0], Type::Named { name: "bytes" }));
                        assert!(matches!(args[1], Type::Named { name: "string" }));
                    }
                    other => panic!("expected Result generic, got {:?}", other),
                }
            }
            other => panic!("expected function type, got {:?}", other),
        }
    }

    #[test]
    fn compile_array_object_index() {
        let result = compile_to_js(
            "const a = [1, 2, 3]; const o = { x: 1 }; const v = a[0] + o[\"x\"];",
            "test.ds",
        )
        .expect("compile should succeed");
        // deka#590 step 2: const literals are no longer frozen at emit; the
        // checker (deka#591) rejects mutation of a const-bound collection.
        assert!(result.js.contains("const a = [1, 2, 3];"), "got: {}", result.js);
        assert!(
            !result.js.contains("Object.freeze([1, 2, 3])"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("const o = {x: 1};"), "got: {}", result.js);
        assert!(
            !result.js.contains("Object.freeze({x: 1})"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("a[0] + o[\"x\"]"), "got: {}", result.js);
    }

    #[test]
    fn compile_await_and_pipe() {
        let result = compile_to_js(
            "async fn fetch() Promise<number> { return 1; } fn double(n: number) number { return n * 2; } const y = await fetch() |> double;",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("await fetch()"), "got: {}", result.js);
        assert!(result.js.contains("(double)("), "got: {}", result.js);
    }

    #[test]
    fn compile_unsafe_expression() {
        let result = compile_to_js("const r = unsafe { JSON.parse('{}') };", "test.ds")
            .expect("compile should succeed");
        assert!(result.js.contains("__case: \"Ok\""), "got: {}", result.js);
        assert!(result.js.contains("JSON.parse('{}')"), "got: {}", result.js);
    }

    #[test]
    fn compile_panic_throws() {
        let result = compile_to_js("const x: number = panic(\"boom\");", "test.ds")
            .expect("compile should succeed");
        assert!(result.js.contains("throw new Error"), "got: {}", result.js);
        assert!(
            !result.js.contains("globalThis.panic"),
            "got: {}",
            result.js
        );
        let result = compile_to_js("const x: number = deka.panic(\"boom\");", "test.ds")
            .expect("compile should succeed");
        assert!(result.js.contains("throw new Error"), "got: {}", result.js);
    }

    #[test]
    fn compile_unsafe_async() {
        let result = compile_to_js("const r = unsafe { await fetch(url) };", "test.ds")
            .expect("compile should succeed");
        assert!(result.js.contains("async function"), "got: {}", result.js);
        assert!(result.js.contains("await fetch(url)"), "got: {}", result.js);
    }

    #[test]
    fn compile_struct_embed_method() {
        let result = compile_to_js(
            "struct Legs {} fn (l Legs) move() string { return \"walk\" } struct Robot { Legs } const r = Robot { Legs: Legs {} }; const m = r.move();",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("const Legs = __deka_struct"), "got: {}", result.js);
        assert!(result.js.contains("const Robot = __deka_struct(\"Robot\", { Legs: Legs })"), "got: {}", result.js);
        assert!(result.js.contains("Legs.impl(\"move\""), "got: {}", result.js);
        assert!(result.js.contains("r.move()"), "got: {}", result.js);
    }

    #[test]
    fn compile_top_level_await() {
        let result = compile_to_js(
            "async fn main() Promise<number> { return 1 } const n = await main();",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("await main()"), "got: {}", result.js);
    }

    #[test]
    fn compile_jsx_element() {
        let result = compile_to_js("const el = <div class=\"box\" />;", "test.dsx")
            .expect("compile should succeed");
        assert!(
            result
                .js
                .contains("import { jsx, jsxs, Fragment } from \"ui/jsx\""),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("jsx("), "got: {}", result.js);
    }

    #[test]
    fn compile_jsx_fragment() {
        let result = compile_to_js("const el = <><span>a</span><span>b</span></>;", "test.dsx")
            .expect("compile should succeed");
        assert!(result.js.contains("Fragment"), "got: {}", result.js);
    }

    #[test]
    fn compile_jsx_component() {
        let result = compile_to_js(
            "const Greeting = fn () { return <h1 /> }; const el = <Greeting name=\"Deka\" />;",
            "test.dsx",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("jsx(Greeting"), "got: {}", result.js);
        assert!(
            result.js.contains("\"name\": \"Deka\""),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_form_import_from_ui_form() {
        let result = compile_to_js(
            "import { Form } from \"ui/form\";\nconst el = <Form action=\"/api/hello\" method=\"post\">Send</Form>;",
            "test.dsx",
        )
        .expect("ui/form is a compiler-provided specifier");
        assert!(result.js.contains("from \"ui/form\""), "got: {}", result.js);
        assert!(result.js.contains("Form"), "got: {}", result.js);
    }

    #[test]
    fn jsx_in_ds_is_rejected() {
        let err = compile_to_js("const el = <div />;", "test.ds").expect_err("jsx in .ds");
        assert!(
            err.iter().any(|d| d.message.contains(".dsx")),
            "got {:?}",
            err
        );
    }

    #[test]
    fn extract_module_meta() {
        let meta = parse_source_module_meta(
            "import { add } from \"./math.ds\";\nexport const x: number = 1;\nexport fn double(n: number) number { return n * 2; }",
        );
        assert_eq!(meta.imports.len(), 1);
        assert_eq!(meta.imports[0].path, "./math.ds");
        assert_eq!(meta.imports[0].specs.len(), 1);
        assert_eq!(meta.imports[0].specs[0].name, "add");
        assert!(meta.imports[0].specs[0].alias.is_none());
        assert_eq!(meta.exports.len(), 2);
        assert_eq!(meta.exports[0].name, "x");
        assert_eq!(meta.exports[1].name, "double");
    }

    #[test]
    fn compile_newtype_construct_and_unwrap() {
        let result = compile_to_js(
            "type Cents number\nconst c: Cents = Cents(500)\nconst n: number = unboxNumber(c)",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("const Cents$proto"),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("function Cents(v)"),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("const c = Cents(500)"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("const n = c[__p]"), "got: {}", result.js);
    }

    #[test]
    fn compile_toNumber_widens_boolean() {
        let result = compile_to_js("const n: number = toNumber(true)", "test.ds")
            .expect("compile should succeed");
        assert!(
            result.js.contains("const n = Number(true)"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_toNumber_rejects_newtype() {
        let err = compile_to_js(
            "type Cents number\nconst c: Cents = Cents(5)\nconst n: number = toNumber(c)",
            "test.ds",
        )
        .expect_err("compile should fail");
        assert!(
            err.iter().any(|d| d.message.contains("cannot convert")),
            "expected conversion error, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_newtype_ascription_rejected() {
        let err = compile_to_js("type Cents number\nconst c: Cents = 500", "test.ds")
            .expect_err("compile should fail");
        assert!(
            err.iter().any(|d| d.message.contains("Cents")),
            "expected ascription error, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_newtype_unwrap_wrong_repr_rejected() {
        let err = compile_to_js(
            "type Cents number\nconst c: Cents = Cents(500)\nconst s: string = string(c)",
            "test.ds",
        )
        .expect_err("compile should fail");
        assert!(
            err.iter().any(|d| d.message.contains("cannot convert")),
            "expected conversion error, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_newtype_add_same() {
        let result = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(100)\nconst b: Cents = Cents(200)\nconst c: Cents = a + b",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("Cents((a[__p] + b[__p]))"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_newtype_sub_same() {
        let result = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(300)\nconst b: Cents = Cents(100)\nconst c: Cents = a - b",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("Cents((a[__p] - b[__p]))"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_newtype_div_same_returns_number() {
        let result = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(300)\nconst b: Cents = Cents(100)\nconst r: number = a / b",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("(a[__p] / b[__p])"),
            "got: {}",
            result.js
        );
        assert!(
            !result.js.contains("Cents((a[__p] / b[__p]))"),
            "division must not rewrap"
        );
    }

    #[test]
    fn compile_newtype_mul_scalar() {
        let result = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(100)\nconst c: Cents = a * 2",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("Cents((a[__p] * 2))"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_newtype_scalar_mul_left() {
        let result = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(100)\nconst c: Cents = 2 * a",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("Cents((2 * a[__p]))"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_newtype_compare_same() {
        let result = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(100)\nconst b: Cents = Cents(200)\nconst eq: boolean = a == b",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("(a[__p] == b[__p])"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_newtype_unary_neg() {
        let result = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(100)\nconst b: Cents = -a",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(result.js.contains("Cents((-a[__p]))"), "got: {}", result.js);
    }

    #[test]
    fn compile_newtype_add_number_rejected() {
        let err = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(100)\nconst c: Cents = a + 2",
            "test.ds",
        )
        .expect_err("compile should fail");
        assert!(
            err.iter().any(|d| d.message.contains("cannot add")),
            "expected cannot add error, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_newtype_mul_same_rejected() {
        let err = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(100)\nconst b: Cents = Cents(200)\nconst c: Cents = a * b",
            "test.ds",
        )
        .expect_err("compile should fail");
        assert!(
            err.iter().any(|d| d.message.contains("multiply")),
            "expected multiply error, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_newtype_number_div_newtype_rejected() {
        let err = compile_to_js(
            "type Cents number\nconst a: Cents = Cents(100)\nconst r: number = 100 / a",
            "test.ds",
        )
        .expect_err("compile should fail");
        assert!(
            err.iter()
                .any(|d| d.message.contains("number") && d.message.contains("Cents")),
            "expected division type error, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_newtype_string_arithmetic_rejected() {
        let err = compile_to_js(
            "type Name string\nconst a: Name = Name(\"a\")\nconst b: Name = Name(\"b\")\nconst c: Name = a + b",
            "test.ds",
        )
        .expect_err("compile should fail");
        assert!(
            err.iter().any(|d| d.message.contains("cannot add")),
            "expected cannot add error for string newtype, got: {:?}",
            err
        );
    }

    #[test]
    fn compile_newtype_receiver_method() {
        let result = compile_to_js(
            "type Cents number\nfn (c Cents) toDollars() number { return unboxNumber(c) / 100 }\nconst c: Cents = Cents(500)\nconst d: number = c.toDollars()",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("const Cents$proto"),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("Cents$proto.toDollars = function()"),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("const c = Cents(500)"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("c.toDollars()"), "got: {}", result.js);
    }

    #[test]
    fn compile_newtype_receiver_method_uses_self() {
        let result = compile_to_js(
            "type Cents number\nfn (c Cents) doubled() Cents { return Cents(unboxNumber(c) * 2) }\nconst c: Cents = Cents(50)\nconst d: Cents = c.doubled()",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("Cents$proto.doubled = function()"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("const c = this"), "got: {}", result.js);
        assert!(
            result.js.contains("Cents(c[__p] * 2)"),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_newtype_receiver_method_param() {
        let result = compile_to_js(
            "type Cents number\nfn (c Cents) add(other: Cents) Cents { return Cents(unboxNumber(c) + unboxNumber(other)) }\nconst a: Cents = Cents(100)\nconst b: Cents = Cents(200)\nconst c: Cents = a.add(b)",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("Cents$proto.add = function(other)"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("a.add(b)"), "got: {}", result.js);
    }

    #[test]
    fn compile_newtype_mutable_receiver_rejected() {
        let err = compile_to_js(
            "type Cents number\nfn (c mut Cents) setValue(v: number) { c = Cents(v) }",
            "test.ds",
        )
        .expect_err("compile should fail");
        assert!(
            err.iter()
                .any(|d| d.message.contains("mutable") && d.message.contains("newtype")),
            "expected mutable newtype receiver error, got: {:?}",
            err
        );
    }
}
