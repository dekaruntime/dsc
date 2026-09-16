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
use deka_syntax::ast::{ExportDecl, Stmt};

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
    let word = word_at_offset(text.as_bytes(), offset)?;
    let arena = bumpalo::Bump::new();
    let program = deka_syntax::parse_recovering(text, &arena).program;
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
    let (imported, module_spec) = program.statements.iter().find_map(|stmt| {
        let Stmt::Import {
            specifiers, source, ..
        } = stmt
        else {
            return None;
        };
        let spec = specifiers.iter().find(|spec| spec.local == local)?;
        Some((spec.imported, *source))
    })?;
    let open_documents = open_document_paths(documents);
    let target = project_module_source(Path::new(file_path), module_spec, &open_documents)?;
    let arena = bumpalo::Bump::new();
    let target_program = deka_syntax::parse_recovering(&target, &arena).program?;
    if !is_exported(&target_program, imported) {
        return None;
    }
    // Module-level items are collected ahead of (and deduped before) any
    // descended locals, so the module's own declaration wins by name.
    let declarations = deka_syntax::declarations_in_scope_at_offset(&target_program, target.len());
    let decl = declarations.iter().find(|decl| decl.name == imported)?;
    Some(format!(
        "{}\nimported from `{module_spec}`",
        fenced(&decl.detail)
    ))
}

/// True when `name` is on the module's export surface: an `export fn` /
/// `export const` declaration or an `export { … }` group (including
/// re-exports).
fn is_exported(program: &deka_syntax::Program, name: &str) -> bool {
    program.statements.iter().any(|stmt| {
        let Stmt::Export { decl, .. } = stmt else {
            return false;
        };
        match decl {
            ExportDecl::Const { name: exported, .. }
            | ExportDecl::Function {
                name: exported, ..
            } => *exported == name,
            ExportDecl::NamedGroup { names, .. } => names
                .iter()
                .any(|exported| exported.alias.unwrap_or(exported.name) == name),
        }
    })
}
