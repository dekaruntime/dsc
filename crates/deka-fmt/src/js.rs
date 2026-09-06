//! JavaScript output formatter backed by dprint-plugin-typescript.
//!
//! dprint gives us configurable indentation, line width, and Prettier-compatible
//! formatting while still being Rust-native and fast enough to run inside the
//! WASM compiler.

use dprint_core::configuration::NewLineKind;
use dprint_plugin_typescript::configuration::{ConfigurationBuilder, QuoteStyle};
use dprint_plugin_typescript::{FormatTextOptions, format_text};
use std::path::PathBuf;

/// Format a JavaScript source string using dprint-plugin-typescript.
pub fn format_js(source: &str) -> Result<String, String> {
    let config = ConfigurationBuilder::new()
        .line_width(80)
        .indent_width(2)
        .use_tabs(false)
        .new_line_kind(NewLineKind::LineFeed)
        .quote_style(QuoteStyle::PreferSingle)
        .build();

    let result = format_text(FormatTextOptions {
        path: &PathBuf::from("output.js"),
        extension: None,
        text: source.into(),
        config: &config,
        external_formatter: None,
    })
    .map_err(|err| format!("failed to format JavaScript: {err:?}"))?;

    Ok(result.unwrap_or_else(|| source.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_basic_function_with_two_space_indent() {
        let input = "function add(left,right){return left+right;}\n";
        let output = format_js(input).unwrap();
        assert!(output.contains("function add(left, right)"));
        assert!(output.contains("  return left + right;"));
    }

    #[test]
    fn formats_struct_emit() {
        let input = r#"const origin=(()=>{const __obj={"__struct":"Point","x":3,"y":4};const __m=globalThis.__phpxStructMethods?globalThis.__phpxStructMethods["Point"]:null;if(__m)Object.assign(__obj,__m);return __obj;})();
console.log((origin.x+origin.y));
"#;
        let output = format_js(input).unwrap();
        assert!(output.contains("const origin"));
        assert!(output.contains("console.log"));
        assert!(output.contains("  "));
    }
}
