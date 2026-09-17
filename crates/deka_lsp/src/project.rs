//! Project-aware diagnostics: the same resolution + check path `dsc check
//! <file>` runs, with open editor buffers overlaid on the filesystem.
//!
//! Native-only: the graph check walks the project on disk, so this module is
//! gated behind the `native` feature and never reaches the wasm build.

use super::*;
use deka_compile::module_graph::{
    self, FsModuleLoader, GraphCompileOptions, ModuleLoader,
};
use std::collections::BTreeMap;

/// Module loader that serves open, unsaved editor buffers ahead of disk so
/// the graph checks what the user sees, not what was last saved.
struct OverlayLoader<'a> {
    inner: FsModuleLoader,
    documents: &'a HashMap<PathBuf, String>,
}

impl ModuleLoader for OverlayLoader<'_> {
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, String> {
        self.inner.resolve(specifier, referrer)
    }

    fn load(&self, path: &Path) -> Result<String, String> {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Some(text) = self.documents.get(&canonical) {
            return Ok(text.clone());
        }
        self.inner.load(path)
    }
}

/// Open documents keyed by canonical file path, for the overlay loader and
/// for range conversion against the exact text the graph checked.
pub(crate) fn open_document_paths(documents: &HashMap<Url, String>) -> HashMap<PathBuf, String> {
    documents
        .iter()
        .filter_map(|(uri, text)| {
            let path = uri.to_file_path().ok()?;
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            Some((canonical, text.clone()))
        })
        .collect()
}

/// Run the project-aware graph check for `entry` — the single function
/// `dsc check <file>` also runs — and group the resulting LSP diagnostics by
/// the file they belong to. Returns `None` when `entry` has no project root
/// (no `deka.json`/`deka.lock` marker), so callers fall back to single-file
/// analysis.
pub(crate) fn project_file_diagnostics(
    entry: &Path,
    open_documents: &HashMap<PathBuf, String>,
) -> Option<Vec<(PathBuf, Vec<Diagnostic>)>> {
    let project_root = module_graph::find_project_root(entry, entry)?;
    let loader = OverlayLoader {
        inner: FsModuleLoader::new(project_root),
        documents: open_documents,
    };
    let module_diagnostics = match module_graph::check_module_graph_with_options(
        entry,
        &loader,
        GraphCompileOptions::default(),
    ) {
        Ok(_) => return Some(Vec::new()),
        Err(diagnostics) => diagnostics,
    };

    let entry = std::fs::canonicalize(entry).unwrap_or_else(|_| entry.to_path_buf());
    let mut by_file: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
    let mut sources: HashMap<PathBuf, String> = HashMap::new();
    for module_diagnostic in module_diagnostics {
        // Graph-level errors (import cycles) name no module; surface them on
        // the entry the user is editing.
        let path = module_diagnostic.path.unwrap_or_else(|| entry.clone());
        let source = sources
            .entry(path.clone())
            .or_insert_with(|| module_source(&path, open_documents));
        if let Some(analysis) =
            analysis_from_compiler_diagnostic(source, &module_diagnostic.diagnostic)
        {
            by_file
                .entry(path)
                .or_default()
                .push(diagnostic_from_analysis(analysis));
        }
    }
    Some(by_file.into_iter().collect())
}

/// The exact text the graph checked for `path`: the open buffer when there
/// is one, otherwise disk.
fn module_source(path: &Path, open_documents: &HashMap<PathBuf, String>) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(text) = open_documents.get(&canonical) {
        return text.clone();
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The source of `module_spec` as imported from `entry`, resolved and loaded
/// through the same overlay loader the project graph check uses (unsaved
/// buffers win over disk). Returns `None` outside a project or when the
/// specifier does not resolve. Hover uses this to read the exporting module's
/// own declarations.
pub(crate) fn project_module_source(
    entry: &Path,
    module_spec: &str,
    open_documents: &HashMap<PathBuf, String>,
) -> Option<String> {
    project_module_location(entry, module_spec, open_documents).map(|(_, source)| source)
}

/// Like [`project_module_source`], but also returns the resolved file path —
/// for a `.mjs`/`.js` specifier this is its sibling `.d.ds` (rfd#39
/// 2026-09-16 amendment, dsc#274), since `FsModuleLoader::resolve` is the one
/// place that resolution happens; every project-aware caller (hover,
/// go-to-definition, completion) shares it rather than re-deriving it.
/// Go-to-definition needs the path to build the target `Location`'s URI;
/// hover only needs the text.
pub(crate) fn project_module_location(
    entry: &Path,
    module_spec: &str,
    open_documents: &HashMap<PathBuf, String>,
) -> Option<(PathBuf, String)> {
    let project_root = module_graph::find_project_root(entry, entry)?;
    let loader = OverlayLoader {
        inner: FsModuleLoader::new(project_root),
        documents: open_documents,
    };
    let canonical_entry = std::fs::canonicalize(entry).unwrap_or_else(|_| entry.to_path_buf());
    let path = loader.resolve(module_spec, &canonical_entry).ok()?;
    let source = loader.load(&path).ok()?;
    Some((path, source))
}

/// The importable names of `module_spec` as imported from `entry`, resolved
/// and parsed through the same loader + parser the project graph check uses:
/// the overlay serves unsaved buffers, `FsModuleLoader` owns resolution, and
/// `collect_module_exports` owns the export surface (`export fn`, not just the
/// legacy `export function` spelling). Returns `None` outside a project or
/// when the specifier does not resolve to a parseable module.
pub(crate) fn project_module_exports(
    entry: &Path,
    module_spec: &str,
    open_documents: &HashMap<PathBuf, String>,
) -> Option<Vec<ExportInfo>> {
    let project_root = module_graph::find_project_root(entry, entry)?;
    let loader = OverlayLoader {
        inner: FsModuleLoader::new(project_root),
        documents: open_documents,
    };
    let canonical_entry = std::fs::canonicalize(entry).unwrap_or_else(|_| entry.to_path_buf());
    let path = loader.resolve(module_spec, &canonical_entry).ok()?;
    let source = loader.load(&path).ok()?;
    let arena = bumpalo::Bump::new();
    let program = deka_syntax::parse_recovering(&source, &arena).program?;
    let exports = deka_syntax::collect_module_exports(&program, &arena);
    Some(export_infos_from_module_exports(&exports))
}

/// Map the graph's `ModuleExports` to completion-facing export info: the
/// importable surface is `values`, the type tables, and pass-through
/// re-exports — never the compiler-private metadata tables.
fn export_infos_from_module_exports(exports: &deka_syntax::ModuleExports) -> Vec<ExportInfo> {
    let mut infos: Vec<ExportInfo> = Vec::new();
    let mut push = |name: &str, kind: CompletionItemKind| {
        if infos.iter().any(|info| info.name == name) {
            return;
        }
        infos.push(ExportInfo {
            name: name.to_string(),
            kind: Some(kind),
        });
    };
    for (name, ty) in exports.values.iter() {
        let kind = if matches!(ty, deka_syntax::typeck::Type::Function { .. }) {
            CompletionItemKind::FUNCTION
        } else {
            CompletionItemKind::CONSTANT
        };
        push(name, kind);
    }
    for name in exports.structs.keys() {
        push(name, CompletionItemKind::STRUCT);
    }
    for name in exports.enums.keys() {
        push(name, CompletionItemKind::ENUM);
    }
    for name in exports.interfaces.keys() {
        push(name, CompletionItemKind::INTERFACE);
    }
    for name in exports
        .aliases
        .keys()
        .chain(exports.newtypes.keys())
        .chain(exports.opaques.keys())
    {
        push(name, CompletionItemKind::TYPE_PARAMETER);
    }
    for name in exports.re_exports.iter() {
        push(name, CompletionItemKind::VARIABLE);
    }
    infos.sort_by(|a, b| a.name.cmp(&b.name));
    infos
}
