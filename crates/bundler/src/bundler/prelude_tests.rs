// Bundler prelude injection (inside IIFE). Compiler prelude synthesis
// (deka#595, one `__deka_struct` per program) is tested in dsc
// (`graph_prelude_synthesized_once_from_union_demand`).
use super::tests::{make_tmp_dir, SimpleVirtualSource};
use super::*;

#[test]
fn bundle_iife_injects_prelude_inside_wrapper() {
    let tmp = make_tmp_dir("iife_prelude_injection");
    let entry = tmp.join("entry.js");
    let source = r#"
const __deka_main = async () => {
    globalThis.app = () => ({ status: 200, body: "ok" });
};
await __deka_main();
"#;
    std::fs::write(&entry, source).expect("write entry");
    let provider = Arc::new(SimpleVirtualSource {
        entry: entry.clone(),
        code: source.to_string(),
    });
    for minify in [false, true] {
        let result = bundle_virtual_entry(
            &entry,
            BundleOptions {
                project_root: tmp.path_buf(),
                minify,
                iife: true,
                client: false,
                prelude: Some("function __deka_struct(id) { return id; }\n".to_string()),
            },
            provider.clone(),
        )
        .expect("bundle should succeed");
        let trimmed = result.trim_start();
        assert!(
            trimmed.starts_with("(async function"),
            "minify={minify}: IIFE bundle must still start with the wrapper: {:?}",
            &trimmed[..60.min(trimmed.len())]
        );
        assert!(
            result.contains("__deka_struct"),
            "minify={minify}: prelude missing from bundle:\n{}",
            result
        );
        let wrapper_at = result.find("(async function").unwrap_or(usize::MAX);
        let prelude_at = result.find("__deka_struct").unwrap_or(usize::MAX);
        assert!(
            prelude_at > wrapper_at,
            "minify={minify}: prelude must be injected inside the IIFE:\n{}",
            result
        );
        use swc_common::SourceMap;
        use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, lexer::Lexer};
        let cm = swc_common::sync::Lrc::new(SourceMap::default());
        let fm = cm.new_source_file(
            swc_common::FileName::Custom(format!("iife_prelude_{minify}.js").into()).into(),
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
        assert!(
            parser.parse_script().is_ok(),
            "minify={minify}: injected bundle is not valid JS:\n{}",
            result
        );
    }
}
