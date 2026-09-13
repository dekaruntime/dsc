//! DekaScript compiler orchestrator (Compiler v2).

pub mod catalog;
pub mod module_graph;
pub mod shake;
pub mod summon;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use bumpalo::Bump;
use deka_emit::{dev_slot_id, dev_slot_source_path, emit_dev_entry, emit_js_module_with_options};
use deka_syntax::typeck::Type;
use deka_syntax::{
    Diagnostic, Expr, ModuleExports, Program, Span, Stmt, check_program_with_imports, parse,
    program_needs_hydration_ids, resolve_imported_enum_constructors,
};
use serde::Serialize;

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
            Expr::Await { expr, .. }
            | Expr::Safe { expr, .. }
            | Expr::Paren { expr, .. }
            | Expr::Spread { expr, .. } => expr_has_jsx(expr),
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
            Stmt::TupleBinding { value, .. }
            | Stmt::Const { value, .. }
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
            Stmt::Try {
                body, catch_body, ..
            } => body.iter().chain(catch_body.iter()).any(stmt_has_jsx),
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

/// The closed, compiler-provided math module. It is emitted as local bindings
/// rather than a host import, so its single constant is available anywhere
/// DekaScript runs without making `Math` ambient.
pub fn is_math_module_spec(spec: &str) -> bool {
    deka_project::module_spec::is_closed_stdlib_module_spec(spec)
}

pub(crate) use deka_project::module_spec::is_stdlib_module_spec;

/// Build synthetic signatures for recognized virtual stdlib modules.
///
/// dsc#111 makes every *ordinary* unresolved import a hard error bound to the
/// `Error` recovery sentinel. The recognized families below are the narrow,
/// explicit exception, defined by deka-modules' closed stdlib vocabulary.
/// Their runtime implementations exist, but this compiler has no real
/// `ModuleExports` declarations for them yet. Each imported value is unchecked
/// (`Infer`) only for these recognized specifiers; `ui` is not among them.
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
        if is_math_module_spec(spec) {
            // `math` is deliberately closed: unlike the package-backed
            // virtual stdlib surface below, only the agreed constant exists.
            // Rejecting a typo at the import is preferable to recreating the
            // unchecked ambient-global hole this module replaces.
            exports
                .values
                .insert("PI", Type::Named { name: "number" });
            continue;
        }
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
                            specs: names
                                .iter()
                                .map(|name| ImportSpec {
                                    name: name.name.to_string(),
                                    alias: name.alias.map(str::to_string),
                                })
                                .collect(),
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
    /// Build-only entries and descriptors. This is a compiler artifact only:
    /// Dsc produces it but never executes the entries.
    pub dev_plan: DevPlan,
}

/// Versioned compiler-to-host contract for build-only values.
#[derive(Debug, Default, Serialize)]
pub struct DevPlan {
    pub version: u32,
    /// Literal `export const prerender = <bool>` disposition for this module,
    /// if present (dsc#54). Hosts plan routes from the entry module's value;
    /// an imported module's `prerender` export is not a route fact. Omitted
    /// from the serialized plan when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prerender: Option<bool>,
    pub slots: Vec<DevPlanSlot>,
}

/// One materialization request in a [`DevPlan`].
#[derive(Debug, Serialize)]
pub struct DevPlanSlot {
    pub id: String,
    pub binding: String,
    pub file: String,
    pub span: Span,
    /// Private type descriptor. Deka validates the returned `Ok(value)` from
    /// this data rather than reimplementing DekaScript type walking.
    pub descriptor: serde_json::Value,
    /// Separate async ES module. It is not part of the runtime output graph.
    pub entry: String,
}

/// Read the nearest project manifest. Runtime paths belong to the project,
/// never to the compiler's vendor layout.
fn project_jsx_runtime(file_path: &str, key: &str) -> Result<Option<String>, Vec<Diagnostic>> {
    for dir in std::path::Path::new(file_path).ancestors().skip(1) {
        let manifest = dir.join("deka.json");
        if !manifest.is_file() {
            continue;
        }
        let read = || -> Result<Option<String>, String> {
            let value: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&manifest).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            match value.get(key) {
                None => Ok(None),
                Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
                _ => Err(format!("deka.json {key} must be a non-empty module specifier")),
            }
        };
        return read().map_err(|message| vec![Diagnostic::error(0, 0, message)]);
    }
    Ok(None)
}

/// Options controlling compiler emission and module resolution.
#[derive(Debug, Default, Clone)]
pub struct CompileOptions {
    /// React automatic JSX runtime; disk projects read `jsxRuntime` from deka.json.
    pub jsx_runtime: Option<String>,
    /// Independent development runtime override; otherwise use the production runtime base.
    pub jsx_dev_runtime: Option<String>,
    /// Emit React jsxDEV calls with DS source locations. Defaults to production.
    pub dev: bool,
    /// Foreign module bytes supplied by a virtual host, keyed by relative specifier.
    /// Filesystem compilation reads these afresh when no virtual source is supplied.
    pub foreign_modules: HashMap<String, String>,
    /// Package identity supplied by a trusted virtual loader; disk sources use
    /// their nearest deka.json. None defaults to unprivileged for virtual files.
    pub package_name: Option<String>,
    /// Base URL for bare module specifiers. When set, imports like
    /// `import { echo } from "io"` are emitted as
    /// `import { echo } from "<module_base>/io.mjs"`.
    pub module_base: Option<String>,
    /// Explicit project root used for module resolution. When set, bare
    /// stdlib imports are resolved against `<module_root>/ds_modules` before
    /// falling back to the current working directory. Build slot ids are also
    /// hashed against this root so
    /// `deka:dev/<id>` is stable across machine and checkout locations; when
    /// absent the historical absolute-path identity is kept (dsc#61).
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
    /// Factories this module's compiler-private `__deka_factories` closure
    /// must capture for consumer build hydration: every descriptor-reachable
    /// factory of this module's exported types, including types private to
    /// this module (dsc#52). Empty (the default) disables the closure export.
    /// Set by module-graph compilation when a live consumer build reaches
    /// these factories.
    pub build_closure_names: HashSet<String>,
    /// Inject `data-deka-id` on host JSX elements. `None` auto-detects from
    /// this module (`client:*` or interactive-component analysis). The module
    /// graph sets `Some` from the union across every emitted module, because
    /// an island's render tree may live in a different file from the
    /// `client:*` usage site.
    pub inject_deka_id: Option<bool>,
}

fn dev_binding<'a>(stmt: &'a Stmt<'a>) -> Option<(&'a str, &'a Expr<'a>)> {
    match stmt {
        Stmt::Const {
            name,
            value: value @ Expr::Build { .. },
            ..
        } => Some((name, value)),
        Stmt::Export {
            decl:
                deka_syntax::ExportDecl::Const {
                    name,
                    value: value @ Expr::Build { .. },
                    ..
                },
            ..
        } => Some((name, value)),
        _ => None,
    }
}

fn build_dev_plan<'a>(
    program: &'a Program<'a>,
    source: &str,
    imports: &HashMap<&str, &ModuleExports<'a>>,
    typeck: &deka_syntax::typeck::TypeckResult<'a>,
    file_path: &str,
    module_base: Option<String>,
    module_root: Option<&Path>,
    jsx_runtime: Option<String>,
    dev: bool,
) -> Result<DevPlan, Vec<Diagnostic>> {
    let mut slots = Vec::new();
    for stmt in program.statements {
        let Some((binding, value)) = dev_binding(stmt) else {
            continue;
        };
        let Some(info) = typeck.dev_blocks.get(&(value as *const Expr<'a>)) else {
            return Err(vec![Diagnostic::error(
                value.span().start.line,
                value.span().start.column,
                format!("internal compiler error: missing dev metadata for `{binding}`"),
            )]);
        };
        // Hash the project-relative identity when a root is known so the slot
        // id is stable across machine/checkout locations (dsc#61).
        let id = dev_slot_id(
            &dev_slot_source_path(file_path, module_root),
            binding,
            value.span(),
        );
        let entry = emit_dev_entry(
            program,
            source,
            imports,
            module_base.clone(),
            typeck,
            info.body,
            &id,
            file_path,
            module_root.map(Path::to_path_buf),
            jsx_runtime.clone(),
            dev,
        )
        .map_err(|message| {
            vec![Diagnostic::error(
                value.span().start.line,
                value.span().start.column,
                message,
            )]
        })?;
        let descriptor = serde_json::to_value(&info.descriptor).map_err(|error| {
            vec![Diagnostic::error(
                value.span().start.line,
                value.span().start.column,
                format!("failed to serialize dev descriptor: {error}"),
            )]
        })?;
        slots.push(DevPlanSlot {
            id,
            binding: binding.to_string(),
            file: file_path.to_string(),
            span: value.span(),
            descriptor,
            entry,
        });
    }
    let prerender = literal_prerender_export(program)?;
    Ok(DevPlan {
        version: 2,
        prerender,
        slots,
    })
}

/// Literal `export const prerender = <bool>` export, if present. A non-literal
/// `prerender` export is an error: the disposition must be statically known or
/// the host cannot plan routes from the compiler contract (dsc#54).
fn literal_prerender_export(program: &Program) -> Result<Option<bool>, Vec<Diagnostic>> {
    for stmt in program.statements {
        let Stmt::Export {
            decl: deka_syntax::ExportDecl::Const { name, value, .. },
            ..
        } = stmt
        else {
            continue;
        };
        if *name != "prerender" {
            continue;
        }
        return match value {
            Expr::Boolean { value, .. } => Ok(Some(*value)),
            _ => Err(vec![Diagnostic::error(
                value.span().start.line,
                value.span().start.column,
                "`prerender` must be a literal boolean (true or false)".to_string(),
            )]),
        };
    }
    Ok(None)
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
        stdlib_exports.iter().map(|(spec, exports)| (*spec, exports)).collect();
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

    if options.client {
        if let Some(diagnostic) = client_ui_server_error(source) {
            return Err(vec![diagnostic]);
        }
    }

    let package = if Path::new(file_path).is_file() {
        catalog::package_name(file_path)
    } else {
        options.package_name.clone()
    };
    let errors = catalog::validate(
        &program,
        source,
        package.as_deref().is_some_and(|n| n.starts_with("@deka/")),
    );
    if !errors.is_empty() {
        return Err(errors);
    }
    let errors = summon::validate(&program, file_path, &options.foreign_modules);
    if !errors.is_empty() {
        return Err(errors);
    }
    resolve_imported_enum_constructors(&mut program, arena, imports);

    let typeck_result = check_program_with_imports(&program, source, imports);
    if !typeck_result.errors.is_empty() {
        return Err(typeck_result.errors);
    }

    let jsx_runtime = match options.jsx_runtime {
        Some(runtime) if runtime.trim().is_empty() => return Err(vec![Diagnostic::error(0, 0, "jsxRuntime must be a non-empty module specifier")]),
        Some(runtime) => Some(runtime),
        None => project_jsx_runtime(file_path, "jsxRuntime")?,
    };
    let jsx_runtime = if options.dev {
        let explicit = match options.jsx_dev_runtime {
            Some(runtime) if runtime.trim().is_empty() => return Err(vec![Diagnostic::error(0, 0, "jsxDevRuntime must be a non-empty module specifier")]),
            Some(runtime) => Some(runtime),
            None => project_jsx_runtime(file_path, "jsxDevRuntime")?,
        };
        Some(explicit.unwrap_or_else(|| {
            let runtime = jsx_runtime.as_deref().unwrap_or("@js/react/jsx-runtime");
            match runtime.rsplit_once('/') {
                Some((base, _)) => format!("{base}/jsx-dev-runtime"),
                None => "jsx-dev-runtime".into(),
            }
        }))
    } else {
        jsx_runtime
    };
    let mut dev_plan = build_dev_plan(
        &program,
        source,
        imports,
        &typeck_result,
        file_path,
        options.module_base.clone(),
        options.module_root.as_deref(),
        jsx_runtime.clone(),
        options.dev,
    )?;

    let emitted = emit_js_module_with_options(
        &program,
        source,
        imports,
        options.module_base,
        &typeck_result.exception_forms,
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
        &typeck_result.enum_case_patterns,
        &typeck_result.union_type_patterns,
        &typeck_result.effect_deps,
        &typeck_result.memo_sites,
        &typeck_result.dev_blocks,
        &options.build_closure_names,
        file_path,
        options.module_root.clone(),
        options.used_exports.as_ref(),
        options.detached_prelude,
        jsx_runtime,
        options.dev,
        options
            .inject_deka_id
            .unwrap_or_else(|| program_needs_hydration_ids(&program, imports)),
    )
    .map_err(|message| vec![Diagnostic::error(0, 0, message)])?;

    for slot in &mut dev_plan.slots {
        slot.entry = catalog::bundle_helpers(std::mem::take(&mut slot.entry))
            .map_err(|e| vec![Diagnostic::error(1, 1, e)])?;
    }
    Ok(CompileResult {
        js: catalog::bundle_helpers(emitted.js).map_err(|e| vec![Diagnostic::error(1, 1, e)])?,
        diagnostics: typeck_result.warnings,
        demand: emitted.demand,
        dev_plan,
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
    fn react_runtime_override_is_verbatim_and_component_emission_has_no_wrapper() {
        let source = "interface Props { title: string } export fn Card(props: Props) ReactNode { return <p>{props.title}</p>; }";
        let js = compile_to_js_with_options(
            source,
            "card.dsx",
            CompileOptions {
                jsx_runtime: Some("@js/custom/runtime".into()),
                module_base: Some("/stdlib".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .js;
        assert_eq!(
            js,
            concat!(
                "\"use strict\";\nimport { jsx, jsxs, Fragment } from \"@js/custom/runtime\";\n\n\n",
                "export function Card(props) {\nreturn jsx(\"p\", {\"children\": props.title});\n}"
            )
        );
        let errors = compile_to_js_with_options(
            source,
            "card.dsx",
            CompileOptions {
                jsx_runtime: Some(" ".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(errors[0].message.contains("non-empty module specifier"));
    }

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
    fn type_parameter_bounds_erase_from_emitted_js() {
        // rfd#56 phase 2 pin: bounds are a checker-side contract only. The
        // emitted JavaScript for a program with bounds must be byte-identical
        // to the same program without them — the same erasure guarantee phase
        // 1 pinned for bare type parameters.
        let unbounded = r#"
interface Named { name: string }
struct User { name: string }
fn greet<T>(x: T) T { return x }
const u = User { name: "Ada" }
const back = greet(u)
"#;
        let bounded = r#"
interface Named { name: string }
struct User { name: string }
fn greet<T: Named>(x: T) T { return x }
const u = User { name: "Ada" }
const back = greet(u)
"#;
        let union_unbounded_pair = r#"
struct Product { price: number }
struct Bundle { price: number }
fn render<T>(item: T) T { return item }
const widget = Product { price: 5 }
const back = render(widget)
"#;
        let union_bounded_pair = r#"
struct Product { price: number }
struct Bundle { price: number }
fn render<T: Product | Bundle>(item: T) T { return item }
const widget = Product { price: 5 }
const back = render(widget)
"#;
        for (plain, bounded) in [
            (unbounded, bounded),
            (union_unbounded_pair, union_bounded_pair),
        ] {
            let plain_js = compile_to_js(plain, "test.ds")
                .expect("unbounded program compiles")
                .js;
            let bounded_js = compile_to_js(bounded, "test.ds")
                .expect("bounded program compiles")
                .js;
            assert_eq!(
                plain_js, bounded_js,
                "bounds must erase: emitted JS differs\n--- plain ---\n{plain_js}\n--- bounded ---\n{bounded_js}"
            );
        }
    }

    #[test]
    fn compile_dev_binding_emits_virtual_value_and_separate_entry() {
        let source = r#"
struct User { name: string }
const users: Array<User> = build {
  return Ok([User { name: "Ada" }])
}
const first = users.has(0) ? users[0] : User { name: "" }
"#;
        let result = compile_to_js(source, "app/users.ds").expect("dev binding compiles");
        assert_eq!(result.dev_plan.version, 2);
        assert_eq!(result.dev_plan.prerender, None);
        assert_eq!(result.dev_plan.slots.len(), 1);
        let slot = &result.dev_plan.slots[0];
        assert_eq!(slot.binding, "users");
        assert!(
            result.js.contains(&format!("deka:dev/{}", slot.id)),
            "{}",
            result.js
        );
        assert!(
            !result.js.contains("Ada"),
            "runtime output leaked dev body:\n{}",
            result.js
        );
        assert!(
            slot.entry.contains("export default async function"),
            "{}",
            slot.entry
        );
        assert!(slot.entry.contains("Ada"), "{}", slot.entry);
        assert_eq!(slot.descriptor["node"], "array");
    }

    #[test]
    fn compile_build_binding_hydrates_through_declared_factories() {
        let source = r#"
type Cents number
enum Status { Active }
struct User { name: string; balance: Cents; status: Status }
fn (user User) greet() string { return "Hello " + user.name }
const user: User = build {
  return Ok(User { name: "Ada", balance: Cents(7), status: Status.Active })
}
const greeting = user.greet()
"#;
        let result = compile_to_js(source, "app/page.ds").expect("build binding compiles");
        let slot = result.dev_plan.slots.first().expect("one build slot");
        let import = format!("import {{ hydrate as __deka_build_{} }}", slot.id);
        let binding = format!(
            "const user = __deka_build_{}({{Cents, Status, User}});",
            slot.id
        );
        assert!(result.js.contains(&import), "{}", result.js);
        assert!(result.js.contains(&binding), "{}", result.js);
        assert!(
            result
                .js
                .find("User.impl(\"greet\"")
                .zip(result.js.find(&binding))
                .is_some_and(|(method, binding)| method < binding),
            "build hydration must happen after receiver methods:\n{}",
            result.js
        );
        assert!(result.js.contains("user.greet()"), "{}", result.js);
    }

    #[test]
    fn prerender_false_export_appears_in_plan() {
        let source = "export const prerender = false";
        let result = compile_to_js(source, "app/page.ds").expect("prerender export compiles");
        assert_eq!(result.dev_plan.version, 2);
        assert_eq!(result.dev_plan.prerender, Some(false));
        let json = serde_json::to_value(&result.dev_plan).expect("plan serializes");
        assert_eq!(json["prerender"], false);
    }

    #[test]
    fn prerender_true_export_appears_in_plan() {
        let source = "export const prerender = true";
        let result = compile_to_js(source, "app/page.ds").expect("prerender export compiles");
        assert_eq!(result.dev_plan.prerender, Some(true));
    }

    #[test]
    fn prerender_absent_is_omitted_from_serialized_plan() {
        let result = compile_to_js("const x: number = 1", "app/page.ds").expect("compiles");
        assert_eq!(result.dev_plan.prerender, None);
        let json = serde_json::to_value(&result.dev_plan).expect("plan serializes");
        assert!(json.get("prerender").is_none(), "{json}");
    }

    #[test]
    fn prerender_non_literal_export_is_rejected() {
        let errors = compile_to_js(
            "const flag: boolean = true\nexport const prerender = flag",
            "app/page.ds",
        )
        .expect_err("non-literal prerender is rejected");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("literal boolean")),
            "{errors:?}"
        );
    }

    #[test]
    fn compile_dev_slot_id_is_stable_across_project_roots() {
        // The same file at the same project-relative location must produce
        // the same build slot id regardless of the absolute checkout root
        // (dsc#61), and the emitted runtime import must reference that id.
        let source = "const labels: Array<string> = build { return Ok([\"Ada\"]) }";
        let compile_at = |root: &str| {
            compile_to_js_with_options(
                source,
                &format!("{root}/app/page.ds"),
                CompileOptions {
                    module_root: Some(PathBuf::from(root)),
                    ..Default::default()
                },
            )
            .expect("build binding compiles")
        };
        let a = compile_at("/a/proj");
        let b = compile_at("/b/proj");
        assert_eq!(a.dev_plan.slots[0].id, b.dev_plan.slots[0].id);
        let import = format!("deka:dev/{}", a.dev_plan.slots[0].id);
        assert!(
            a.js.contains(&import),
            "emitted JS must import the plan slot id:\n{}",
            a.js
        );
    }

    #[test]
    fn prerender_non_export_const_is_not_route_disposition() {
        // Only the exported binding is a route fact; a local const named
        // `prerender` must not leak into the plan.
        let result = compile_to_js("const prerender: boolean = false", "app/page.ds")
            .expect("compiles");
        assert_eq!(result.dev_plan.prerender, None);
    }

    #[test]
    fn compile_dev_slot_id_keeps_absolute_identity_without_root() {
        // Playground/wasm callers pass no module root: the historical
        // absolute-path identity is preserved (dsc#61).
        let source = "const labels: Array<string> = build { return Ok([\"Ada\"]) }";
        let a = compile_to_js(source, "/a/proj/app/page.ds").expect("compiles");
        let b = compile_to_js(source, "/b/proj/app/page.ds").expect("compiles");
        assert_ne!(a.dev_plan.slots[0].id, b.dev_plan.slots[0].id);
    }

    #[test]
    fn compile_dev_binding_requires_result_return() {
        let source = r#"
const labels: Array<string> = build {
  return ["Ada"]
}
"#;
        let errors = compile_to_js(source, "app/users.ds").expect_err("raw dev value is rejected");
        assert!(
            errors.iter().any(|error| error
                .message
                .contains("expected return type `Result<Array<string>, string>`")),
            "{errors:?}"
        );
    }

    #[test]
    fn compile_dev_binding_requires_explicit_declared_type() {
        let errors = compile_to_js(
            "const labels = build { return Ok([\"Ada\"]) }",
            "app/users.ds",
        )
        .expect_err("untyped dev binding is rejected");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("explicit declared type")),
            "{errors:?}"
        );
    }

    #[test]
    fn build_is_keyword_and_dev_remains_an_identifier() {
        let result = compile_to_js(
            "const dev: string = \"local name\"\nconst value: string = build { return Ok(dev) }",
            "app/users.ds",
        )
        .expect("build binding compiles");
        assert_eq!(result.dev_plan.slots.len(), 1);
        assert!(
            result.js.contains("const dev = \"local name\""),
            "{}",
            result.js
        );
    }

    #[test]
    fn compile_dev_binding_keeps_dev_imports_out_of_runtime_output() {
        let source = r#"
import { load } from "./dev-data.ds"
import { greeting } from "./runtime.ds"
const users: string = build {
  return Ok(load())
}
const message: string = greeting()
"#;
        let arena = Bump::new();
        let mut dev_exports = ModuleExports::default();
        dev_exports.values.insert(
            "load",
            Type::Function {
                params: vec![],
                ret: Box::new(Type::Named { name: "string" }),
                optional: 0,
            },
        );
        let mut runtime_exports = ModuleExports::default();
        runtime_exports.values.insert(
            "greeting",
            Type::Function {
                params: vec![],
                ret: Box::new(Type::Named { name: "string" }),
                optional: 0,
            },
        );
        let imports = HashMap::from([
            ("./dev-data.ds", &dev_exports),
            ("./runtime.ds", &runtime_exports),
        ]);
        let result = compile_to_js_with_imports(source, "app/users.ds", &arena, &imports)
            .expect("declared imports compile");
        let slot = result.dev_plan.slots.first().expect("one dev slot");
        assert!(!result.js.contains("./dev-data.ds"), "{}", result.js);
        assert!(result.js.contains("./runtime.ds"), "{}", result.js);
        assert!(slot.entry.contains("./dev-data.js"), "{}", slot.entry);
        assert!(!slot.entry.contains("./runtime.ds"), "{}", slot.entry);
    }

    #[test]
    fn compile_dev_binding_requires_typed_module_const() {
        let source = r#"
fn load() string {
  const value: string = build { return Ok("Ada") }
  return value
}
"#;
        let errors =
            compile_to_js(source, "app/users.ds").expect_err("nested dev binding is rejected");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("module-level `const`")),
            "{errors:?}"
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
        assert!(
            !with_struct.js.contains("v.__enum"),
            "got:\n{}",
            with_struct.js
        );
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
        assert!(!result.js.contains("__case"));
        assert!(result.js.contains("!== undefined"));
        assert!(result.js.contains("else {"));
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
            result
                .js
                .contains("__deka_match_scrutinee_1?.__deka_struct === \"Point\""),
            "got: {}",
            result.js
        );
        assert!(
            result
                .js
                .contains("typeof __deka_match_scrutinee_1 === \"string\""),
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
        assert!(
            result.js.contains("const Point = __deka_struct"),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("Point.impl(\"distance\""),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("p1.distance(p2)"), "got: {}", result.js);
    }

    #[test]
    fn compile_generic_type_parameters_erase() {
        // rfd#56: types erase. `Signal<T>` with its constructor and a
        // receiver method must emit exactly what the non-generic `Signal`
        // spelling emits — the type parameter never reaches output.
        let concrete = compile_to_js(
            "struct Signal { value: number }\n\
             fn signal(initial: number) Signal { return Signal { value: initial } }\n\
             fn (s mut Signal) set(next: number) { s.value = next; }\n\
             let count = signal(0)\n\
             count.set(1)\n\
             const v: number = count.value",
            "test.ds",
        )
        .expect("concrete compile should succeed");
        let generic = compile_to_js(
            "struct Signal<T> { value: T }\n\
             fn signal<T>(initial: T) Signal<T> { return Signal { value: initial } }\n\
             fn (s mut Signal) set<T>(next: T) { s.value = next; }\n\
             let count = signal(0)\n\
             count.set(1)\n\
             const v: number = count.value",
            "test.ds",
        )
        .expect("generic compile should succeed");
        assert_eq!(concrete.js, generic.js);
        assert!(
            !generic.js.contains("<"),
            "a type parameter leaked into output:\n{}",
            generic.js
        );
    }

    #[test]
    fn unresolved_import_is_an_error_boundary_not_infer() {
        let errors = compile_to_js(
            "import { add } from \"./math.ds\";\nconst r: number = add.missing(1);",
            "test.ds",
        )
        .expect_err("unresolved imports must fail before their uses can typecheck");

        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert_eq!(errors[0].line, 1);
        assert!(errors[0].message.contains("imported name `add`"));
        assert!(errors[0].message.contains("./math.ds"));
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
        assert!(
            result.js.contains("User({ id: 1, name: \"D\" })"),
            "got:\n{}",
            result.js
        );
        // getType() rewrites to the module-local tag read; the brand id is
        // the struct name, so cross-module identity is by value, not object.
        assert!(
            result.js.contains("__deka_type_of(u)"),
            "got:\n{}",
            result.js
        );
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

    /// dsc#58: a struct literal written inside an `unsafe` body is raw
    /// JavaScript text, and `User { ... }` is a syntax error in JavaScript
    /// expression position — arrow bodies in particular took the whole
    /// emitted module down with them. The emitter rewrites the known-struct
    /// spelling to the factory call the syntax denotes.
    #[test]
    fn unsafe_struct_literal_rewrites_to_factory_call() {
        let source = r#"
struct User { name: string }
const direct = unsafe { User { name: "Ada" } }
const arrow = unsafe { () => User { name: "Bob" } }
"#;
        let result = compile_to_js(source, "app/unsafe.ds").expect("compile should succeed");
        assert!(
            result.js.contains("User({ name: \"Ada\" })"),
            "direct struct literal must become a factory call:\n{}",
            result.js
        );
        assert!(
            result.js.contains("() => User({ name: \"Bob\" })"),
            "arrow-body struct literal must become a factory call:\n{}",
            result.js
        );
    }

    /// dsc#58: the rewrite must not touch text where `Identifier {` is
    /// already valid JavaScript — class declarations, strings, templates.
    #[test]
    fn unsafe_rewrite_preserves_valid_js_named_like_structs() {
        let source = "struct User { name: string }\n\
                      const c = unsafe { class User { constructor() { this.n = 1 } } ; 5 }\n\
                      const s = unsafe { \"User { not: code }\" }";
        let result = compile_to_js(source, "app/unsafe.ds").expect("compile should succeed");
        assert!(
            result.js.contains("class User {"),
            "class declaration must pass through verbatim:\n{}",
            result.js
        );
        assert!(
            !result.js.contains("class User({"),
            "class declaration must not be rewritten:\n{}",
            result.js
        );
        assert!(
            result.js.contains("\"User { not: code }\""),
            "string literal must pass through verbatim:\n{}",
            result.js
        );
    }

    /// dsc#51: `import { User as Person }` binds the union type-pattern to
    /// the local name, but the runtime factory was instantiated under the
    /// declared name, so the brand tag reads "User". The emitted test must
    /// compare against the declared brand — testing the local spelling
    /// never matches and the match falls through to non-exhaustive.
    #[test]
    fn aliased_import_union_type_pattern_uses_declared_brand() {
        use bumpalo::Bump;
        use deka_syntax::{collect_module_exports, parse};
        use std::collections::HashMap;
        let arena = Bump::new();
        let lib_src = "struct User { name: string }\n\
                       fn (u User) greet() string { return \"hi \" + u.name }\n\
                       export { User }";
        let lib_parse = parse(lib_src, &arena);
        assert!(lib_parse.errors.is_empty(), "{:?}", lib_parse.errors);
        let lib_program = lib_parse.program.unwrap();
        let lib_exports = collect_module_exports(&lib_program, &arena);

        let main_src = "import { User as Person } from \"./lib.ds\";\n\
                        fn describe(e: Person | number) string {\n\
                          return match (e) {\n\
                            Person(p) => p.greet(),\n\
                            number(n) => string(n),\n\
                          }\n\
                        }";
        let result = compile_to_js_with_imports(main_src, "main.ds", &arena, &{
            let mut m: HashMap<&str, &deka_syntax::typeck::ModuleExports> = HashMap::new();
            m.insert("./lib.ds", &lib_exports);
            m
        })
        .expect("compile should succeed");
        assert!(
            result.js.contains("__deka_struct === \"User\""),
            "brand test must use the declared name:\n{}",
            result.js
        );
        assert!(
            !result.js.contains("__deka_struct === \"Person\""),
            "brand test must not use the local alias:\n{}",
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
            lib_exports
                .structs
                .get("User")
                .map(|i| i.is_super)
                .unwrap_or(false),
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
        assert!(
            result.js.contains("__deka_super_desc$User"),
            "got:\n{}",
            result.js
        );
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
        // dsc#88: object string-key indexing (`o["x"]`) is a check-time
        // error now, so the index half of this test uses array indexing.
        let result = compile_to_js(
            "const a = [1, 2, 3]; const o = { x: 1 }; const v = a.has(0) && a.has(1) ? a[0] + a[1] + o.x : 0;",
            "test.ds",
        )
        .expect("compile should succeed");
        // deka#590 step 2: const literals are no longer frozen at emit; the
        // checker (deka#591) rejects mutation of a const-bound collection.
        assert!(
            result.js.contains("const a = [1, 2, 3];"),
            "got: {}",
            result.js
        );
        assert!(
            !result.js.contains("Object.freeze([1, 2, 3])"),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("const o = {x: 1};"),
            "got: {}",
            result.js
        );
        assert!(
            !result.js.contains("Object.freeze({x: 1})"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("a[0] + a[1] + o.x"), "got: {}", result.js);
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
        assert!(result.js.contains("ok: true"), "got: {}", result.js);
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
        assert!(
            result.js.contains("const Legs = __deka_struct"),
            "got: {}",
            result.js
        );
        assert!(
            result
                .js
                .contains("const Robot = __deka_struct(\"Robot\", { Legs: Legs })"),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("Legs.impl(\"move\""),
            "got: {}",
            result.js
        );
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
                .contains("import { jsx, jsxs, Fragment } from \"@js/react/jsx-runtime\""),
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
    fn compile_jsx_hyphenated_attributes() {
        // dsc#69: hyphenated HTML attribute names must survive into the
        // emitted props object — user-authored `data-*` names flow through
        // untouched once the parser accepts them.
        let result = compile_to_js(
            "const n = 1; const el = <p data-x=\"1\" aria-label=\"y\" data-count={n - 1}>z</p>;",
            "test.dsx",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("\"data-x\": \"1\""),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("\"aria-label\": \"y\""),
            "got: {}",
            result.js
        );
        // The subtraction inside the attribute value is untouched.
        assert!(result.js.contains("n - 1"), "got: {}", result.js);
    }

    #[test]
    fn compile_subtraction_untouched_by_hyphenated_attributes() {
        // dsc#69: joining `-` in attribute-name position must not change how
        // ordinary subtraction compiles.
        let result = compile_to_js("const a = 10; const b = 3; const n = a - b; const m = a - 1;", "test.ds")
            .expect("compile should succeed");
        assert!(result.js.contains("a - b"), "got: {}", result.js);
        assert!(result.js.contains("a - 1"), "got: {}", result.js);
    }

    #[test]
    fn compile_jsx_keyword_attributes() {
        // dsc#77: keyword attribute names must survive into the emitted props
        // object — `type`/`for` are the two most common form attributes, and
        // the accessible label/input association is `for` + `id`.
        let result = compile_to_js(
            "const el = <form><label for=\"email\">Email</label><input type=\"text\" id=\"email\" /><button type=\"submit\">Send</button></form>;",
            "test.dsx",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("\"for\": \"email\""),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("\"type\": \"text\""),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("\"id\": \"email\""),
            "got: {}",
            result.js
        );
        assert!(
            result.js.contains("\"type\": \"submit\""),
            "got: {}",
            result.js
        );
    }

    #[test]
    fn compile_keywords_untouched_by_jsx_attribute_names() {
        // dsc#77 regression pin: outside attribute-name position, `for`,
        // `if`/`else`, and `type` keep their keyword meaning in the emitted
        // JS.
        let result = compile_to_js(
            "type Email string; const xs = [1]; let total = 0; for (const x of xs) { if (x > 0) { total = total + x } else { total = total } }",
            "test.ds",
        )
        .expect("compile should succeed");
        assert!(
            result.js.contains("for (const x of xs)"),
            "got: {}",
            result.js
        );
        assert!(result.js.contains("if (x > 0)"), "got: {}", result.js);
        assert!(result.js.contains("else"), "got: {}", result.js);
    }

    #[test]
    fn unknown_and_retired_ui_imports_are_not_virtual_stdlib() {
        for spec in [
            "@deka/anything",
            "ui",
            "ui/button",
            "ui/form",
            "@deka/ui/button",
        ] {
            assert!(!is_stdlib_module_spec(spec), "{spec}");
            let source = format!("import {{ missing }} from \"{spec}\"; const value = missing;");
            let arena = Bump::new();
            assert!(!infer_stdlib_imports_for_source(&source, &arena).contains_key(spec));
            assert!(compile_to_js(&source, "test.ds").is_err(), "{spec}");
        }
    }

    #[test]
    fn math_module_exposes_only_typed_pi_as_a_local_runtime_binding() {
        let result = compile_to_js(
            "import { PI } from \"math\";\nconst circumference: number = PI * 2;",
            "test.ds",
        )
        .expect("the compiler-provided math module must typecheck");
        assert!(
            result.js.contains("const PI = Math.PI;"),
            "math PI was not lowered to its local binding: {}",
            result.js
        );
        assert!(
            result.js.contains("const circumference = PI * 2;"),
            "PI did not retain its number type: {}",
            result.js
        );
        assert!(
            !result.js.contains("from \"math\""),
            "math must not require a host package: {}",
            result.js
        );

        let errors = compile_to_js("import { E } from \"math\";", "test.ds")
            .expect_err("the closed math module must not gain unapproved constants");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("cannot resolve imported name `E` from `math`")),
            "unexpected closed-module diagnostic: {errors:?}"
        );
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

    // ------------------------------------------------------------------
    // JSX text (deka#67, deka#68)
    // ------------------------------------------------------------------

    #[test]
    fn compile_jsx_text_survives_byte_for_byte() {
        // The dsc#67 done-when fixture: every character — Latin-1 accents,
        // emoji, CJK, Greek, Cyrillic, Arabic, Ge'ez, combining marks, and
        // every character the code lexer rejects (`' $ £ # @ ~ \``) — must
        // survive byte-for-byte into the emitted JS string literal.
        let source = r#"export fn P() {
  return <section>
    <h1>Café Yirgacheffe — 18,50 €</h1>
    <p>Ethiopian የቡና ☕ — don't miss it: $18.50 / £15, #1 seller @ 100% arabica</p>
    <p>日本語 · Ελληνικά · Русский · العربية</p>
  </section>
}
"#;
        let result = compile_to_js(source, "page.dsx").expect("compile should succeed");
        for expected in [
            "Café Yirgacheffe — 18,50 €",
            "Ethiopian የቡና ☕ — don't miss it: $18.50 / £15, #1 seller @ 100% arabica",
            "日本語 · Ελληνικά · Русский · العربية",
        ] {
            assert!(
                result.js.contains(expected),
                "emitted JS must contain {:?} verbatim, got:\n{}",
                expected,
                result.js
            );
        }
        // Every non-ASCII character of the source must appear in the output.
        for ch in source.chars().filter(|c| !c.is_ascii()) {
            assert!(
                result.js.contains(ch),
                "emitted JS lost non-ASCII character {:?}\n{}",
                ch,
                result.js
            );
        }
    }

    #[test]
    fn compile_multiscript_identifiers_strings_comments_survive_byte_for_byte() {
        // dsc#70: multi-script text must compile, not just fail gracefully —
        // a VALID program with CJK, Arabic, Cyrillic, Ge'ez, emoji with a
        // variation selector, and combining marks in identifiers, string
        // bodies, and comments is emitted byte-for-byte.
        let source = "const café = \"日本語 · العربية · የቡና ☕\u{FE0F} cafe\u{301}\"\n\
                      // Русский comment 日本語\n\
                      const العربية = café\n";
        let result = compile_to_js(source, "multi.ds").expect("compile should succeed");
        for expected in ["café", "日本語 · العربية · የቡና ☕\u{FE0F} cafe\u{301}", "العربية"] {
            assert!(
                result.js.contains(expected),
                "emitted JS must contain {:?} verbatim, got:\n{}",
                expected,
                result.js
            );
        }
    }

    #[test]
    fn diagnostic_columns_count_characters_not_bytes() {
        // A byte-counting column would report 15 (é is two bytes) and would
        // be shifted by line 1's multi-byte comment if line state leaked;
        // characters are what count (dsc#70 done-when).
        let err = compile_to_js("// 日本語 comment\nconst café = ©", "col.ds")
            .expect_err("compile should fail");
        assert!(
            err.iter()
                .any(|d| d.message.contains("unexpected character")),
            "expected unexpected-character diagnostic, got: {err:?}"
        );
        assert_eq!(
            (err[0].line, err[0].column),
            (2, 14),
            "diagnostic must point at the character, not the byte: {err:?}"
        );
    }

    /// dsc#72: input nested up to the parser's recursion limit (64 levels)
    /// must compile end-to-end (parse -> typeck -> emit). Runs on the ~2 MiB
    /// test-thread stack — the tightest stack any real caller has — so it
    /// also pins the stack-safety of the depth limit for the later passes,
    /// which recurse over the AST in step with its (now bounded) depth.
    #[test]
    fn deep_nesting_compiles_below_limit() {
        // 64 nested blocks, one statement frame per level: exactly the limit.
        let source = format!("{}{}", "{".repeat(64), "}".repeat(64));
        compile_to_js(&source, "deep.ds").expect("nesting at the limit must compile");

        // Deep-but-legal expression nesting in a typed binding.
        let source = format!("const x = {}{}{}", "(".repeat(60), "1", ")".repeat(60));
        compile_to_js(&source, "deep.ds").expect("60-deep parens must compile");
    }

    /// dsc#72: input nested past the limit must fail with the positioned
    /// `nesting too deep` diagnostic — never the uncatchable SIGABRT a stack
    /// overflow produces. Covers every recursive parse path: expressions,
    /// blocks, and JSX children.
    #[test]
    fn deep_nesting_beyond_limit_is_a_positioned_diagnostic() {
        for source in [
            format!("const x = {}{}{}", "(".repeat(1000), "1", ")".repeat(1000)),
            "{".repeat(1000),
            format!(
                "export fn P() {{ return {}text{} }}",
                "<div>".repeat(1000),
                "</div>".repeat(1000)
            ),
        ] {
            let errors = compile_to_js(&source, "deep.dsx").expect_err("must not compile");
            assert!(
                errors.iter().any(|e| e.message.contains("nesting too deep")),
                "{errors:?}"
            );
            for e in &errors {
                assert!(e.line >= 1 && e.column >= 1, "unpositioned: {e:?}");
            }
        }
    }

    /// Feeds exotic and malformed source through the full compile pipeline
    /// (lexer -> parser -> typecheck -> emit). The contract under test: user
    /// input never panics and never yields a diagnostic without a line and
    /// column. A panic fails the test naturally; nothing catches here.
    ///
    /// Non-ASCII coverage now spans every lexer path, not just JSX text
    /// (dekaruntime/dsc#70), and quotes/backticks may sit at EOF
    /// (dekaruntime/dsc#71). Deeply nested input goes far past the parser's
    /// recursion limit (64 levels, dsc#72) and must surface the positioned
    /// `nesting too deep` diagnostic — never the uncatchable stack-overflow
    /// abort, which would kill this whole test binary.
    #[test]
    fn malformed_input_never_panics_and_diagnostics_are_positioned() {
        let mut corpus: Vec<String> = Vec::new();

        // Every ASCII character rejected by deka#67 in every position: bare,
        // in code, in strings, and in JSX text/attributes.
        for c in ['$', '#', '@', '~', '`', '\'', '"', '\\'] {
            for template in [
                "{c}",
                "const x = {c}",
                "const x = \"{c}\"",
                "fn f() {{ return {c} }}",
                "export fn P() {{ return <p>{c}</p> }}",
                "export fn P() {{ return <p a={c} /> }}",
                "export fn P() {{ return <p>{c} {c} {c}</p> }}",
            ] {
                corpus.push(template.replace("{c}", &c.to_string()));
            }
        }
        // Multi-byte characters in EVERY position: JSX text, attribute
        // values, string literals, bare/in-code (dsc#70), and in comments.
        // Includes CJK, Arabic, Cyrillic, Ge'ez, emoji with variation
        // selector, combining marks, the replacement char, and a BOM.
        for c in ['€', '£', '©', '日', '\u{FFFD}', '\u{FEFF}', '☕', 'é'] {
            for template in [
                "{c}",
                "const x = {c}",
                "const x = \"{c}\"",
                "const caf{c} = 1",
                "// comment {c}\nconst x = 1",
                "fn f() {{ return {c} }}",
                "export fn P() {{ return <p>{c}</p> }}",
                "export fn P() {{ return <p a=\"{c}\" /> }}",
                "export fn P() {{ return <p>{c} {c} {c}</p> }}",
            ] {
                corpus.push(template.replace("{c}", &c.to_string()));
            }
        }
        // Multi-script soup: several scripts in one identifier, one string,
        // and one comment, valid and malformed (unterminated variants).
        for source in [
            "const 日本語_العربية_ identifier = 1",
            "const s = \"日本語 · Ελληνικά · Русский · العربية · የቡና ☕\u{FE0F} cafe\u{301}\"",
            "// 日本語 العربية Русский የቡና ☕\u{FE0F} cafe\u{301}\nconst x = 1",
            "const s = \"日本語",
            "// የቡና caf",
            "const café = '日本語 ☕\u{FE0F}",
            "unsafe { const s = `العربية } still string`; }",
        ] {
            corpus.push(source.to_string());
        }

        // Deeply nested constructs, balanced and unclosed. Depths go well past
        // the parser's recursion limit (64, dsc#72) so the harness exercises
        // the depth-limit diagnostic path — thousands of levels must produce
        // a positioned diagnostic, never a stack-overflow abort (the abort is
        // uncatchable and would kill this whole test binary).
        for open_close in [
            ("<div>", ""),
            ("<div><span>", ""),
            ("<div>", "</span>"),
            ("[", "]"),
            ("{", "}"),
            ("(", ")"),
            ("f(", ")"),
            ("Option<", ">"),
            ("/*", "*/"),
            ("unsafe {", "}"),
        ] {
            for depth in [1usize, 8, 32, 256] {
                let mut s = String::new();
                for _ in 0..depth {
                    s.push_str(open_close.0);
                }
                for _ in 0..depth {
                    s.push_str(open_close.1);
                }
                corpus.push(s);
            }
            // Fully unclosed variants (256 deep; the parser's depth limit
            // makes these cheap — the diagnostic fires at level 65).
            let mut s = String::new();
            for _ in 0..256 {
                s.push_str(open_close.0);
            }
            corpus.push(s);
        }
        // Balanced quote/backtick nesting; unterminated-at-EOF variants are
        // in the multi-byte section above and in the tripwire below.
        for quote in ["\"", "'", "`"] {
            for depth in [1usize, 8, 32] {
                let mut s = String::new();
                for _ in 0..depth {
                    s.push_str(quote);
                }
                for _ in 0..depth {
                    s.push_str(quote);
                }
                corpus.push(s);
            }
        }

        // Deterministic pseudo-random ASCII soup (xorshift64) in both flat
        // and code-shaped forms; multi-byte soup is covered above.
        let mut state = 0x243F6A8885A308D3u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..64 {
            let len = (next() % 200) as usize;
            let s: String = (0..len)
                .map(|_| (32 + (next() % 95)) as u8 as char)
                .collect();
            corpus.push(s);
        }

        // Structured soup: random slivers of plausible-looking code.
        let fragments = [
            "export fn", "return", "<div>", "</div>", "{c}", "\"", "'",
            "<p>", "$", "const x =", "match (x) {", "=>", "}}}", "{{{",
            "unsafe {", "`x", "\n", "fn f(", ": number", "import", "from",
            ".dsx", "caf", "1_000",
        ];
        for _ in 0..200 {
            let len = (next() % 12) as usize;
            let mut s = String::new();
            for _ in 0..len {
                s.push_str(fragments[(next() as usize) % fragments.len()]);
                s.push(' ');
            }
            corpus.push(s);
        }

        for source in &corpus {
            for path in ["fuzzer.ds", "fuzzer.dsx"] {
                match compile_to_js(source, path) {
                    Ok(_) => {}
                    Err(diags) => {
                        assert!(
                            !diags.is_empty(),
                            "compile failed without diagnostics for {:?} ({})",
                            source,
                            path
                        );
                        for d in &diags {
                            assert!(
                                d.line >= 1 && d.column >= 1,
                                "diagnostic without line/column for {:?} ({}): {:?}",
                                source,
                                path,
                                d
                            );
                        }
                    }
                }
            }
        }
    }

    /// Tripwire for the code-lexer panics once tracked as known/pre-existing
    /// in the dsc#67 PR and fixed by dsc#70/dsc#71. It keeps the exact corpus
    /// that used to panic — non-ASCII characters outside JSX text, BOM-prefixed
    /// files, lossy-decoded truncated UTF-8, and one-character unterminated
    /// strings/backticks at EOF — but the expectation is inverted: every input
    /// must now compile or fail with positioned diagnostics, NEVER panic. If a
    /// fix regresses and any of these starts crashing the compiler again,
    /// this test fails. Do not delete it and do not delete inputs from it.
    #[test]
    fn known_pre_existing_lexer_panics_are_tracked() {
        let mut former: Vec<String> = Vec::new();
        // dsc#70: non-ASCII outside JSX text.
        for template in [
            "const x = {c}",
            "{c}",
            "const {c}foo = 1",
            "const x = \"{c}\" + caf\u{FFFD}",
        ] {
            for c in ['©', '€', '\u{FEFF}', '\u{FFFD}', 'é'] {
                former.push(template.replace("{c}", &c.to_string()));
            }
        }
        for bytes in [
            &b"caf\xc3"[..],                   // truncated 2-byte char
            &b"\xed\xa0\x80"[..],              // UTF-8-encoded lone surrogate
            &b"\xef\xbb\xbfconst x = 1"[..],   // BOM-prefixed file
            &b"\xf0\x9f\x8e"[..],              // truncated emoji
        ] {
            former.push(String::from_utf8_lossy(bytes).into_owned());
        }
        // dsc#71: unterminated one-character string/backtick at EOF.
        for source in ["\"", "'", "`", "const x = '", "const x = `"] {
            former.push(source.to_string());
        }

        for source in &former {
            // These inputs used to panic with `byte index N is not a char
            // boundary` or a `byte range starts at .. but ends at ..`
            // underflow; a panic here now fails the test naturally.
            match compile_to_js(source, "former.ds") {
                Ok(_) => {}
                Err(diags) => {
                    assert!(
                        !diags.is_empty(),
                        "compile failed without diagnostics for {source:?}"
                    );
                    for d in &diags {
                        assert!(
                            d.line >= 1 && d.column >= 1,
                            "diagnostic without line/column for {source:?}: {d:?}"
                        );
                    }
                }
            }
        }
    }

    /// Byte-truncation harness: every byte-truncation prefix of valid
    /// multi-script sources — including the dsc#67 done-when fixture — fed
    /// through the full compile pipeline. Prefixes that cut a UTF-8 sequence
    /// in half are lossy-decoded to U+FFFD, which is itself an invalid-input
    /// case (dsc#70). The contract: diagnostic or success, NEVER panic/abort.
    /// Catches the whole dsc#71 family (any construct whose last byte is an
    /// opener) rather than just the pinned one-character cases.
    #[test]
    fn truncation_of_multiscript_sources_never_panics() {
        let fixture = "const el = <p>Café Yirgacheffe — 日本語 · العربية · ☕</p>\n\
                       const result = unsafe { deka.ui.renderToString(el) }\n\
                       match (result) { Ok(r) => r.html, Err(e) => e }\n"
            .to_string();
        // A plain-code counterpart: multi-script identifiers, strings, and
        // comments outside JSX, so the truncation covers dsc#70's paths too.
        let plain = "const café = \"日本語 · العربية · የቡና ☕\u{FE0F} cafe\u{301}\"\n\
                     // Русский comment 日本語\n\
                     export fn f() number { return 1 }\n";
        let sources = [fixture, plain.to_string()];
        let mut total = 0usize;
        let mut succeeded = 0usize;
        let mut diagnosed = 0usize;
        for source in &sources {
            for end in 0..=source.len() {
                total += 1;
                let prefix = String::from_utf8_lossy(&source.as_bytes()[..end]).into_owned();
                match compile_to_js(&prefix, "truncated.ds") {
                    Ok(_) => succeeded += 1,
                    Err(diags) => {
                        diagnosed += 1;
                        assert!(
                            !diags.is_empty(),
                            "compile failed without diagnostics for prefix {end}"
                        );
                        for d in &diags {
                            assert!(
                                d.line >= 1 && d.column >= 1,
                                "diagnostic without line/column at prefix {end}: {d:?}"
                            );
                        }
                    }
                }
            }
        }
        eprintln!(
            "truncation harness: {total} prefixes: {succeeded} success, {diagnosed} diagnostics, 0 panics"
        );
        assert!(total > 0);
    }
}
