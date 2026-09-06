//! Shared compile-and-report helper for CLI commands.

use std::path::{Path, PathBuf};

use deka_compile::module_graph::{self, GraphCompileOptions};
use deka_compile::{compile_to_js, format_diagnostic, format_diagnostics};

pub use deka_compile::SourceModuleMeta as ModuleMeta;

pub struct CompileReport {
    pub js: String,
    pub warnings: Vec<String>,
}

pub fn compile_or_report(source: &str, input: &str) -> Result<CompileReport, String> {
    match compile_to_js(source, input) {
        Ok(result) => {
            let warnings = result.diagnostics.iter().map(format_diagnostic).collect();
            Ok(CompileReport {
                js: result.js,
                warnings,
            })
        }
        Err(diagnostics) => Err(diagnostics
            .iter()
            .map(format_diagnostic)
            .collect::<Vec<_>>()
            .join("\n")),
    }
}

pub fn compile_js_or_report(source: &str, input: &str) -> Result<String, String> {
    compile_or_report(source, input).map(|report| report.js)
}

pub fn find_project_root(cwd: &Path, input: &Path) -> Option<PathBuf> {
    let absolute_input = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let start = if absolute_input.is_dir() {
        absolute_input
    } else {
        absolute_input.parent()?.to_path_buf()
    };

    for dir in start.ancestors() {
        if dir.join("deka.json").is_file() || dir.join("deka.lock").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

pub fn is_deka_source_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("ds" | "dsx")
    )
}

/// Emit one module. Files with imports go through the graph so relative and
/// linked packages typecheck; isolated files use the single-file pipeline.
pub fn compile_source_js(input: &Path, cwd: &Path, client: bool) -> Result<String, String> {
    let source = std::fs::read_to_string(input)
        .map_err(|err| format!("failed to read {}: {err}", input.display()))?;
    let input_name = input
        .to_str()
        .ok_or_else(|| format!("input path is not valid UTF-8: {}", input.display()))?;
    let meta = deka_compile::parse_source_module_meta(&source);
    if meta.imports.is_empty() {
        return compile_js_or_report(&source, input_name);
    }

    let project_root = find_project_root(cwd, input)
        .or_else(|| input.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let loader = module_graph::FsModuleLoader::new(project_root);
    let graph = module_graph::compile_module_graph_with_options(
        input,
        &loader,
        GraphCompileOptions {
            client,
            ..Default::default()
        },
    )
    .map_err(|diagnostics| format_diagnostics(&diagnostics))?;
    graph
        .modules
        .get(&graph.entry)
        .cloned()
        .ok_or_else(|| "module graph did not emit entry module".to_string())
}

/// Preserve-mode emit keeps source specifiers; only relative `.ds` / `.dsx`
/// targets become their `.js` peers on disk.
pub fn rewrite_relative_ds_imports(mut js: String) -> String {
    for quote in ['\'', '"'] {
        let needle = format!("from {quote}");
        let mut cursor = 0;
        while let Some(found) = js[cursor..].find(&needle) {
            let start = cursor + found + needle.len();
            let Some(end) = js[start..].find(quote) else {
                break;
            };
            let end = start + end;
            let specifier = &js[start..end];
            if (specifier.starts_with("./") || specifier.starts_with("../"))
                && (specifier.ends_with(".ds") || specifier.ends_with(".dsx"))
            {
                let ext_len = if specifier.ends_with(".dsx") { 4 } else { 3 };
                js.replace_range(end - ext_len..end, ".js");
                cursor = end - 1;
            } else {
                cursor = end + 1;
            }
        }
    }
    js
}
