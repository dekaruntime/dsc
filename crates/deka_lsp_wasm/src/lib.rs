//! Stable browser diagnostics ABI.
//!
//! This module intentionally only exposes source analysis. It has no document
//! store, filesystem, network, or LSP transport dependency.

use dekascript_lsp::{AnalysisContext, AnalysisDiagnostic, analyze, is_dekascript_context};
use serde::Serialize;

/// Version of the DekaScript diagnostics JSON and WASM ABI.
pub const DIAGNOSTICS_ABI_VERSION: u32 = 1;
const ALLOC_ALIGN: usize = 8;

#[derive(Serialize)]
struct DiagnosticsResponse<'a> {
    abi_version: u32,
    uri_or_path: &'a str,
    accepted: bool,
    diagnostics: Vec<AnalysisDiagnostic>,
}

/// Analyze source text for a `.ds` URI or path and encode the stable browser
/// response. Non-DekaScript contexts are explicitly rejected with no results.
pub fn diagnostics_json(source: &str, uri_or_path: &str) -> String {
    let context = AnalysisContext::new(uri_or_path);
    let accepted = is_dekascript_context(&context);
    let diagnostics = accepted
        .then(|| analyze(source, &context))
        .unwrap_or_default();
    serde_json::to_string(&DiagnosticsResponse {
        abi_version: DIAGNOSTICS_ABI_VERSION,
        uri_or_path,
        accepted,
        diagnostics,
    })
    .unwrap_or_else(|_| {
        "{\"abi_version\":1,\"uri_or_path\":\"\",\"accepted\":false,\"diagnostics\":[]}".to_string()
    })
}

/// Result header returned from [`deka_diagnostics_analyze`] and
/// [`deka_diagnostics_metadata`]. The JSON bytes immediately follow this
/// header in the same allocation.
#[repr(C)]
pub struct WasmResult {
    pub ptr: u32,
    pub len: u32,
}

/// Allocate a WASM-memory buffer for request bytes.
#[unsafe(no_mangle)]
pub extern "C" fn deka_diagnostics_alloc(size: u32) -> *mut u8 {
    if size == 0 {
        return core::ptr::null_mut();
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(size as usize, ALLOC_ALIGN) else {
        return core::ptr::null_mut();
    };
    // SAFETY: the layout was validated above.
    unsafe { std::alloc::alloc(layout) }
}

/// Free a buffer previously allocated by [`deka_diagnostics_alloc`].
///
/// # Safety
///
/// `ptr` must be null or an allocation returned by `deka_diagnostics_alloc`
/// with the exact original `size`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn deka_diagnostics_free(ptr: *mut u8, size: u32) {
    if ptr.is_null() || size == 0 {
        return;
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(size as usize, ALLOC_ALIGN) else {
        return;
    };
    // SAFETY: upheld by the function contract.
    unsafe { std::alloc::dealloc(ptr, layout) };
}

/// Return a JSON diagnostics response for UTF-8 source text and a UTF-8
/// URI/path. The caller owns the returned result allocation.
///
/// # Safety
///
/// Non-empty pointer/length pairs must point to readable UTF-8 buffers in
/// WASM memory. Invalid inputs return a structured rejected response.
#[unsafe(no_mangle)]
pub extern "C" fn deka_diagnostics_analyze(
    source_ptr: *const u8,
    source_len: u32,
    uri_ptr: *const u8,
    uri_len: u32,
) -> *mut WasmResult {
    let json = match (
        read_utf8(source_ptr, source_len),
        read_utf8(uri_ptr, uri_len),
    ) {
        (Ok(source), Ok(uri_or_path)) => diagnostics_json(source, uri_or_path),
        _ => diagnostics_json("", ""),
    };
    box_result(&json)
}

/// Return ABI metadata as a JSON result allocation.
#[unsafe(no_mangle)]
pub extern "C" fn deka_diagnostics_metadata() -> *mut WasmResult {
    box_result("{\"abi_version\":1,\"name\":\"deka_diagnostics\"}")
}

fn read_utf8<'a>(ptr: *const u8, len: u32) -> Result<&'a str, ()> {
    if len == 0 {
        return Ok("");
    }
    if ptr.is_null() {
        return Err(());
    }
    // SAFETY: non-empty pointers are checked and the caller owns the range.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len as usize) };
    core::str::from_utf8(bytes).map_err(|_| ())
}

fn box_result(json: &str) -> *mut WasmResult {
    let header_size = core::mem::size_of::<WasmResult>();
    let total_size = match header_size.checked_add(json.len()) {
        Some(size) if size <= u32::MAX as usize => size,
        _ => return core::ptr::null_mut(),
    };
    let base = deka_diagnostics_alloc(total_size as u32);
    if base.is_null() {
        return core::ptr::null_mut();
    }
    let result_ptr = base.cast::<WasmResult>();
    // SAFETY: `base` is a `total_size` allocation, and the payload starts
    // immediately after the fixed result header.
    unsafe {
        let json_ptr = base.add(header_size);
        core::ptr::copy_nonoverlapping(json.as_ptr(), json_ptr, json.len());
        (*result_ptr).ptr = json_ptr as u32;
        (*result_ptr).len = json.len() as u32;
    }
    result_ptr
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn stable_json_adapter_preserves_utf16_ranges() {
        let response: Value = serde_json::from_str(&diagnostics_json(
            "const label = 'é'; const = ;\n",
            "file:///workspace/main.ds",
        ))
        .expect("structured diagnostics JSON");

        assert_eq!(response["abi_version"], DIAGNOSTICS_ABI_VERSION);
        assert_eq!(response["accepted"], true);
        assert_eq!(response["diagnostics"][0]["severity"], "error");
        assert_eq!(
            response["diagnostics"][0]["range"]["start"]["character"],
            25
        );
        assert_eq!(response["diagnostics"][0]["range"]["end"]["character"], 26);
    }

    #[test]
    fn stable_json_adapter_rejects_non_dekascript_paths() {
        let response: Value = serde_json::from_str(&diagnostics_json(
            "const = ;",
            "file:///workspace/legacy.phpx",
        ))
        .expect("structured diagnostics JSON");

        assert_eq!(response["accepted"], false);
        assert_eq!(response["diagnostics"].as_array().map(Vec::len), Some(0));
    }
}
