//! Go-to-definition: resolve the identifier at the cursor to its declaring
//! location.
//!
//! An imported name resolves through the same project loader hover uses
//! ([`resolve_import_spec`], [`project_module_location`]) — for a `.mjs`
//! import this is its sibling `.d.ds` (rfd#39 2026-09-16 amendment, dsc#274),
//! so a `Ctrl+click` on a name declared only in a declaration file lands in
//! that file, not a "no definition" dead end.

use super::*;

/// The definition location for the identifier at `offset` in `text`, or
/// `None` when the cursor is not on a resolvable imported name.
pub(crate) fn entry_definition(
    documents: &HashMap<Url, String>,
    text: &str,
    file_path: &str,
    offset: usize,
) -> Option<Location> {
    let word = word_at_offset(text.as_bytes(), offset)?;
    let arena = bumpalo::Bump::new();
    let program = deka_syntax::parse_recovering(text, &arena).program?;
    let (imported, module_spec) = resolve_import_spec(&program, &word)?;
    let open_documents = open_document_paths(documents);
    let (target_path, target_text) =
        project_module_location(Path::new(file_path), module_spec, &open_documents)?;
    let target_arena = bumpalo::Bump::new();
    let target_program = deka_syntax::parse_recovering(&target_text, &target_arena).program?;
    if !is_exported(&target_program, imported) {
        return None;
    }
    let declarations =
        deka_syntax::declarations_in_scope_at_offset(&target_program, target_text.len());
    let decl = declarations.iter().find(|decl| decl.name == imported)?;
    let target_uri = Url::from_file_path(&target_path).ok()?;
    let line_index = LineIndex::new(&target_text);
    Some(Location {
        uri: target_uri,
        range: span_to_range(Span::new(decl.span.byte_start, decl.span.byte_end), &line_index),
    })
}
