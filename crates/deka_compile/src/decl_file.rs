//! `.d.ds` declaration files (rfd#39, 2026-09-16 amendment; dsc#274).
//!
//! A declaration file holds signatures only — `export opaque type`,
//! `export fn`, `export total fn` — and sits beside the `.mjs` it describes.
//! The module graph treats it as an ordinary graph node (parsed, typechecked
//! via [`deka_syntax::collect_module_exports`], propagated to importers) so
//! every existing cross-module mechanism — import validation, re-export
//! chains, opaque nominal identity — applies unchanged. Two things are
//! specific to a declaration file:
//!
//! 1. Its grammar is restricted to declarations only ([`validate_shape`]);
//!    a function body is a hard error (item 1 of the amendment).
//! 2. Its declared functions are checked, not trusted, against the real
//!    sibling module ([`verify_structure`]), reusing the existing summon
//!    tier-1 structural verifier (`crate::summon`) rather than a second
//!    implementation of "does this export exist, is it a function, is the
//!    arity compatible" (item 5).
//!
//! `.d.ts` resolution (dsc#276) and `declare module` / project overrides
//! (dsc#275) are separate issues; [`sibling_declaration_path`] is the seam
//! the former plugs a `.d.ts` fallback into.
use bumpalo::Bump;
use deka_syntax::{Diagnostic, ExportDecl, Program, Stmt, SummonedFunction};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// True for a path whose file name ends in `.d.ds`. Deliberately a suffix
/// check on the file name, not `Path::extension()`: `extension()` on
/// `three.d.ds` returns `"ds"`, indistinguishable from a plain `three.ds`.
pub fn is_declaration_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".d.ds"))
}

/// `./three.mjs` / `./three.js` -> `./three.d.ds`, beside it.
pub fn sibling_declaration_path(js_path: &Path) -> Option<PathBuf> {
    let name = js_path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".mjs").or_else(|| name.strip_suffix(".js"))?;
    Some(js_path.with_file_name(format!("{stem}.d.ds")))
}

/// `./three.d.ds` -> `./three.mjs`, the module it describes.
pub fn sibling_module_path(decl_path: &Path) -> Option<PathBuf> {
    let name = decl_path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".d.ds")?;
    Some(decl_path.with_file_name(format!("{stem}.mjs")))
}

/// The "no declarations, no import" diagnostic (item 4): a resolvable `.mjs`/
/// `.js` module with no sibling `.d.ds` (and, until dsc#276, no `.d.ts`).
/// Names both fixes, as the amendment requires.
pub fn missing_declaration_message(specifier: &str, decl_path: &Path) -> String {
    let decl_name = decl_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<module>.d.ds");
    format!(
        "importing '{specifier}' needs a declaration: write {decl_name} beside it, or run `dsc summon infer {specifier}`"
    )
}

/// Every top-level statement in a `.d.ds` file must be `export opaque type`
/// or a bodyless `export (total)? fn` — the same two forms the parser
/// produces (universally, so `declare module { }` blocks can share the
/// grammar, dsc#275); everything else, including a function *with* a body,
/// is rejected here where the file kind is known.
pub fn validate_shape(program: &Program<'_>) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for stmt in program.statements.iter() {
        match stmt {
            Stmt::Export {
                decl: ExportDecl::Opaque { .. } | ExportDecl::Declare(_),
                ..
            } => {}
            Stmt::Export {
                decl: ExportDecl::Function { .. },
                span,
            } => {
                diagnostics.push(Diagnostic::error(
                    span.start.line,
                    span.start.column,
                    "a declaration file (`.d.ds`) may not contain a function body; write the signature only (`export fn name(params) Return`, no body)",
                ));
            }
            other => {
                let span = other.span();
                diagnostics.push(Diagnostic::error(
                    span.start.line,
                    span.start.column,
                    "a declaration file (`.d.ds`) may only contain `export opaque type`, `export fn`, and `export total fn`",
                ));
            }
        }
    }
    diagnostics
}

/// Tier-1 structural verification (rfd#39's 2026-09-12 amendment, section 1;
/// re-asserted for declaration files by the 2026-09-16 amendment, item 5):
/// every declared function's export must exist, be a function, and have a
/// compatible arity in the real sibling module. This wraps the declared
/// functions in a synthetic `summon { … } from "…"` statement and hands it to
/// `crate::summon::validate` unchanged — the same tier-1 walk a `summon`
/// block gets — rather than re-implementing the JS export walk.
pub fn verify_structure<'a>(
    decl_path: &Path,
    program: &'a Program<'a>,
    arena: &'a Bump,
    virtual_modules: &HashMap<String, String>,
    skip_fs: bool,
) -> Vec<Diagnostic> {
    let functions: Vec<SummonedFunction<'a>> = program
        .statements
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Export {
                decl: ExportDecl::Declare(function),
                ..
            } => Some(function.clone()),
            _ => None,
        })
        .collect();
    if functions.is_empty() {
        return Vec::new();
    }
    let Some(mjs_path) = sibling_module_path(decl_path) else {
        return Vec::new();
    };
    let Some(mjs_name) = mjs_path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let source = deka_syntax::alloc_str(arena, &format!("./{mjs_name}"));
    let synthetic = Program {
        statements: deka_syntax::alloc_slice(
            arena,
            vec![Stmt::Summon {
                functions: deka_syntax::alloc_slice(arena, functions),
                source,
                span: program.span,
            }],
        ),
        span: program.span,
        has_top_level_await: false,
    };
    crate::summon::validate(
        &synthetic,
        &decl_path.to_string_lossy(),
        virtual_modules,
        skip_fs,
    )
}
