use super::*;

pub(crate) fn diagnostic_from_analysis(diagnostic: AnalysisDiagnostic) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: diagnostic.range.start.line,
                character: diagnostic.range.start.character,
            },
            end: Position {
                line: diagnostic.range.end.line,
                character: diagnostic.range.end.character,
            },
        },
        severity: Some(severity_to_lsp(diagnostic.severity)),
        code: Some(tower_lsp::lsp_types::NumberOrString::String(
            diagnostic.code,
        )),
        source: Some(LANGUAGE_ID.to_string()),
        message: diagnostic.message,
        ..Diagnostic::default()
    }
}

pub(crate) fn severity_to_lsp(severity: AnalysisSeverity) -> DiagnosticSeverity {
    match severity {
        AnalysisSeverity::Error => DiagnosticSeverity::ERROR,
        AnalysisSeverity::Warning => DiagnosticSeverity::WARNING,
        AnalysisSeverity::Information => DiagnosticSeverity::INFORMATION,
    }
}

pub(crate) fn should_skip_unused_import_warning(
    warning: &Diagnostic,
    unresolved_ranges: &std::collections::HashSet<(u32, u32, u32, u32)>,
) -> bool {
    if warning.severity != Some(DiagnosticSeverity::WARNING)
        || !warning.message.contains("Unused import")
    {
        return false;
    }
    let range = warning.range;
    let key = (
        range.start.line,
        range.start.character,
        range.end.line,
        range.end.character,
    );
    unresolved_ranges.contains(&key)
}
pub(crate) fn unresolved_import_diagnostics(
    source: &str,
    file_path: &str,
    workspace_roots: &[PathBuf],
) -> Vec<Diagnostic> {
    let root = match find_php_modules_root(Path::new(file_path), workspace_roots) {
        Some(root) => root,
        None => return Vec::new(),
    };

    let imports = parse_imports(source);
    if imports.is_empty() {
        return Vec::new();
    }

    let line_index = LineIndex::new(source);
    let mut module_cache: HashMap<(String, bool), Option<Vec<ExportInfo>>> = HashMap::new();
    let mut diagnostics = Vec::new();

    for import in imports {
        if import.imported == "default" {
            continue;
        }

        let key = (import.from.clone(), import.is_wasm);
        let exports = module_cache
            .entry(key)
            .or_insert_with(|| module_exports(&root, &import.from, import.is_wasm));
        let Some(exports) = exports else {
            continue;
        };

        let found = exports.iter().any(|export| export.name == import.imported);
        if found {
            continue;
        }

        diagnostics.push(Diagnostic {
            range: span_to_range(import.span, &line_index),
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(tower_lsp::lsp_types::NumberOrString::String(
                "Import Error".to_string(),
            )),
            source: Some(LANGUAGE_ID.to_string()),
            message: format!(
                "Import Error: Module '{}' has no export named '{}'.",
                import.from, import.imported
            ),
            ..Diagnostic::default()
        });
    }

    diagnostics
}

pub(crate) fn target_capability_diagnostics(
    source: &str,
    target_mode: TargetMode,
) -> Vec<Diagnostic> {
    if target_mode == TargetMode::Server {
        return Vec::new();
    }

    let imports = parse_imports(source);
    if imports.is_empty() {
        return Vec::new();
    }

    let line_index = LineIndex::new(source);
    let mut diagnostics = Vec::new();
    for import in imports {
        if let Some(block) = adwa_capability_block(&import.from) {
            diagnostics.push(Diagnostic {
                range: span_to_range(import.module_span.unwrap_or(import.span), &line_index),
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(tower_lsp::lsp_types::NumberOrString::String(
                    "Target Capability Error".to_string(),
                )),
                source: Some(LANGUAGE_ID.to_string()),
                message: format!(
                    "Target Capability Error: Module '{}' is unavailable for target 'adwa' ({}).\nhelp: {}",
                    import.from, block.reason, block.suggestion
                ),
                ..Diagnostic::default()
            });
        }
    }
    diagnostics
}

pub(crate) struct CapabilityBlock {
    reason: &'static str,
    suggestion: &'static str,
}

pub(crate) fn adwa_capability_block(module_spec: &str) -> Option<CapabilityBlock> {
    if module_spec == "db"
        || module_spec.starts_with("db/")
        || module_spec == "postgres"
        || module_spec.starts_with("postgres/")
        || module_spec == "mysql"
        || module_spec.starts_with("mysql/")
        || module_spec == "sqlite"
        || module_spec.starts_with("sqlite/")
    {
        return Some(CapabilityBlock {
            reason: "database host capability is disabled",
            suggestion: "Run with `dekascript.target = server` or move database access behind a server endpoint.",
        });
    }
    if module_spec == "process"
        || module_spec.starts_with("process/")
        || module_spec == "env"
        || module_spec.starts_with("env/")
    {
        return Some(CapabilityBlock {
            reason: "process/env host capability is disabled",
            suggestion: "Inject values through app config/context instead of reading process/env in `adwa`.",
        });
    }
    None
}
