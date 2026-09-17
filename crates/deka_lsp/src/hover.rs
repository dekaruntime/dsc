//! Hover: the declared signature or type of the name under the cursor.
//!
//! Locals, params and top-level items come from the module's own parse via
//! [`deka_syntax::declarations_in_scope_at_offset`] — the same scope query
//! completion uses, so hover and completion can never disagree about what is
//! in scope. Imported names resolve through the project loader to the
//! exporting module's own parse, so a hover on `greeting` from
//! `import { greeting } from './greet'` shows the signature as declared in
//! `greet.ds`, including unsaved buffers.

use super::*;
use deka_syntax::ast::{Expr, ExportDecl, Stmt};

/// Hover markdown for the identifier at `offset` in `text`, or `None` when
/// the cursor is not on a resolvable name. Annotation hovers and the legacy
/// import-statement hover keep their existing behavior.
pub(crate) fn entry_hover(
    documents: &HashMap<Url, String>,
    text: &str,
    file_path: &str,
    offset: usize,
) -> Option<String> {
    if let Some(hover) = hover_for_annotation(text, offset) {
        return Some(hover);
    }
    let arena = bumpalo::Bump::new();
    let program = deka_syntax::parse_recovering(text, &arena).program;
    if let Some(program) = program.as_ref() {
        if let Some(hover) = bridge_call_hover(program, offset) {
            return Some(hover);
        }
    }
    let word = word_at_offset(text.as_bytes(), offset)?;
    if let Some(program) = program.as_ref() {
        let declarations = deka_syntax::declarations_in_scope_at_offset(program, offset);
        if let Some(decl) = declarations.iter().find(|decl| decl.name == word) {
            if decl.kind == deka_syntax::ScopeItemKind::Import {
                if let Some(hover) =
                    imported_name_hover(documents, file_path, program, decl.name)
                {
                    return Some(hover);
                }
            }
            return Some(fenced(&decl.detail));
        }
    }
    // Mid-edit or legacy fallback: the import statement the cursor sits on.
    hover_from_import(text, offset)
}

/// The `kind`/`action` of the `bridge kind.action(...)` call whose span
/// contains `offset`, if any. Bridge calls are compiler syntax, not a scoped
/// name, so they are found by walking the parsed program rather than through
/// `declarations_in_scope_at_offset`.
pub(crate) fn bridge_call_at(program: &deka_syntax::Program, offset: usize) -> Option<(String, String)> {
    // Owned strings, not `&'a str`: `walk_stmt`'s callback takes `&Expr<'_>`
    // with a fresh lifetime per call, so a borrow from inside it cannot
    // escape (the same reason `deka_compile::catalog`'s scan collects into
    // owned `String`s rather than borrowing).
    let mut found = None;
    for stmt in program.statements {
        deka_syntax::visit::walk_stmt(stmt, &mut |expr| {
            if found.is_some() {
                return;
            }
            if let Expr::Bridge {
                kind, action, span, ..
            } = expr
            {
                if span.byte_start <= offset && offset <= span.byte_end {
                    found = Some((kind.to_string(), action.to_string()));
                }
            }
        });
    }
    found
}

/// Hover on a `bridge kind.action(...)` call: the declared signature from
/// dsc's embedded host declaration file (rfd#27's 2026-09-16 amendment,
/// dsc#272 item 7), since that call has no ordinary scoped declaration to
/// show a signature from.
pub(crate) fn bridge_call_hover(program: &deka_syntax::Program, offset: usize) -> Option<String> {
    let (kind, action) = bridge_call_at(program, offset)?;
    let signature = deka_syntax::bridge::format_signature(&kind, &action)?;
    Some(format!(
        "{}\nhost bridge call — declared in deka's `deka-host.d.ds`",
        fenced(&signature)
    ))
}

/// Where dsc materializes its embedded host declaration file so an editor's
/// go-to-definition can open it as an ordinary file:// document (rfd#27's
/// 2026-09-16 amendment: "the language server shows it as a read-only
/// document"). Written once per content version and then chmod'd read-only,
/// so an edit attempt in the client fails at the filesystem level — no
/// virtual-document LSP extension needed, and no editor-specific support to
/// maintain. Returns `None` (silently — this only degrades go-to-definition)
/// if the temp directory cannot be written.
pub(crate) fn host_decl_document_path() -> Option<PathBuf> {
    let path = std::env::temp_dir().join("dsc").join("deka-host.d.ds");
    let up_to_date = fs::read(&path)
        .map(|existing| existing == deka_syntax::bridge::HOST_DECL_SOURCE.as_bytes())
        .unwrap_or(false);
    if up_to_date {
        return Some(path);
    }
    let parent = path.parent()?;
    fs::create_dir_all(parent).ok()?;
    // A previous run's read-only bit would otherwise block the rewrite
    // (e.g. a dsc upgrade changing the embedded catalog).
    if path.exists() {
        if let Ok(metadata) = fs::metadata(&path) {
            let mut perms = metadata.permissions();
            perms.set_readonly(false);
            let _ = fs::set_permissions(&path, perms);
        }
    }
    fs::write(&path, deka_syntax::bridge::HOST_DECL_SOURCE).ok()?;
    if let Ok(metadata) = fs::metadata(&path) {
        let mut perms = metadata.permissions();
        perms.set_readonly(true);
        let _ = fs::set_permissions(&path, perms);
    }
    Some(path)
}

/// Go-to-definition target for the `bridge kind.action(...)` call whose span
/// contains `offset`: a `Location` inside the materialized, read-only
/// `deka-host.d.ds`, pointed at that action's own declaration.
pub(crate) fn bridge_call_definition(
    program: &deka_syntax::Program,
    offset: usize,
) -> Option<Location> {
    let (kind, action) = bridge_call_at(program, offset)?;
    let signature = deka_syntax::bridge::find(&kind, &action)?;
    let path = host_decl_document_path()?;
    let uri = Url::from_file_path(&path).ok()?;
    let line_index = LineIndex::new(deka_syntax::bridge::HOST_DECL_SOURCE);
    let range = span_to_range(
        Span::new(signature.span.byte_start, signature.span.byte_end),
        &line_index,
    );
    Some(Location { uri, range })
}

fn fenced(detail: &str) -> String {
    format!("```dekascript\n{detail}\n```")
}

/// Hover for an imported name: resolve the module through the project loader,
/// parse the exporting module, and render the declaration of the imported
/// name from that module's own AST — so the signature shows parameter names,
/// not just types. `local` is the name in this module; the specifier carries
/// the name the exporting module declares.
fn imported_name_hover(
    documents: &HashMap<Url, String>,
    file_path: &str,
    program: &deka_syntax::Program,
    local: &str,
) -> Option<String> {
    let (imported, module_spec) = resolve_import_spec(program, local)?;
    let open_documents = open_document_paths(documents);
    let target = project_module_source(Path::new(file_path), module_spec, &open_documents)?;
    let arena = bumpalo::Bump::new();
    let target_program = deka_syntax::parse_recovering(&target, &arena).program?;
    // For an ordinary named import `imported` already is the declared name;
    // for `import X from "./m"` (rfd#12 ESM alignment amendment) `imported`
    // is the sentinel key `"default"`, which resolves to whatever name the
    // exporting module actually declared (`export default fn Page() { … }`
    // declares `Page`, not `default`).
    let declared = exported_declared_name(&target_program, imported)?;
    // Module-level items are collected ahead of (and deduped before) any
    // descended locals, so the module's own declaration wins by name.
    let declarations = deka_syntax::declarations_in_scope_at_offset(&target_program, target.len());
    let decl = declarations.iter().find(|decl| decl.name == declared)?;
    Some(format!(
        "{}\nimported from `{module_spec}`",
        fenced(&decl.detail)
    ))
}

/// The specifier under which `local` was imported (`import { imported as
/// local } from "module_spec"`): the exporting module's own name for it, and
/// the module specifier. Shared by hover and go-to-definition (`definition.rs`)
/// so both resolve the same import the same way.
pub(crate) fn resolve_import_spec<'a>(
    program: &'a deka_syntax::Program,
    local: &str,
) -> Option<(&'a str, &'a str)> {
    program.statements.iter().find_map(|stmt| {
        let Stmt::Import {
            specifiers, source, ..
        } = stmt
        else {
            return None;
        };
        let spec = specifiers.iter().find(|spec| spec.local == local)?;
        Some((spec.imported, *source))
    })
}

/// The declared name behind an export key: for most exports the key and the
/// declared name are the same, but a default export's key is always
/// `"default"` while the declaration keeps its own name (rfd#12 ESM
/// alignment amendment). Declaration files (rfd#39 2026-09-16 amendment)
/// export ambient `Opaque` names and `declare fn` signatures the same way.
/// Returns `None` when `exported` is not on the module's export surface at
/// all. `pub(crate)` so go-to-definition (`definition.rs`) shares this
/// instead of re-deriving it.
pub(crate) fn exported_declared_name<'a>(
    program: &'a deka_syntax::Program<'a>,
    exported: &str,
) -> Option<&'a str> {
    program.statements.iter().find_map(|stmt| {
        let Stmt::Export { decl, .. } = stmt else {
            return None;
        };
        match decl {
            ExportDecl::Const { name, .. } if *name == exported => Some(*name),
            ExportDecl::Function {
                name,
                is_default: true,
                ..
            } if exported == "default" => Some(*name),
            ExportDecl::Function {
                name,
                is_default: false,
                ..
            } if *name == exported => Some(*name),
            ExportDecl::NamedGroup { names, .. } => names
                .iter()
                .find(|n| n.alias.unwrap_or(n.name) == exported)
                .map(|n| n.name),
            // `.d.ds` declaration files (rfd#39 2026-09-16 amendment).
            ExportDecl::Opaque { name } if *name == exported => Some(*name),
            ExportDecl::Declare(function) if function.name == exported => Some(function.name),
            _ => None,
        }
    })
}
