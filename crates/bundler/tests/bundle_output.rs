/// Issue #9 — Verify that the bundler produces valid JavaScript output.
///
/// Creates a temp directory with a minimal JS entry file, runs the bundler,
/// and asserts the output is non-empty and parses as valid JS.
use bundler::{BundleOptions, VirtualSource, bundle_virtual_entry};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

struct SimpleVirtualSource {
    entry: PathBuf,
    code: String,
}

impl VirtualSource for SimpleVirtualSource {
    fn load_virtual(&self, path: &Path) -> Result<Option<String>, String> {
        if path == self.entry {
            Ok(Some(self.code.clone()))
        } else {
            Ok(None)
        }
    }
}

fn make_tmp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("deka_bundler_test_{}_{}", name, nanos));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test dir");
    dir
}

#[test]
fn bundle_output_is_valid_js() {
    let tmp = make_tmp_dir("valid_js_parse");
    let entry = tmp.join("main.js");
    let source = r#"
export function greet(name) {
    return "Hello, " + name + "!";
}
export const version = 1;
"#;
    std::fs::write(&entry, source).expect("write entry");

    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.clone(),
            minify: false,
            iife: false,
            client: false,
        },
        provider,
    )
    .expect("bundle should succeed");

    // Output must be non-empty
    assert!(!result.is_empty(), "bundle output is empty");

    // Output should contain the original values
    assert!(
        result.contains("greet") || result.contains("Hello"),
        "bundle output missing expected content:\n{}",
        &result[..result.len().min(500)]
    );

    // Verify the output parses as valid JS using swc_ecma_parser.
    // This catches syntax errors that would break V8 at runtime.
    use swc_common::SourceMap;
    use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};

    let cm = swc_common::sync::Lrc::new(SourceMap::default());
    let fm = cm.new_source_file(
        swc_common::FileName::Custom("bundle_output.js".into()).into(),
        result.clone(),
    );
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax {
            jsx: false,
            ..Default::default()
        }),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let parsed = parser.parse_module();
    assert!(
        parsed.is_ok(),
        "bundle output is not valid JS: {:?}\nFirst 500 chars:\n{}",
        parsed.err(),
        &result[..result.len().min(500)]
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn bundle_minified_output_is_valid_js() {
    let tmp = make_tmp_dir("minified_js_parse");
    let entry = tmp.join("main.js");
    let source = "export const x = 42;\nexport function add(a, b) { return a + b; }\n";
    std::fs::write(&entry, source).expect("write entry");

    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });
    let result = bundle_virtual_entry(
        &entry,
        BundleOptions {
            project_root: tmp.clone(),
            minify: true,
            iife: false,
            client: false,
        },
        provider,
    )
    .expect("minified bundle should succeed");

    assert!(!result.is_empty(), "minified bundle output is empty");

    // Parse as valid JS
    use swc_common::SourceMap;
    use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};

    let cm = swc_common::sync::Lrc::new(SourceMap::default());
    let fm = cm.new_source_file(
        swc_common::FileName::Custom("minified_output.js".into()).into(),
        result.clone(),
    );
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax {
            jsx: false,
            ..Default::default()
        }),
        Default::default(),
        StringInput::from(&*fm),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let parsed = parser.parse_module();
    assert!(
        parsed.is_ok(),
        "minified bundle output is not valid JS: {:?}\nOutput:\n{}",
        parsed.err(),
        &result[..result.len().min(500)]
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
