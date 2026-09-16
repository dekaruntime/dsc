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
