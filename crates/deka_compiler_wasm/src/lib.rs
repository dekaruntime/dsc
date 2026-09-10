//! Browser compiler ABI for Deka source languages.
//!
//! The neutral `deka_compiler_*` exports are the only browser-facing ABI.

use std::alloc::{Layout, alloc, dealloc};
use std::{ptr, slice, str};

use deka_syntax::{Diagnostic as DekaDiagnostic, Severity};
use serde::{Deserialize, Serialize};

/// Alignment used for all WASM-side allocations.  Must be large enough for
/// `WasmResult` (two `u32`s, align 4) as well as arbitrary byte buffers.
const ALLOC_ALIGN: usize = 8;
/// Version of the allocation and JSON response ABI.
///
/// 1: initial ABI with `mode` passed as a plain string ("auto" / "deka").
/// 2: `mode` replaced by a JSON options blob `{ "mode": "deka", "moduleBase": "..." }`.
pub const ABI_VERSION: u32 = 2;
const COMPILER_NAME: &str = "deka";
const SOURCE_COMMIT: &str = match option_env!("DEKA_SOURCE_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

mod project;

/// Result descriptor returned by `deka_compiler_compile`. The browser shim
/// reads UTF-8 JSON from `ptr`/`len`, then frees the whole allocation with
/// `deka_compiler_free(result_ptr, size_of::<WasmResult>() + result.len)`.
#[repr(C)]
pub struct WasmResult {
    pub ptr: u32,
    pub len: u32,
}

/// Allocate a buffer of `size` bytes in WASM memory.
#[unsafe(no_mangle)]
pub extern "C" fn deka_compiler_alloc(size: u32) -> *mut u8 {
    let layout = Layout::from_size_align(size as usize, ALLOC_ALIGN).expect("invalid alloc size");
    // SAFETY: layout has non-zero size.
    unsafe { alloc(layout) }
}

/// Free a buffer previously returned by `deka_compiler_alloc`.
///
/// # Safety
///
/// `ptr` must be null or point to a buffer returned by `deka_compiler_alloc`
/// with exactly the supplied `size`; passing any other allocation is undefined.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deka_compiler_free(ptr: *mut u8, size: u32) {
    if ptr.is_null() {
        return;
    }
    let layout = Layout::from_size_align(size as usize, ALLOC_ALIGN).expect("invalid free size");
    // SAFETY: ptr must have been allocated by deka_compiler_alloc with the same size/alignment.
    unsafe { dealloc(ptr, layout) }
}

/// Compile DekaScript source using a JSON options blob and return a
/// JSON-encoded `WasmResult`.
///
/// The options blob has the form `{ "mode": "deka", "moduleBase": "..." }`.
/// `mode` is required; `moduleBase` is optional. When `moduleBase` is supplied,
/// bare import specifiers are rewritten to `<moduleBase>/<spec>.mjs`.
///
/// # Safety
/// Non-empty pointer/length pairs must point to valid, immutable UTF-8 buffers
/// in WASM memory. Invalid request text produces a structured diagnostic.
#[unsafe(no_mangle)]
pub extern "C" fn deka_compiler_compile(
    source_ptr: *const u8,
    source_len: u32,
    filename_ptr: *const u8,
    filename_len: u32,
    options_ptr: *const u8,
    options_len: u32,
) -> *mut WasmResult {
    let source = read_utf8(source_ptr, source_len, "source");
    let filename = read_utf8(filename_ptr, filename_len, "filename");
    let options = read_utf8(options_ptr, options_len, "options");
    let json = match (source, filename, options) {
        (Ok(source), Ok(filename), Ok(options)) => compile_request(source, filename, options),
        (source, filename, options) => request_error(
            filename.unwrap_or("<unknown>"),
            source
                .err()
                .or(filename.err())
                .or(options.err())
                .unwrap_or("invalid request"),
        ),
    };
    box_result(&json)
}

/// Return static compiler metadata without compiling a source file.
#[unsafe(no_mangle)]
pub extern "C" fn deka_compiler_metadata() -> *mut WasmResult {
    box_result(&json(&CompilerMetadata::current()))
}

/// Format a JavaScript source string and return a JSON-encoded `WasmResult`.
///
/// # Safety
/// Non-empty pointer/length pairs must point to valid, immutable UTF-8 buffers
/// in WASM memory.
#[unsafe(no_mangle)]
pub extern "C" fn deka_compiler_format_js(
    source_ptr: *const u8,
    source_len: u32,
) -> *mut WasmResult {
    box_result(&json(&format_request(
        read_utf8(source_ptr, source_len, "source"),
        deka_fmt::format_js,
    )))
}

/// Format a DekaScript source string and return a JSON-encoded `WasmResult`.
///
/// # Safety
/// Non-empty pointer/length pairs must point to valid, immutable UTF-8 buffers
/// in WASM memory.
#[unsafe(no_mangle)]
pub extern "C" fn deka_compiler_format_ds(
    source_ptr: *const u8,
    source_len: u32,
) -> *mut WasmResult {
    box_result(&json(&format_request(
        read_utf8(source_ptr, source_len, "source"),
        deka_fmt::format_ds,
    )))
}

fn format_request(
    source: Result<&str, &str>,
    formatter: fn(&str) -> Result<String, String>,
) -> FormatResponse {
    match source {
        Ok(source) => match formatter(source) {
            Ok(code) => FormatResponse {
                abi_version: ABI_VERSION,
                ok: true,
                output: Some(FormatOutput { code }),
                diagnostics: Vec::new(),
            },
            Err(message) => FormatResponse {
                abi_version: ABI_VERSION,
                ok: false,
                output: None,
                diagnostics: vec![internal_diagnostic("<format>", source, message)],
            },
        },
        Err(message) => FormatResponse {
            abi_version: ABI_VERSION,
            ok: false,
            output: None,
            diagnostics: vec![internal_diagnostic("<format>", "", message.to_string())],
        },
    }
}

#[derive(Serialize)]
struct FormatOutput {
    code: String,
}

#[derive(Serialize)]
struct FormatResponse {
    abi_version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<FormatOutput>,
    diagnostics: Vec<Diagnostic>,
}

pub(crate) fn read_utf8<'a>(
    ptr: *const u8,
    len: u32,
    label: &'static str,
) -> Result<&'a str, &'static str> {
    if len == 0 {
        return Ok("");
    }
    if ptr.is_null() {
        return Err(match label {
            "source" => "source pointer is null",
            "filename" => "filename pointer is null",
            _ => "options pointer is null",
        });
    }
    // SAFETY: non-empty buffers have been checked for a non-null pointer; the
    // ABI contract requires the caller to provide a valid readable range.
    let bytes = unsafe { slice::from_raw_parts(ptr, len as usize) };
    str::from_utf8(bytes).map_err(|_| match label {
        "source" => "source is not valid UTF-8",
        "filename" => "filename is not valid UTF-8",
        _ => "options is not valid UTF-8",
    })
}

#[derive(Serialize)]
struct CompileResponse<'a> {
    abi_version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<CompileOutput>,
    diagnostics: Vec<Diagnostic>,
    metadata: CompileMetadata<'a>,
}

#[derive(Serialize)]
struct CompileOutput {
    code: String,
    /// Present only when source contains build-only values. Browser hosts can
    /// inspect the requirement but cannot execute it without a Deka host.
    #[serde(rename = "devPlan", skip_serializing_if = "Option::is_none")]
    dev_plan: Option<deka_compile::DevPlan>,
}

#[derive(Serialize)]
struct CompileMetadata<'a> {
    filename: &'a str,
    language: &'a str,
    compiler: CompilerMetadata,
}

#[derive(Serialize)]
struct CompilerMetadata {
    name: &'static str,
    version: &'static str,
    source_commit: &'static str,
}

impl CompilerMetadata {
    const fn current() -> Self {
        Self {
            name: COMPILER_NAME,
            version: env!("CARGO_PKG_VERSION"),
            source_commit: SOURCE_COMMIT,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Diagnostic {
    severity: &'static str,
    code: String,
    message: String,
    filename: String,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    help: Option<String>,
    /// Pre-formatted error string produced by the same `deka_validation` crate
    /// used by the native runtime, so the browser can display diagnostics
    /// without re-implementing the formatter.
    rendered: String,
}

#[derive(Debug, Deserialize)]
struct CompileRequestOptions {
    mode: String,
    #[serde(rename = "moduleBase")]
    module_base: Option<String>,
}

fn compile_request(source: &str, filename: &str, options_json: &str) -> String {
    let options = match parse_compile_options(filename, options_json) {
        Ok(options) => options,
        Err(message) => return request_error(filename, message),
    };

    let compile_options = deka_compile::CompileOptions {
        module_base: options.module_base,
        module_root: None,
        used_exports: None,
        client: false,
        // Single-file playground compilation stays self-contained; only the
        // module graph detaches the prelude (deka#595).
        detached_prelude: false,
        // Build closures are emitted by the module graph; single-file
        // playground compilation has no cross-module builds.
        build_closure_names: std::collections::HashSet::new(),
    };

    match deka_compile::compile_to_js_with_options(source, filename, compile_options) {
        Ok(result) => {
            let diagnostics = result
                .diagnostics
                .iter()
                .map(|d| diagnostic_from_deka_syntax(d, source, filename))
                .collect::<Vec<_>>();
            let has_error = diagnostics.iter().any(|d| d.severity == "error");
            let ok = !has_error && !result.js.is_empty();
            let output = ok.then(|| CompileOutput {
                code: result.js,
                dev_plan: (!result.dev_plan.slots.is_empty()).then_some(result.dev_plan),
            });
            json(&CompileResponse {
                abi_version: ABI_VERSION,
                ok,
                output,
                diagnostics,
                metadata: CompileMetadata {
                    filename,
                    language: &options.mode,
                    compiler: CompilerMetadata::current(),
                },
            })
        }
        Err(diagnostics) => {
            let diagnostics = diagnostics
                .iter()
                .map(|d| diagnostic_from_deka_syntax(d, source, filename))
                .collect::<Vec<_>>();
            json(&CompileResponse {
                abi_version: ABI_VERSION,
                ok: false,
                output: None,
                diagnostics,
                metadata: CompileMetadata {
                    filename,
                    language: &options.mode,
                    compiler: CompilerMetadata::current(),
                },
            })
        }
    }
}

fn parse_compile_options(
    filename: &str,
    options_json: &str,
) -> Result<CompileRequestOptions, &'static str> {
    if !filename.ends_with(".ds") && !filename.ends_with(".dsx") {
        return Err("Deka browser compiler only accepts .ds or .dsx source files");
    }
    let mut options: CompileRequestOptions = serde_json::from_str(options_json)
        .map_err(|_| "invalid compile options JSON; expected `{ \"mode\": \"deka\" }`")?;
    if options.mode.is_empty() {
        options.mode = "deka".to_string();
    }
    match options.mode.as_str() {
        "auto" | "deka" => {
            options.mode = "deka".to_string();
            Ok(options)
        }
        _ => Err("unsupported language mode; supported modes are `auto` and `deka`"),
    }
}

fn diagnostic_from_deka_syntax(
    diagnostic: &DekaDiagnostic,
    source: &str,
    filename: &str,
) -> Diagnostic {
    let help = diagnostic.help_text.as_deref().unwrap_or("");
    let severity = severity_label(diagnostic.severity);
    let rendered = strip_ansi_codes(&deka_validation::format_validation_error(
        source,
        filename,
        "DekaScript",
        diagnostic.line,
        diagnostic.column,
        &diagnostic.message,
        help,
        diagnostic.underline_length.max(1),
    ));
    Diagnostic {
        severity,
        code: "compiler".to_string(),
        message: diagnostic.message.clone(),
        filename: filename.to_string(),
        start_line: diagnostic.line,
        start_column: diagnostic.column,
        end_line: diagnostic.line,
        end_column: diagnostic
            .column
            .saturating_add(diagnostic.underline_length.max(1)),
        help: diagnostic
            .help_text
            .clone()
            .filter(|h| !h.trim().is_empty()),
        rendered,
    }
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

/// Remove ANSI escape sequences so the pre-rendered diagnostic is safe for
/// HTML <pre> elements in the browser (where env-controlled color is unwanted).
fn strip_ansi_codes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(c) = chars.next() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn internal_diagnostic(filename: &str, source: &str, message: String) -> Diagnostic {
    let rendered = strip_ansi_codes(&deka_validation::format_validation_error(
        source,
        filename,
        "Compiler Error",
        1,
        1,
        &message,
        "",
        1,
    ));
    Diagnostic {
        severity: "error",
        code: "emitter".to_string(),
        message: message.clone(),
        filename: filename.to_string(),
        start_line: 1,
        start_column: 1,
        end_line: 1,
        end_column: 1,
        help: None,
        rendered,
    }
}

fn request_error(filename: &str, message: &str) -> String {
    json(&CompileResponse {
        abi_version: ABI_VERSION,
        ok: false,
        output: None,
        diagnostics: vec![internal_diagnostic(filename, "", message.to_string())],
        metadata: CompileMetadata {
            filename,
            language: "unknown",
            compiler: CompilerMetadata::current(),
        },
    })
}

pub(crate) fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{\"abi_version\":1,\"ok\":false,\"diagnostics\":[{\"severity\":\"error\",\"code\":\"serialization\",\"message\":\"failed to serialize compiler response\",\"filename\":\"<unknown>\",\"start_line\":1,\"start_column\":1,\"end_line\":1,\"end_column\":1}],\"metadata\":{\"filename\":\"<unknown>\",\"language\":\"unknown\",\"compiler\":{\"name\":\"deka\",\"version\":\"unknown\",\"source_commit\":\"unknown\"}}}".to_string())
}

/// Allocate a single contiguous block containing a `WasmResult` header followed
/// by the JSON payload, then return a pointer to the header.
pub(crate) fn box_result(json: &str) -> *mut WasmResult {
    let header_size = std::mem::size_of::<WasmResult>();
    let total_size = header_size + json.len();
    let base = deka_compiler_alloc(total_size as u32);
    if base.is_null() {
        return ptr::null_mut();
    }

    let result_ptr = base.cast::<WasmResult>();
    let json_ptr = unsafe { base.add(header_size) };

    unsafe {
        ptr::copy_nonoverlapping(json.as_ptr(), json_ptr, json.len());
        (*result_ptr).ptr = json_ptr as u32;
        (*result_ptr).len = json.len() as u32;
    }

    result_ptr
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::Value;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TourLesson {
        id: String,
        title: String,
        expect_compile: bool,
        expect_error: Option<String>,
    }

    fn load_tour_lessons() -> Vec<(TourLesson, String)> {
        let tour_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/tour");
        let manifest_path = tour_dir.join("manifest.json");
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            return Vec::new();
        };
        let manifest: Vec<TourLesson> =
            serde_json::from_str(&raw).expect("tests/tour/manifest.json");

        let ds_files: Vec<String> = std::fs::read_dir(&tour_dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", tour_dir.display()))
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "ds" || ext == "dsx")
            })
            .map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .expect("tour .ds stem")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();

        let manifest_ids: std::collections::HashSet<&str> =
            manifest.iter().map(|lesson| lesson.id.as_str()).collect();
        for id in &ds_files {
            assert!(
                manifest_ids.contains(id.as_str()),
                "tests/tour/{id}.ds is not listed in manifest.json"
            );
        }
        for lesson in &manifest {
            assert!(
                ds_files.iter().any(|id| id == &lesson.id),
                "manifest id {} has no tests/tour/{}.ds",
                lesson.id,
                lesson.id
            );
        }

        manifest
            .into_iter()
            .map(|lesson| {
                let ds_path = tour_dir.join(format!("{}.ds", lesson.id));
                let dsx_path = tour_dir.join(format!("{}.dsx", lesson.id));
                let source_path = if ds_path.exists() {
                    ds_path
                } else if dsx_path.exists() {
                    dsx_path
                } else {
                    panic!("tests/tour/{}.ds or .dsx not found", lesson.id);
                };
                let source = std::fs::read_to_string(&source_path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", source_path.display()));
                (lesson, source)
            })
            .collect()
    }

    #[test]
    fn deka_mode_compiles_a_ds_fixture_with_structured_metadata() {
        let response: Value = serde_json::from_str(&compile_request(
            "const answer = 42;",
            "lesson.ds",
            r#"{"mode":"auto"}"#,
        ))
        .expect("response JSON");

        assert_eq!(response["abi_version"], ABI_VERSION);
        assert_eq!(response["ok"], true);
        assert_eq!(response["metadata"]["language"], "deka");
        assert_eq!(response["metadata"]["filename"], "lesson.ds");
        assert!(
            response["output"]["code"]
                .as_str()
                .is_some_and(|code| code.contains("const answer = 42"))
        );
        assert_eq!(response["diagnostics"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn compile_response_exposes_dev_plan_without_executing_it() {
        let source = r#"
const labels: Array<string> = build {
  return Ok(["Ada"])
}
"#;
        let response: Value = serde_json::from_str(&compile_request(
            source,
            "app/data.ds",
            r#"{"mode":"deka"}"#,
        ))
        .expect("response JSON");
        assert_eq!(response["ok"], true, "{response}");
        assert_eq!(response["output"]["devPlan"]["version"], 2, "{response}");
        assert_eq!(
            response["output"]["devPlan"]["slots"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            response["output"]["code"]
                .as_str()
                .is_some_and(|code| code.contains("deka:dev/"))
        );
    }

    #[test]
    fn module_base_rejects_bare_stdlib_without_declarations() {
        let response: Value = serde_json::from_str(&compile_request(
            r#"import { echo } from "io"; echo("hello");"#,
            "lesson.ds",
            r#"{"mode":"deka","moduleBase":"/tour/modules"}"#,
        ))
        .expect("response JSON");

        assert_eq!(response["ok"], false, "{response}");
        let diagnostics = response["diagnostics"]
            .as_array()
            .expect("diagnostics should be present");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("imported name `echo`"))),
            "expected unresolved import diagnostic, got: {response}"
        );
    }

    #[test]
    fn module_base_rejects_relative_imports_without_declarations() {
        let response: Value = serde_json::from_str(&compile_request(
            r#"import { add } from "./math.ds"; const r = add(1, 2);"#,
            "lesson.ds",
            r#"{"mode":"deka","moduleBase":"/tour/modules"}"#,
        ))
        .expect("response JSON");

        assert_eq!(response["ok"], false, "{response}");
        let diagnostics = response["diagnostics"]
            .as_array()
            .expect("diagnostics should be present");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("imported name `add`"))),
            "expected unresolved import diagnostic, got: {response}"
        );
    }

    #[test]
    fn module_base_rejects_unresolved_package_imports() {
        let response: Value = serde_json::from_str(&compile_request(
            r#"import { Widget } from "@acme/widgets"; const answer = 42;"#,
            "lesson.ds",
            r#"{"mode":"deka","moduleBase":"/tour/modules"}"#,
        ))
        .expect("response JSON");

        assert_eq!(response["ok"], false, "{response}");
        let diagnostics = response["diagnostics"]
            .as_array()
            .expect("diagnostics should be present");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("imported name `Widget`"))),
            "expected unresolved import diagnostic, got: {response}"
        );
    }

    #[test]
    fn rejects_phpx_mode_and_filename_without_fallback() {
        let filename_response: Value = serde_json::from_str(&compile_request(
            "function greeting($name: string): string { return $name; }",
            "legacy.phpx",
            r#"{"mode":"phpx"}"#,
        ))
        .expect("response JSON");

        assert_eq!(filename_response["ok"], false);
        assert_eq!(filename_response["metadata"]["language"], "unknown");
        assert!(
            filename_response["diagnostics"][0]["message"]
                .as_str()
                .is_some_and(|message| message.contains("only accepts .ds"))
        );

        let mode_response: Value = serde_json::from_str(&compile_request(
            "const answer = 42;",
            "lesson.ds",
            r#"{"mode":"phpx"}"#,
        ))
        .expect("response JSON");
        assert_eq!(mode_response["ok"], false);
        assert!(
            mode_response["diagnostics"][0]["message"]
                .as_str()
                .is_some_and(|message| message.contains("supported modes are `auto` and `deka`"))
        );
    }

    #[test]
    fn diagnostics_are_monaco_ready_and_native_parity_is_stable() {
        let source = "function broken(";
        let native: Value =
            serde_json::from_str(&compile_request(source, "broken.ds", r#"{"mode":"deka"}"#))
                .expect("native response JSON");
        let wasm_abi: Value =
            serde_json::from_str(&compile_request(source, "broken.ds", r#"{"mode":"auto"}"#))
                .expect("WASM ABI response JSON");

        assert_eq!(native["ok"], false);
        assert_eq!(native["diagnostics"], wasm_abi["diagnostics"]);
        let diagnostic = &native["diagnostics"][0];
        assert_eq!(diagnostic["severity"], "error");
        assert_eq!(diagnostic["filename"], "broken.ds");
        assert!(diagnostic["start_line"].as_u64().is_some());
        assert!(diagnostic["start_column"].as_u64().is_some());
    }

    #[test]
    fn mode_and_filename_errors_are_structured() {
        let response: Value =
            serde_json::from_str(&compile_request("", "lesson.txt", r#"{"mode":"auto"}"#))
                .expect("response JSON");
        assert_eq!(response["ok"], false);
        assert_eq!(response["diagnostics"][0]["code"], "emitter");
        assert_eq!(response["metadata"]["filename"], "lesson.txt");
    }

    #[test]
    fn ds_structs_use_deka_struct_factory_not_phpx_legacy_registry() {
        let source = r#"struct Point {
  x: number
  y: number
}

const origin = Point { x: 3, y: 4 };
"#;
        let response: Value =
            serde_json::from_str(&compile_request(source, "struct.ds", r#"{"mode":"deka"}"#))
                .expect("response JSON");

        assert_eq!(response["ok"], true, "{response}");
        let code = response["output"]["code"]
            .as_str()
            .expect("compiled code should be present");
        assert!(
            code.contains("const Point = __deka_struct(\"Point\")"),
            "expected struct factory, got:\n{code}"
        );
        assert!(
            code.contains("const origin = Point({ x: 3, y: 4 })"),
            "expected struct factory literal, got:\n{code}"
        );
        assert!(
            !code.contains("__phpxStructMethods"),
            "DS structs should not reference legacy DekaScript registry, got:\n{code}"
        );
    }

    #[test]
    fn all_website_tour_sources_match_the_native_abi_contract() {
        let lessons = load_tour_lessons();
        assert!(
            !lessons.is_empty(),
            "tests/tour/manifest.json missing or empty; dsc owns tour fixtures"
        );

        let tour_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/tour");
        for (lesson, source) in lessons {
            let extension = if tour_dir.join(format!("{}.dsx", lesson.id)).exists() {
                "dsx"
            } else {
                "ds"
            };
            let filename = format!("{}.{}", lesson.id, extension);
            // The standalone WASM compiler cannot resolve stdlib index packages
            // like `io` because it has no filesystem/network access. Skip those
            // lessons here; they are covered by the native language gate and the
            // live testsuite playground instead.
            if source.contains("from \"io\"") || source.contains("from 'io'") {
                continue;
            }
            let response: Value =
                serde_json::from_str(&compile_request(&source, &filename, r#"{"mode":"deka"}"#))
                    .unwrap_or_else(|error| {
                        panic!("{}: invalid response JSON: {error}", lesson.id)
                    });
            assert_eq!(
                response["ok"], lesson.expect_compile,
                "{} ({}): {response}",
                lesson.id, lesson.title
            );
            if lesson.expect_compile {
                assert!(
                    response["output"]["code"].as_str().is_some(),
                    "{}: {response}",
                    lesson.id
                );
            } else {
                assert!(
                    response["diagnostics"]
                        .as_array()
                        .is_some_and(|diagnostics| {
                            diagnostics
                                .iter()
                                .any(|diagnostic| diagnostic["severity"].as_str() == Some("error"))
                        }),
                    "{}: expected an error diagnostic: {response}",
                    lesson.id
                );
            }
        }
    }
}
