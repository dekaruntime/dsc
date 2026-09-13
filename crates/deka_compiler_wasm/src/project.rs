//! Project-mode WASM compiler ABI.
//!
//! A project is a virtual file system of DekaScript modules. The browser can
//! write source files into the project, compile the whole graph, and read back
//! per-module JavaScript output. The v2 compiler emits real ES modules, which
//! the browser loader can evaluate directly or wrap as needed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use deka_compile::module_graph::{compile_module_graph_with_options, GraphCompileOptions, ModuleLoader};
use deka_project::module_spec::ds_source_candidates;
use serde::Serialize;

use crate::{Diagnostic, WasmResult, box_result, internal_diagnostic, json};

/// A project holds a virtual file system and the results of the last compile.
pub struct ProjectState {
    files: HashMap<String, String>,
    compiled: HashMap<String, CompiledModule>,
    diagnostics: Vec<Diagnostic>,
    ok: bool,
    /// Base URL for bare stdlib import specifiers. When set, imports like
    /// `import { echo } from "io"` are left virtual and emitted as
    /// `import { echo } from "<module_base>/io.mjs"`, matching the
    /// single-file compiler's `moduleBase` option (deka#497).
    module_base: Option<String>,
}

struct CompiledModule {
    code: String,
}

#[derive(Serialize, Debug)]
struct ProjectCompileResponse {
    abi_version: u32,
    ok: bool,
    modules: HashMap<String, ModuleOutput>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Serialize, Debug, Clone)]
struct ModuleOutput {
    code: String,
}

#[derive(Serialize, Debug)]
struct ProjectReadResponse {
    abi_version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<ModuleOutput>,
    diagnostics: Vec<Diagnostic>,
}

impl ProjectState {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            compiled: HashMap::new(),
            diagnostics: Vec::new(),
            ok: false,
            module_base: None,
        }
    }

    pub fn write(&mut self, path: &str, source: &str) {
        self.files.insert(normalize_path(path), source.to_string());
    }

    pub fn set_module_base(&mut self, module_base: &str) {
        let trimmed = module_base.trim();
        self.module_base = (!trimmed.is_empty()).then(|| trimmed.to_string());
    }

    pub fn compile(&mut self) -> String {
        self.compiled.clear();
        self.diagnostics.clear();
        self.ok = true;

        // Each file in the project is treated as its own entry. The v2 module
        // graph discovers relative imports within the virtual file system.
        for path in self.files.keys().cloned().collect::<Vec<_>>() {
            match self.compile_module(&path) {
                Ok(code) => {
                    self.compiled.insert(path.clone(), CompiledModule { code });
                }
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    self.ok = false;
                }
            }
        }

        let modules: HashMap<String, ModuleOutput> = self
            .compiled
            .iter()
            .map(|(path, module)| {
                (
                    path.clone(),
                    ModuleOutput {
                        code: module.code.clone(),
                    },
                )
            })
            .collect();

        json(&ProjectCompileResponse {
            abi_version: crate::ABI_VERSION,
            ok: self.ok,
            modules,
            diagnostics: self.diagnostics.clone(),
        })
    }

    pub fn read(&self, path: &str) -> String {
        let normalized = normalize_path(path);
        if let Some(module) = self.compiled.get(&normalized) {
            return json(&ProjectReadResponse {
                abi_version: crate::ABI_VERSION,
                ok: true,
                output: Some(ModuleOutput {
                    code: module.code.clone(),
                }),
                diagnostics: Vec::new(),
            });
        }
        json(&ProjectReadResponse {
            abi_version: crate::ABI_VERSION,
            ok: false,
            output: None,
            diagnostics: vec![internal_diagnostic(
                path,
                "",
                format!("Module '{}' has not been compiled or does not exist in the project.", path),
            )],
        })
    }

    fn compile_module(&self, path: &str) -> Result<String, Diagnostic> {
        let source = self
            .files
            .get(path)
            .ok_or_else(|| internal_diagnostic(path, "", format!("File '{}' not found in project.", path)))?;

        let entry = PathBuf::from(path);
        let loader = ProjectModuleLoader {
            files: &self.files,
        };

        let options = GraphCompileOptions {
            jsx_runtime: None,
            client: false,
            module_base: self.module_base.clone(),
            // Virtual in-memory project: paths are already project-relative,
            // so no root relativization is needed.
            module_root: None,
        };
        match compile_module_graph_with_options(&entry, &loader, options) {
            Ok(result) => {
                // The graph contains the entry and all reachable modules. Return
                // the emitted JS for the requested entry file, self-contained:
                // the wasm host serves modules as separate files, so the entry
                // carries its own prelude rather than the shared program-level
                // one (deka#595).
                let entry_canon = std::fs::canonicalize(&entry).unwrap_or_else(|_| entry.clone());
                let modules = result.self_contained_modules();
                modules
                    .get(&entry_canon)
                    .cloned()
                    .or_else(|| modules.get(&entry).cloned())
                    .ok_or_else(|| {
                        internal_diagnostic(
                            path,
                            source,
                            "entry module was not emitted by the compiler graph".to_string(),
                        )
                    })
            }
            Err(diagnostics) => {
                let message = diagnostics
                    .iter()
                    .map(|d| d.message.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(internal_diagnostic(path, source, message))
            }
        }
    }
}

struct ProjectModuleLoader<'a> {
    files: &'a HashMap<String, String>,
}

impl<'a> ModuleLoader for ProjectModuleLoader<'a> {
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, String> {
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let resolved = resolve_relative_path(referrer, specifier)?;
            let candidates = relative_candidates(&resolved);
            for candidate in &candidates {
                if self.files.contains_key(candidate) {
                    return Ok(PathBuf::from(candidate));
                }
            }
            return Err(format!(
                "no module '{}' resolved to any of: {}",
                specifier,
                candidates.join(", ")
            ));
        }
        // Bare stdlib specifier (e.g. "io", "crypto"). If the project contains a
        // matching type stub, resolve to it so the compiler can typecheck the
        // import; otherwise the caller may leave it virtual for runtime serving
        // (deka#497).
        let bare = specifier.trim().strip_prefix("@deka/").unwrap_or(specifier.trim());
        let candidates = vec![
            format!("{}.ds", bare),
            format!("{}/index.ds", bare),
            format!("{}.dsx", bare),
            format!("{}/index.dsx", bare),
        ];
        for candidate in &candidates {
            if self.files.contains_key(candidate) {
                return Ok(PathBuf::from(candidate));
            }
        }
        Err(format!(
            "non-relative import '{}' is not supported in project mode; use './foo.ds'",
            specifier
        ))
    }

    fn load(&self, path: &Path) -> Result<String, String> {
        let key = path.to_string_lossy().replace('\\', "/");
        self.files
            .get(&key)
            .cloned()
            .ok_or_else(|| format!("missing virtual file '{}'", path.display()))
    }
}

fn normalize_path(path: &str) -> String {
    let normalized = Path::new(path)
        .to_string_lossy()
        .replace('\\', "/");
    normalized.strip_prefix("./").unwrap_or(&normalized).to_string()
}

fn resolve_relative_path(current_path: &Path, specifier: &str) -> Result<PathBuf, String> {
    let parent = current_path
        .parent()
        .ok_or_else(|| format!("'{}' has no parent directory", current_path.display()))?;
    let joined = parent.join(specifier);
    let normalized = normalize_path(&joined.to_string_lossy());
    Ok(PathBuf::from(normalized))
}

fn relative_candidates(resolved: &Path) -> Vec<String> {
    ds_source_candidates(resolved)
        .into_iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect()
}

/// Allocate a new project and return its opaque handle.
#[unsafe(no_mangle)]
pub extern "C" fn deka_compiler_project_new() -> u32 {
    let project = Box::new(ProjectState::new());
    Box::into_raw(project) as u32
}

/// Free a project previously allocated with `deka_compiler_project_new`.
///
/// # Safety
/// `project_id` must be a handle returned by `deka_compiler_project_new` that
/// has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deka_compiler_project_free(project_id: u32) {
    if project_id == 0 {
        return;
    }
    unsafe {
        let _ = Box::from_raw(project_id as *mut ProjectState);
    }
}

/// Write a source file into the project, replacing any existing file at `path`.
///
/// # Safety
/// Pointer/length pairs must point to valid, immutable UTF-8 buffers in WASM
/// memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deka_compiler_project_write(
    project_id: u32,
    path_ptr: *const u8,
    path_len: u32,
    source_ptr: *const u8,
    source_len: u32,
) {
    let project = match project_from_id(project_id) {
        Some(p) => p,
        None => return,
    };
    let path = crate::read_utf8(path_ptr, path_len, "path");
    let source = crate::read_utf8(source_ptr, source_len, "source");
    if let (Ok(path), Ok(source)) = (path, source) {
        project.write(path, source);
    }
}

/// Set the base URL used to rewrite bare stdlib import specifiers during
/// compile (e.g. `io` becomes `<base>/io.mjs`). Optional; when unset, project
/// mode only supports relative `./foo.ds` imports.
///
/// # Safety
/// `project_id` must be a valid project handle. Pointer/length pairs must
/// point to valid, immutable UTF-8 buffers in WASM memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deka_compiler_project_set_module_base(
    project_id: u32,
    base_ptr: *const u8,
    base_len: u32,
) {
    let project = match project_from_id(project_id) {
        Some(p) => p,
        None => return,
    };
    if let Ok(base) = crate::read_utf8(base_ptr, base_len, "module base") {
        project.set_module_base(&base);
    }
}

/// Compile every module in the project and return a JSON-encoded result.
///
/// # Safety
/// `project_id` must be a valid project handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deka_compiler_project_compile(project_id: u32) -> *mut WasmResult {
    let project = match project_from_id(project_id) {
        Some(p) => p,
        None => {
            return box_result(&json(&ProjectCompileResponse {
                abi_version: crate::ABI_VERSION,
                ok: false,
                modules: HashMap::new(),
                diagnostics: vec![internal_diagnostic(
                    "<project>",
                    "",
                    "invalid project handle".to_string(),
                )],
            }))
        }
    };
    let json = project.compile();
    box_result(&json)
}

/// Read the emitted JavaScript for one module after a successful compile.
///
/// # Safety
/// `project_id` must be a valid project handle. Pointer/length pairs must point
/// to valid, immutable UTF-8 buffers in WASM memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deka_compiler_project_read(
    project_id: u32,
    path_ptr: *const u8,
    path_len: u32,
) -> *mut WasmResult {
    let project = match project_from_id(project_id) {
        Some(p) => p,
        None => {
            return box_result(&json(&ProjectReadResponse {
                abi_version: crate::ABI_VERSION,
                ok: false,
                output: None,
                diagnostics: vec![internal_diagnostic(
                    "<project>",
                    "",
                    "invalid project handle".to_string(),
                )],
            }))
        }
    };
    let path = crate::read_utf8(path_ptr, path_len, "path");
    let json = match path {
        Ok(path) => project.read(path),
        Err(message) => json(&ProjectReadResponse {
            abi_version: crate::ABI_VERSION,
            ok: false,
            output: None,
            diagnostics: vec![internal_diagnostic("<project>", "", message.to_string())],
        }),
    };
    box_result(&json)
}

fn project_from_id(project_id: u32) -> Option<&'static mut ProjectState> {
    if project_id == 0 {
        return None;
    }
    unsafe { (project_id as *mut ProjectState).as_mut() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn project_compiles_relative_import_and_emits_es_modules() {
        let mut project = ProjectState::new();
        project.write(
            "math.ds",
            "export fn add(a: number, b: number) number {\n  return a + b;\n}\n",
        );
        project.write(
            "main.ds",
            "import { add } from \"./math.ds\";\nconst result: number = add(1, 2);\n",
        );

        let json = project.compile();
        let response: Value = serde_json::from_str(&json).expect("valid compile response JSON");

        assert_eq!(response["ok"], true, "compile failed: {}", json);
        assert!(response["modules"]["main.ds"]["code"].is_string(), "missing main.ds");
        assert!(response["modules"]["math.ds"]["code"].is_string(), "missing math.ds");

        let main = response["modules"]["main.ds"]["code"].as_str().unwrap();
        assert!(
            main.contains("import { add } from \"./math.ds\""),
            "expected ES import, got:\n{}",
            main
        );

        let math = response["modules"]["math.ds"]["code"].as_str().unwrap();
        assert!(
            math.contains("export function add"),
            "expected ES export, got:\n{}",
            math
        );
    }

    #[test]
    fn project_compiles_user_declared_type_parameters() {
        // rfd#56 phase 1 reverses deka#561: user code may declare type
        // parameters, unbounded form, at the wasm project boundary too. A
        // generic export must compile and instantiate at the importing
        // call site.
        let mut project = ProjectState::new();
        project.write(
            "lib.ds",
            "export fn first<T>(values: Array<T>) Option<T> { return values.has(0) ? Some(values[0]) : None }\n",
        );
        project.write(
            "main.ds",
            "import { first } from \"./lib.ds\"\nconst item: number = match (first([1])) { Some(value) => value, None => 0 }\n",
        );

        let json = project.compile();
        let response: Value = serde_json::from_str(&json).expect("valid compile response JSON");

        assert_eq!(response["ok"], true, "compile failed: {}", json);
        let main = response["modules"]["main.ds"]["code"].as_str().unwrap();
        assert!(main.contains("first([1])"), "got:\n{}", main);
    }

    #[test]
    fn project_preserves_builtin_container_return_types_across_modules() {
        // The original test here covered user-generic exports, which the
        // deka#561 ban removes. What it was really guarding — return-type
        // preservation across a module boundary — still matters for the
        // builtin containers, which remain legal.
        let mut project = ProjectState::new();
        project.write(
            "lib.ds",
            "export fn head(values: Array<string>) Option<string> { return values.has(0) ? Some(values[0]) : None }\n",
        );
        project.write(
            "main.ds",
            "import { head } from \"./lib.ds\"\nconst item: string = match (head([\"x\"])) { Some(value) => value, None => \"\" }\n",
        );

        let json = project.compile();
        let response: Value = serde_json::from_str(&json).expect("valid compile response JSON");

        assert_eq!(response["ok"], true, "compile failed: {}", json);
        let main = response["modules"]["main.ds"]["code"].as_str().unwrap();
        assert!(main.contains("head([\"x\"])"), "got:\n{}", main);
    }

    #[test]
    fn project_reports_unresolved_relative_import() {
        let mut project = ProjectState::new();
        project.write(
            "main.ds",
            "import { missing } from \"./nowhere.ds\";\nconsole.log(missing());\n",
        );

        let json = project.compile();
        let response: Value = serde_json::from_str(&json).expect("valid compile response JSON");

        assert_eq!(response["ok"], false, "expected compile failure");
        let messages: Vec<&str> = response["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["message"].as_str())
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("no module")),
            "expected unresolved import diagnostic, got: {:?}",
            messages
        );
    }

    #[test]
    fn relative_candidates_are_ds_only() {
        let paths = relative_candidates(Path::new("src/foo"));
        assert_eq!(
            paths,
            vec!["src/foo.ds", "src/foo.dsx", "src/foo/index.ds", "src/foo/index.dsx"]
        );
        assert!(relative_candidates(Path::new("src/foo.phpx")).is_empty());
        assert_eq!(
            relative_candidates(Path::new("src/foo.ds")),
            vec!["src/foo.ds"]
        );
    }

    #[test]
    fn project_with_module_base_resolves_documented_virtual_stdlib_imports() {
        let mut project = ProjectState::new();
        project.set_module_base("https://hats.dump.invalid/modules");
        project.write(
            "math.ds",
            "export fn add(a: number, b: number) number {\n  return a + b;\n}\n",
        );
        project.write(
            "main.ds",
            "import { echo } from \"io\";\nimport { add } from \"./math.ds\";\necho(add(1, 2));\n",
        );

        let json = project.compile();
        let response: Value = serde_json::from_str(&json).expect("valid compile response JSON");

        assert_eq!(response["ok"], true, "compile failed: {}", json);
        let main = response["modules"]["main.ds"]["code"].as_str().unwrap();
        assert!(
            main.contains("import { echo } from \"https://hats.dump.invalid/modules/io.mjs\""),
            "expected moduleBase rewrite, got:\n{}",
            main
        );
        assert!(
            main.contains("import { add } from \"./math.ds\""),
            "expected relative import preserved, got:\n{}",
            main
        );
    }

    #[test]
    fn project_without_module_base_still_rejects_stdlib_imports() {
        let mut project = ProjectState::new();
        project.write("main.ds", "import { echo } from \"io\";\necho(\"hi\");\n");

        let json = project.compile();
        let response: Value = serde_json::from_str(&json).expect("valid compile response JSON");

        assert_eq!(response["ok"], false, "expected compile failure");
        let messages: Vec<&str> = response["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["message"].as_str())
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("non-relative import 'io'")),
            "expected non-relative import diagnostic, got: {:?}",
            messages
        );
    }

    #[test]
    fn project_reports_missing_export_from_relative_import() {
        let mut project = ProjectState::new();
        project.set_module_base("https://hats.dump.invalid/modules");
        project.write(
            "math.ds",
            "export fn add(a: number, b: number) number {\n  return a + b;\n}\n",
        );
        project.write(
            "main.fail.ds",
            "import { echo } from \"io\";\nimport { subtract } from \"./math.ds\";\necho(subtract(1, 2));\n",
        );

        let json = project.compile();
        let response: Value = serde_json::from_str(&json).expect("valid compile response JSON");

        assert_eq!(response["ok"], false, "expected compile failure");
        let messages: Vec<&str> = response["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["message"].as_str())
            .collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("Missing export 'subtract' in './math.ds'")),
            "expected missing export diagnostic, got: {:?}",
            messages
        );
    }
}
