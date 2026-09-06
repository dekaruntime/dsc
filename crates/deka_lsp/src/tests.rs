use super::*;

const MODULES_DIR: &str = "ds_modules";
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{}_{}", prefix, nonce));
    fs::create_dir_all(&dir).expect("mkdir");
    dir
}

#[test]
fn parses_import_module_path_with_span() {
    let line = "import { query } from 'db/postgres'";
    let (module, span) = parse_module_path_with_span(line, 0).expect("module span");
    assert_eq!(module, "db/postgres");
    assert_eq!(&line[span.start..span.end], "db/postgres");
}

#[test]
fn detects_import_module_at_cursor_offset() {
    let src = "import { query } from 'db/postgres'\n$query = 1\n";
    let offset = src.find("postgres").expect("postgres");
    let module = import_module_at_offset(src, offset).expect("module");
    assert_eq!(module, "db/postgres");
    let non_import = src.find("$query").expect("query var");
    assert!(import_module_at_offset(src, non_import).is_none());
}

#[test]
fn collects_all_matching_import_module_spans() {
    let src = "import { a } from 'db/postgres'\nimport { b } from 'db/mysql'\nimport { c } from 'db/postgres'\n";
    let spans = import_module_spans(src, "db/postgres");
    assert_eq!(spans.len(), 2);
    assert_eq!(&src[spans[0].start..spans[0].end], "db/postgres");
    assert_eq!(&src[spans[1].start..spans[1].end], "db/postgres");
}

#[test]
fn target_mode_defaults_to_server() {
    let params = InitializeParams::default();
    assert_eq!(
        TargetMode::from_initialize_params(&params),
        TargetMode::Server
    );
}

#[test]
fn target_mode_reads_adwa_from_init_options() {
    let mut params = InitializeParams::default();
    params.initialization_options = Some(json!({
        "dekascript": {
            "target": "adwa"
        }
    }));
    assert_eq!(
        TargetMode::from_initialize_params(&params),
        TargetMode::Adwa
    );
}

#[test]
fn target_capability_diagnostics_block_db_modules_for_adwa() {
    let source = "import { query } from 'db/postgres'\n";
    let diagnostics = target_capability_diagnostics(source, TargetMode::Adwa);
    assert_eq!(diagnostics.len(), 1, "diagnostics={diagnostics:?}");
    let first = &diagnostics[0];
    assert!(
        first.message.contains("db/postgres"),
        "message={}",
        first.message
    );
    assert!(first.message.contains("help:"), "message={}", first.message);
    assert_eq!(
        first.code,
        Some(tower_lsp::lsp_types::NumberOrString::String(
            "Target Capability Error".to_string()
        ))
    );
}

#[test]
fn target_capability_diagnostics_allow_db_modules_for_server() {
    let source = "import { query } from 'db/postgres'\n";
    let diagnostics = target_capability_diagnostics(source, TargetMode::Server);
    assert!(diagnostics.is_empty(), "diagnostics={diagnostics:?}");
}

#[test]
fn analysis_core_returns_structured_diagnostics_for_ds_context() {
    let diagnostics = analyze("const = ;\n", &AnalysisContext::new("file:///tmp/main.ds"));
    assert!(!diagnostics.is_empty());
    assert!(diagnostics.iter().all(|diagnostic| {
        (
            diagnostic.range.start.line,
            diagnostic.range.start.character,
        ) <= (diagnostic.range.end.line, diagnostic.range.end.character)
    }));
}

#[test]
fn finds_whole_word_occurrences_only() {
    let src = b"foo food foo\nfoo_bar foo\n";
    let spans = find_word_occurrences(src, "foo");
    let ranges: Vec<(usize, usize)> = spans.into_iter().map(|s| (s.start, s.end)).collect();
    assert_eq!(ranges, vec![(0, 3), (9, 12), (21, 24)]);
}

#[test]
fn collects_module_rename_edits_across_workspace_files() {
    let dir = temp_dir("dekascript_lsp_module_rename");
    let file_a = dir.join("a.ds");
    let file_b = dir.join("b.ds");
    let src_a = "import { query } from 'db/postgres'\n";
    let src_b = "import { exec } from 'db/postgres'\n";
    fs::write(&file_a, src_a).expect("write a");
    fs::write(&file_b, src_b).expect("write b");

    let uri_a = Url::from_file_path(&file_a).expect("uri a");
    let edits = collect_module_rename_edits(
        std::slice::from_ref(&dir),
        &uri_a,
        src_a,
        "db/postgres",
        "db/mysql",
    );
    assert_eq!(edits.len(), 2);
    let uri_b = Url::from_file_path(&file_b).expect("uri b");
    assert_eq!(edits.get(&uri_a).map(|v| v.len()), Some(1));
    assert_eq!(edits.get(&uri_b).map(|v| v.len()), Some(1));
}

#[test]
fn collects_symbol_rename_edits_with_word_boundaries_across_files() {
    let dir = temp_dir("dekascript_lsp_symbol_rename");
    let file_a = dir.join("a.ds");
    let file_b = dir.join("b.ds");
    let src_a = "const foo = 1;\nconst food = 2;\n";
    let src_b = "function run(foo: number): number { return foo; }\n";
    fs::write(&file_a, src_a).expect("write a");
    fs::write(&file_b, src_b).expect("write b");

    let uri_a = Url::from_file_path(&file_a).expect("uri a");
    let edits =
        collect_symbol_rename_edits(std::slice::from_ref(&dir), &uri_a, src_a, "foo", "bar");
    let uri_b = Url::from_file_path(&file_b).expect("uri b");
    assert_eq!(edits.get(&uri_a).map(|v| v.len()), Some(1));
    assert_eq!(edits.get(&uri_b).map(|v| v.len()), Some(2));
}

#[test]
fn collects_references_across_workspace_files() {
    let dir = temp_dir("dekascript_lsp_refs");
    let file_a = dir.join("a.ds");
    let file_b = dir.join("b.ds");
    let src_a = "function run(user: string): string { return user; }\n";
    let src_b = "const user = 'sami';\n";
    fs::write(&file_a, src_a).expect("write a");
    fs::write(&file_b, src_b).expect("write b");

    let uri_a = Url::from_file_path(&file_a).expect("uri a");
    let refs = collect_reference_locations(std::slice::from_ref(&dir), &uri_a, src_a, "user");
    assert_eq!(refs.len(), 3);
}

#[test]
fn provides_annotation_completion_items() {
    let src = "struct User {\n    $id: int @\n}\n";
    let offset = src.find('@').expect("annotation") + 1;
    let items = completion_for_annotation(src, offset).expect("annotation completion");
    assert!(items.iter().any(|item| item.label == "@autoIncrement"));
    assert!(items.iter().any(|item| item.label == "@relation"));
}

#[test]
fn provides_annotation_hover_docs() {
    let src = "struct User { $id: int @autoIncrement; }";
    let offset = src.find("autoIncrement").expect("annotation");
    let hover = hover_for_annotation(src, offset).expect("annotation hover");
    assert!(hover.contains("@autoIncrement"));
    assert!(hover.contains("Requires an `int` field"));
}

#[test]
fn resolves_project_alias_module_file() {
    let dir = temp_dir("dekascript_lsp_alias_resolve");
    let php_modules = dir.join(MODULES_DIR);
    let db = dir.join("db");
    fs::create_dir_all(&php_modules).expect("mkdir php_modules");
    fs::create_dir_all(&db).expect("mkdir db");
    fs::write(db.join("index.ds"), "export const x = 1;").expect("write module");

    let resolved = resolve_module_file(&php_modules, "@/db", false).expect("resolve alias");
    assert_eq!(resolved, db.join("index.ds"));
}

#[test]
fn finds_php_modules_from_workspace_roots_fallback() {
    let workspace = temp_dir("dekascript_lsp_workspace_modules");
    let php_modules = workspace.join(MODULES_DIR);
    let project = workspace.join("apps").join("sample");
    let file = project.join("main.ds");
    fs::create_dir_all(&php_modules).expect("mkdir php_modules");
    fs::create_dir_all(&project).expect("mkdir project");
    fs::write(&file, "import { x } from 'core/result'").expect("write file");

    let resolved =
        find_php_modules_root(&file, std::slice::from_ref(&workspace)).expect("resolve modules");
    assert_eq!(resolved, php_modules);
}

#[test]
fn completes_named_exports_for_import_clause() {
    let workspace = temp_dir("dekascript_lsp_import_exports");
    let php_modules = workspace.join(MODULES_DIR);
    let db = php_modules.join("db");
    fs::create_dir_all(&db).expect("mkdir db");
    fs::write(
        db.join("index.ds"),
        "export function stats() {}\nexport function status() {}\n",
    )
    .expect("write module");
    let file = workspace.join("main.ds");
    fs::write(&file, "import { sta } from 'db'\n").expect("write main");

    let source = fs::read_to_string(&file).expect("read main");
    let offset = source.find("sta").expect("sta") + 3;
    let items = completion_for_import(
        &source,
        file.to_str().expect("file"),
        offset,
        std::slice::from_ref(&workspace),
    )
    .expect("completion");
    let labels: Vec<String> = items.into_iter().map(|item| item.label).collect();
    assert!(
        labels.iter().any(|label| label == "stats"),
        "labels={labels:?}"
    );
    assert!(
        labels.iter().any(|label| label == "status"),
        "labels={labels:?}"
    );
}

#[test]
fn completes_named_exports_without_closing_brace() {
    let workspace = temp_dir("dekascript_lsp_import_partial");
    let php_modules = workspace.join(MODULES_DIR);
    let db = php_modules.join("db");
    fs::create_dir_all(&db).expect("mkdir db");
    fs::write(db.join("index.ds"), "export function stats() {}\n").expect("write module");
    let file = workspace.join("main.ds");
    let source = "import { sta from 'db'\n";

    let offset = source.find("sta").expect("sta") + 3;
    let items = completion_for_import(
        source,
        file.to_str().expect("file"),
        offset,
        std::slice::from_ref(&workspace),
    )
    .expect("completion");
    let labels: Vec<String> = items.into_iter().map(|item| item.label).collect();
    assert!(
        labels.iter().any(|label| label == "stats"),
        "labels={labels:?}"
    );
}

#[test]
fn reports_missing_named_import_export() {
    let workspace = temp_dir("dekascript_lsp_missing_export");
    let php_modules = workspace.join(MODULES_DIR);
    let db = php_modules.join("db");
    fs::create_dir_all(&db).expect("mkdir db");
    fs::write(db.join("index.ds"), "export function stats() {}\n").expect("write module");
    let file = workspace.join("main.ds");
    let source = "import { stat } from 'db'\n";
    fs::write(&file, source).expect("write main");

    let diagnostics = unresolved_import_diagnostics(
        source,
        file.to_str().expect("file"),
        std::slice::from_ref(&workspace),
    );
    assert_eq!(diagnostics.len(), 1, "diagnostics={diagnostics:?}");
    assert!(diagnostics[0].message.contains("no export named 'stat'"));
    assert_eq!(
        diagnostics[0].code,
        Some(tower_lsp::lsp_types::NumberOrString::String(
            "Import Error".to_string()
        ))
    );
}

#[test]
fn accepts_valid_named_import_alias() {
    let workspace = temp_dir("dekascript_lsp_import_alias_ok");
    let php_modules = workspace.join(MODULES_DIR);
    let db = php_modules.join("db");
    fs::create_dir_all(&db).expect("mkdir db");
    fs::write(db.join("index.ds"), "export function stats() {}\n").expect("write module");
    let file = workspace.join("main.ds");
    let source = "import { stats as stat } from 'db'\n";
    fs::write(&file, source).expect("write main");

    let diagnostics = unresolved_import_diagnostics(
        source,
        file.to_str().expect("file"),
        std::slice::from_ref(&workspace),
    );
    assert!(diagnostics.is_empty(), "diagnostics={diagnostics:?}");
}

#[test]
fn indexes_resolves_and_renames_only_dekascript_files() {
    let dir = temp_dir("dekascript_lsp_extension_boundary");
    let source = "import { value } from './module';\nconst renamed = value;\n";
    let ds_file = dir.join("main.ds");
    let legacy_phpx = dir.join("legacy.phpx");
    let legacy_php = dir.join("legacy.php");
    fs::write(&ds_file, source).expect("write DekaScript fixture");
    fs::write(dir.join("module.ds"), "export const value = 1;\n").expect("write module");
    fs::write(&legacy_phpx, "const renamed = value;\n").expect("write legacy fixture");
    fs::write(&legacy_php, "const renamed = value;\n").expect("write legacy fixture");

    assert_eq!(
        collect_dekascript_files(&dir),
        vec![ds_file.clone(), dir.join("module.ds")]
    );
    assert_eq!(
        resolve_module_file(&dir, "./module", false),
        Some(dir.join("module.ds"))
    );
    assert!(resolve_module_file(&dir, "./legacy.phpx", false).is_none());
    assert!(resolve_module_file(&dir, "./legacy.php", false).is_none());

    let uri = Url::from_file_path(&ds_file).expect("DekaScript URI");
    let edits = collect_symbol_rename_edits(
        std::slice::from_ref(&dir),
        &uri,
        source,
        "renamed",
        "updated",
    );
    assert_eq!(edits.len(), 1);
    assert!(edits.contains_key(&uri));
    assert!(!is_dekascript_path(&legacy_phpx));
    assert!(!is_dekascript_path(&legacy_php));
}

#[test]
fn skips_unused_warning_when_unresolved_import_exists_at_same_span() {
    let warning = Diagnostic {
        range: Range::new(Position::new(0, 9), Position::new(0, 13)),
        message: "Unused import 'stat'.".to_string(),
        severity: Some(DiagnosticSeverity::WARNING),
        ..Diagnostic::default()
    };
    let mut unresolved = std::collections::HashSet::new();
    unresolved.insert((0, 9, 0, 13));
    assert!(should_skip_unused_import_warning(&warning, &unresolved));
}

#[test]
fn keeps_non_unused_or_non_overlapping_warnings() {
    let warning = Diagnostic {
        range: Range::new(Position::new(0, 9), Position::new(0, 13)),
        message: "Unused import 'stat'.".to_string(),
        severity: Some(DiagnosticSeverity::WARNING),
        ..Diagnostic::default()
    };
    let unresolved = std::collections::HashSet::new();
    assert!(!should_skip_unused_import_warning(&warning, &unresolved));
}
