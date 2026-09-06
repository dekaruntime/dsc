//! Shared compile-and-report helper for CLI commands.

use deka_compile::{compile_to_js, format_diagnostic};

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
