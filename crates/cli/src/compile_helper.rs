//! Shared compile-and-report helper for CLI commands.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use deka_compile::module_graph::{self, GraphCompileOptions, ModuleLoader};
use deka_compile::{
    compile_to_js, compile_to_js_with_options, format_diagnostic, format_diagnostics,
};
use sha2::{Digest, Sha256};

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
///
/// `self_contained` inlines that module's prelude so the file can run in its
/// own scope (the isolate ESM loader). Default preserve emit leaves the
/// prelude detached for a later bundle.
pub fn compile_source_js(
    input: &Path,
    cwd: &Path,
    client: bool,
    self_contained: bool,
) -> Result<String, String> {
    let (entry, modules) = compile_graph_modules(input, cwd, client, self_contained)?;
    modules
        .get(&entry)
        .cloned()
        .ok_or_else(|| "module graph did not emit entry module".to_string())
}

/// Root that build slot ids are relativized against (dsc#61). The deka host
/// runs dsc with cwd = project root and `DEKA_MODULE_ROOT` set; otherwise use
/// `deka.json`/`deka.lock` detection. When neither identifies a project, keep
/// the compiler's historical absolute-path identity by returning no root.
/// `compile_dev_plan` and `compile_graph_modules` share this so plan ids and
/// graph-emitted `deka:dev/<id>` imports agree.
pub fn slot_id_root(cwd: &Path, input: &Path) -> Option<PathBuf> {
    slot_id_root_from(
        std::env::var_os("DEKA_MODULE_ROOT").map(PathBuf::from),
        cwd,
        input,
    )
}

fn slot_id_root_from(env_root: Option<PathBuf>, cwd: &Path, input: &Path) -> Option<PathBuf> {
    env_root
        .filter(|root| !root.as_os_str().is_empty())
        .or_else(|| find_project_root(cwd, input))
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

/// Bare package imports must already live in `ds_modules/` and `deka.lock`.
/// dsc never fetches or writes the lock; missing entries, missing install
/// dirs, and integrity mismatches are hard failures.
pub fn unresolved_bare_import_error(
    source: &str,
    input: &Path,
    project_root: &Path,
) -> Option<String> {
    let arena = bumpalo::Bump::new();
    let parsed = deka_syntax::parse(source, &arena);
    let program = parsed.program?;
    let loader = module_graph::FsModuleLoader::new(project_root.to_path_buf());
    let lock = read_lockfile(project_root);
    let linked = deka_project::modules::read_linked_modules(project_root).unwrap_or_default();
    for stmt in program.statements.iter() {
        let deka_syntax::Stmt::Import {
            specifiers,
            source: spec,
            span,
        } = stmt
        else {
            continue;
        };
        if skip_bare_package_check(spec, specifiers.is_empty()) {
            continue;
        }
        if is_linked_bare_import(spec, &linked) {
            continue;
        }
        let package_name = lock_package_name(spec).unwrap_or_else(|| spec.to_string());
        let aliases = deka_project::module_spec::module_spec_aliases(&package_name);
        if let Some(err) = package_lock_error(
            source,
            input,
            project_root,
            spec,
            &aliases,
            lock.as_ref(),
            *span,
        ) {
            return Some(err);
        }
        if loader.resolve(spec, input).is_ok() {
            continue;
        }
        return Some(format_package_error(
            source,
            input,
            project_root,
            spec,
            *span,
            "Unresolved Import",
            &format!("module '{spec}' is not installed in ds_modules/"),
            &format!(
                "run `deka install` or `deka add {spec}`. dsc does not fetch packages or write deka.lock."
            ),
        ));
    }
    None
}

/// Lock / install identity for a bare import (`json` / `json/x` → `@deka/json`).
fn lock_package_name(spec: &str) -> Option<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('@') {
        let mut parts = trimmed.split('/');
        let scope = parts.next()?;
        let name = parts.next()?;
        if name.is_empty() {
            return None;
        }
        return Some(format!("{scope}/{name}"));
    }
    let first = trimmed.split('/').next()?;
    if first.is_empty() {
        return None;
    }
    Some(format!("@deka/{first}"))
}

fn is_linked_bare_import(spec: &str, linked: &std::collections::BTreeMap<String, PathBuf>) -> bool {
    for package in linked.keys() {
        for alias in deka_project::module_spec::module_spec_aliases(package) {
            if spec == alias || spec.starts_with(&(alias + "/")) {
                return true;
            }
        }
    }
    false
}

fn package_lock_error(
    source: &str,
    input: &Path,
    project_root: &Path,
    spec: &str,
    aliases: &[String],
    lock: Option<&serde_json::Value>,
    span: deka_syntax::Span,
) -> Option<String> {
    let modules_dirs = deka_project::modules::existing_modules_dirs(project_root);
    let installed = installed_package_dir(&modules_dirs, aliases);
    let lock_entry = lock.and_then(|lock| lock_entry_for(lock, aliases));

    if modules_dirs.is_empty() {
        return Some(format_package_error(
            source,
            input,
            project_root,
            spec,
            span,
            "Unresolved Import",
            &format!("module '{spec}' is not installed in ds_modules/"),
            &format!(
                "run `deka install` or `deka add {spec}`. dsc does not fetch packages or write deka.lock."
            ),
        ));
    }

    if lock_entry.is_none() {
        return Some(format_package_error(
            source,
            input,
            project_root,
            spec,
            span,
            "Unresolved Import",
            &format!("module '{spec}' has no deka.lock entry"),
            &format!(
                "run `deka install` or `deka add {spec}`. dsc does not fetch packages or write deka.lock."
            ),
        ));
    }

    let Some(package_dir) = installed else {
        return Some(format_package_error(
            source,
            input,
            project_root,
            spec,
            span,
            "Unresolved Import",
            &format!("module '{spec}' is not installed in ds_modules/"),
            &format!(
                "run `deka install` or `deka add {spec}`. dsc does not fetch packages or write deka.lock."
            ),
        ));
    };

    let expected = lock_entry.and_then(lock_fs_graph_hash)?;
    match compute_fs_graph_hash(&package_dir) {
        Ok(actual) if actual == expected => None,
        Ok(_) | Err(_) => Some(format_package_error(
            source,
            input,
            project_root,
            spec,
            span,
            "Integrity Mismatch",
            &format!("module '{spec}' in ds_modules/ does not match deka.lock"),
            &format!(
                "run `deka install` or `deka add {spec}`. dsc does not fetch packages or write deka.lock."
            ),
        )),
    }
}

fn read_lockfile(project_root: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(project_root.join("deka.lock")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(value)
}

fn lock_packages(lock: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
    lock.get("packages")
        .and_then(|packages| packages.as_object())
        .or_else(|| {
            lock.get("php")
                .and_then(|php| php.get("packages"))
                .and_then(|packages| packages.as_object())
        })
}

fn lock_entry_for<'a>(
    lock: &'a serde_json::Value,
    aliases: &[String],
) -> Option<&'a serde_json::Value> {
    let packages = lock_packages(lock)?;
    aliases.iter().find_map(|alias| packages.get(alias))
}

fn lock_fs_graph_hash(entry: &serde_json::Value) -> Option<String> {
    let metadata = entry.get(2)?;
    let hash = metadata
        .get("fsGraph")
        .or_else(|| metadata.get("fs_graph"))
        .and_then(|graph| graph.get("hash"))
        .and_then(|hash| hash.as_str())
        .map(str::trim)
        .filter(|hash| !hash.is_empty() && hash.chars().all(|ch| ch.is_ascii_hexdigit()))?;
    Some(hash.to_string())
}

fn installed_package_dir(modules_dirs: &[PathBuf], aliases: &[String]) -> Option<PathBuf> {
    for dir in modules_dirs {
        for alias in aliases {
            let candidate = dir.join(alias);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    None
}

fn compute_fs_graph_hash(root: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect_integrity_files(root, root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for path in files {
        let rel = path
            .strip_prefix(root)
            .map_err(|_| "failed to normalize integrity path".to_string())?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        hasher.update(rel_str.as_bytes());
        hasher.update(b"\0");
        let mut file = std::fs::File::open(&path)
            .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
        let mut buf = [0u8; 8192];
        loop {
            let read = file
                .read(&mut buf)
                .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
        }
        hasher.update(b"\n");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_integrity_files(
    root: &Path,
    current: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut entries = std::fs::read_dir(current)
        .map_err(|err| format!("failed to read {}: {err}", current.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("failed to read {}: {err}", current.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name.as_ref(),
                "ds_modules"
                    | "php_modules"
                    | ".git"
                    | "target"
                    | "node_modules"
                    | "dist"
                    | ".deka"
                    | ".cache"
            ) {
                continue;
            }
            collect_integrity_files(root, &path, out)?;
        } else if path.is_file() {
            if name == ".DS_Store" || name.starts_with("._") {
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

fn skip_bare_package_check(spec: &str, specs_empty: bool) -> bool {
    let trimmed = spec.trim();
    if trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with('/')
        || trimmed.starts_with("@/")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("file://")
    {
        return true;
    }
    if specs_empty && trimmed.to_ascii_lowercase().ends_with(".css") {
        return true;
    }
    let bare = trimmed.strip_prefix("@deka/").unwrap_or(trimmed);
    bare == "ui"
        || bare.starts_with("ui/")
        || deka_compile::shake::normalize_ui_specifier(trimmed).is_some()
}

fn format_package_error(
    source: &str,
    input: &Path,
    project_root: &Path,
    spec: &str,
    span: deka_syntax::Span,
    kind: &str,
    message: &str,
    help: &str,
) -> String {
    let file_path = input
        .strip_prefix(project_root)
        .unwrap_or(input)
        .display()
        .to_string();
    let (line, column, underline) = specifier_frame(source, spec, span);
    deka_validation::format_validation_error(
        source, &file_path, kind, line, column, message, help, underline,
    )
}

fn specifier_frame(source: &str, spec: &str, span: deka_syntax::Span) -> (usize, usize, usize) {
    let start = span.byte_start.min(source.len());
    let end = span.byte_end.min(source.len()).max(start);
    let snippet = &source[start..end];
    let double = format!("\"{spec}\"");
    let single = format!("'{spec}'");
    let (rel, quote) = if let Some(offset) = snippet.find(&double) {
        (offset, 1usize)
    } else if let Some(offset) = snippet.find(&single) {
        (offset, 1usize)
    } else {
        (0, 0)
    };
    let (line, column) = byte_to_line_col(source, start + rel + quote);
    (line, column, spec.chars().count().max(1))
}

fn byte_to_line_col(source: &str, byte: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut column = 1usize;
    for (index, ch) in source.char_indices() {
        if index >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_id_root_prefers_env_over_markers() {
        let cwd = Path::new("/repo");
        let input = Path::new("/repo/app/page.ds");
        let root = slot_id_root_from(Some(PathBuf::from("/env/root")), cwd, input);
        assert_eq!(root, Some(PathBuf::from("/env/root")));
    }

    #[test]
    fn slot_id_root_ignores_empty_env() {
        let cwd = Path::new("/repo");
        let input = Path::new("/repo/app/page.ds");
        let root = slot_id_root_from(Some(PathBuf::new()), cwd, input);
        assert_eq!(root, None);
    }

    #[test]
    fn slot_id_root_keeps_no_root_without_markers() {
        // No deka.json/deka.lock exists under /no-such-project on the test
        // machine, so callers retain the compiler's absolute-path identity.
        let cwd = Path::new("/no-such-project");
        let input = Path::new("/no-such-project/app/page.ds");
        let root = slot_id_root_from(None, cwd, input);
        assert_eq!(root, None);
    }
}
