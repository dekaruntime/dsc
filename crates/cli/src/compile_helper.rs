//! Shared compile-and-report helper for CLI commands.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use deka_compile::module_graph::{self, GraphCompileOptions};
use deka_compile::{compile_to_js_with_options, format_diagnostic, format_diagnostics};

pub use deka_compile::SourceModuleMeta as ModuleMeta;

pub struct CompileReport {
    pub js: String,
    pub warnings: Vec<String>,
}

pub fn compile_or_report(source: &str, input: &str) -> Result<CompileReport, String> {
    compile_or_report_with_options(source, input, deka_compile::CompileOptions::default())
}

pub fn compile_or_report_with_options(
    source: &str,
    input: &str,
    options: deka_compile::CompileOptions,
) -> Result<CompileReport, String> {
    match compile_to_js_with_options(source, input, options) {
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
///
/// `self_contained` inlines that module's prelude so the file can run in its
/// own scope (the isolate ESM loader). Default preserve emit leaves the
/// prelude detached for a later bundle.
pub fn compile_source_js(
    input: &Path,
    cwd: &Path,
    client: bool,
    self_contained: bool,
    dev: bool,
) -> Result<String, String> {
    let (entry, modules) = compile_graph_modules(input, cwd, client, self_contained, dev)?;
    modules
        .get(&entry)
        .cloned()
        .ok_or_else(|| "module graph did not emit entry module".to_string())
}

/// Root that build slot ids are relativized against (dsc#61). Project markers
/// in the source tree (`deka.json` or `deka.lock`) are the only root input.
/// When neither identifies a project, keep the compiler's historical
/// absolute-path identity by returning no root.
/// `compile_dev_plan` and `compile_graph_modules` share this so plan ids and
/// graph-emitted `deka:dev/<id>` imports agree.
pub fn slot_id_root(cwd: &Path, input: &Path) -> Option<PathBuf> {
    find_project_root(cwd, input)
}

/// Compile an entry and return only the build-time materialization contract.
/// Dsc exposes this to its host, but never evaluates a plan entry itself.
pub fn compile_dev_plan(input: &Path, cwd: &Path) -> Result<deka_compile::DevPlan, String> {
    let source = std::fs::read_to_string(input)
        .map_err(|err| format!("failed to read {}: {err}", input.display()))?;
    let input_name = input
        .to_str()
        .ok_or_else(|| format!("input path is not valid UTF-8: {}", input.display()))?;
    let module_root = slot_id_root(cwd, input);
    let meta = deka_compile::parse_source_module_meta(&source);
    if meta.imports.is_empty() {
        return compile_to_js_with_options(
            &source,
            input_name,
            deka_compile::CompileOptions {
                module_root: module_root.clone(),
                ..Default::default()
            },
        )
        .map(|result| result.dev_plan)
        .map_err(|diagnostics| format_diagnostics(&diagnostics));
    }

    let project_root = find_project_root(cwd, input)
        .or_else(|| input.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    if let Some(err) = unresolved_bare_import_error(&source, input, &project_root) {
        return Err(err);
    }
    let loader = module_graph::FsModuleLoader::new(project_root);
    module_graph::compile_module_graph_with_options(
        input,
        &loader,
        GraphCompileOptions {
            module_root,
            ..Default::default()
        },
    )
    .map(|graph| graph.dev_plan)
    .map_err(|diagnostics| format_diagnostics(&diagnostics))
}

/// Compile the entry's reachable graph. Keys are canonical source paths.
/// Isolate hosts (`--self-contained --out <dir>`) write every module, not
/// just the entry, so package imports (`http`, `jwt`) stay in the map.
pub fn compile_graph_modules(
    input: &Path,
    cwd: &Path,
    client: bool,
    self_contained: bool,
    dev: bool,
) -> Result<(PathBuf, HashMap<PathBuf, String>), String> {
    let source = std::fs::read_to_string(input)
        .map_err(|err| format!("failed to read {}: {err}", input.display()))?;
    let input_name = input
        .to_str()
        .ok_or_else(|| format!("input path is not valid UTF-8: {}", input.display()))?;
    let entry = std::fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf());
    let meta = deka_compile::parse_source_module_meta(&source);
    if meta.imports.is_empty() {
        let js = compile_to_js_with_options(
            &source,
            input_name,
            deka_compile::CompileOptions {
                module_root: slot_id_root(cwd, input),
                dev,
                ..Default::default()
            },
        )
        .map(|result| result.js)
        .map_err(|diagnostics| format_diagnostics(&diagnostics))?;
        return Ok((entry.clone(), HashMap::from([(entry, js)])));
    }

    let project_root = find_project_root(cwd, input)
        .or_else(|| input.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    if let Some(err) = unresolved_bare_import_error(&source, input, &project_root) {
        return Err(err);
    }
    let loader = module_graph::FsModuleLoader::new(project_root);
    let graph = module_graph::compile_module_graph_with_options(
        input,
        &loader,
        GraphCompileOptions {
            client,
            module_root: slot_id_root(cwd, input),
            dev,
            ..Default::default()
        },
    )
    .map_err(|diagnostics| format_diagnostics(&diagnostics))?;
    let modules = if self_contained {
        graph.self_contained_modules()
    } else {
        graph.modules
    };
    Ok((graph.entry, modules))
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

pub fn project_root_from_cwd(cwd: &Path) -> PathBuf {
    find_project_root(cwd, cwd).unwrap_or_else(|| cwd.to_path_buf())
}

pub fn print_cli_error(action: &str, err: &str) {
    if err.contains("Validation Error") {
        stdio::raw(err.trim_end_matches('\n'));
    } else {
        stdio::error(action, err);
    }
}

/// Apply the shared scanner and project gate. Standalone sources without
/// package imports need no lock; package imports require the full project gate.
pub fn unresolved_bare_import_error(
    source: &str,
    _input: &Path,
    project_root: &Path,
) -> Option<String> {
    let imports = deka_project::ds_imports::paths(source);
    let require_lockfile = imports.iter().any(|spec| {
        deka_project::module_spec::is_bare_module_specifier(spec)
            && !spec.starts_with("@/")
            && !deka_compile::is_math_module_spec(spec)
    });
    deka_project::project_gate::validate_project(
        project_root,
        &imports,
        &deka_project::project_gate::GateOptions {
            require_lockfile,
            context: "dsc",
            ..Default::default()
        },
    )
    .err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_gate_matches_shared_scanner() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("deka.json"), "{}").unwrap();
        std::fs::write(project.path().join("deka.lock"), r#"{"packages":{}}"#).unwrap();
        for (source, expected_paths) in [
            (
                r#"import { value } from "@vendor/pkg";"#,
                vec!["@vendor/pkg"],
            ),
            (r#"import "ui/button";"#, vec!["ui/button"]),
            (
                r#"export { value } from "@deka/anything";"#,
                vec!["@deka/anything"],
            ),
            (r#"export * from "third-party";"#, vec!["third-party"]),
            // The gate must still scan imports when compilation will fail to parse.
            (
                r#"import { value } from "@vendor/pkg"; const = ;"#,
                vec!["@vendor/pkg"],
            ),
            (
                r#"// import "fake"
                const text = 'import "also-fake"'; /* export * from "fake" */"#,
                vec![],
            ),
            (
                r#"import { PI } from "math"; import { x } from "./local.ds";"#,
                vec!["math", "./local.ds"],
            ),
        ] {
            let imports = deka_project::ds_imports::paths(source);
            assert_eq!(imports, expected_paths, "{source}");
            let expected = deka_project::project_gate::validate_project(
                project.path(),
                &imports,
                &deka_project::project_gate::GateOptions {
                    context: "dsc",
                    ..Default::default()
                },
            )
            .err();
            assert_eq!(
                unresolved_bare_import_error(
                    source,
                    &project.path().join("main.ds"),
                    project.path()
                ),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn standalone_sources_and_closed_math_need_no_lock() {
        let project = tempfile::tempdir().unwrap();
        for source in ["const value = 1;", r#"import { PI } from "math";"#] {
            assert!(
                unresolved_bare_import_error(
                    source,
                    &project.path().join("main.ds"),
                    project.path()
                )
                .is_none()
            );
        }
        assert!(
            unresolved_bare_import_error(
                r#"import { x } from "@vendor/pkg";"#,
                &project.path().join("main.ds"),
                project.path()
            )
            .unwrap()
            .contains("deka.lock")
        );
    }

    #[test]
    fn slot_id_root_keeps_no_root_without_markers() {
        // No deka.json/deka.lock exists under /no-such-project on the test
        // machine, so callers retain the compiler's absolute-path identity.
        let cwd = Path::new("/no-such-project");
        let input = Path::new("/no-such-project/app/page.ds");
        let root = slot_id_root(cwd, input);
        assert_eq!(root, None);
    }
}
