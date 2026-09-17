//! Default exports and imports (rfd#12 ESM alignment amendment, dsc#280).

use deka_compile::module_graph::{compile_module_graph, FsModuleLoader};
use std::{fs, path::Path, process::Command};

fn scratch_dir() -> tempfile::TempDir {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache/default-export-tests");
    fs::create_dir_all(&scratch).unwrap();
    tempfile::tempdir_in(scratch).unwrap()
}

fn run(root: &Path, main_assertions: &str) {
    let graph = compile_module_graph(&root.join("main.ds"), &FsModuleLoader::new(root.into()))
        .unwrap_or_else(|errors| panic!("{errors:#?}"));
    for (path, js) in &graph.modules {
        fs::write(
            path.with_extension("compiled.mjs"),
            format!(
                "{}\n{}{}",
                graph.prelude,
                js.replace(".ds\"", ".compiled.mjs\""),
                if path.file_name().unwrap() == "main.ds" {
                    main_assertions
                } else {
                    ""
                }
            ),
        )
        .unwrap();
    }
    let output = Command::new("node")
        .arg(root.join("main.compiled.mjs"))
        .output()
        .expect("Node.js is required");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn default_function_export_is_called_through_default_import() {
    let dir = scratch_dir();
    let root = dir.path();
    fs::write(
        root.join("page.ds"),
        "export default fn Page() number { return 42; }\n",
    )
    .unwrap();
    fs::write(
        root.join("main.ds"),
        "import Page from \"./page.ds\";\nconst value: number = Page();\n",
    )
    .unwrap();

    let graph = compile_module_graph(&root.join("main.ds"), &FsModuleLoader::new(root.into()))
        .unwrap_or_else(|errors| panic!("{errors:#?}"));
    let page_js = graph
        .modules
        .iter()
        .find(|(path, _)| path.file_name().unwrap() == "page.ds")
        .map(|(_, js)| js.clone())
        .expect("page.ds compiled");
    // Emit ESM `export default` unchanged (dsc#280 item 6).
    assert!(
        page_js.contains("export default function Page"),
        "expected `export default function Page`, got:\n{page_js}"
    );
    let main_js = graph
        .modules
        .iter()
        .find(|(path, _)| path.file_name().unwrap() == "main.ds")
        .map(|(_, js)| js.clone())
        .expect("main.ds compiled");
    assert!(
        main_js.contains("import Page from \"./page.ds\";"),
        "expected a default import, got:\n{main_js}"
    );

    run(
        root,
        "\nimport assert from 'node:assert/strict'; assert.equal(value, 42);",
    );
}

#[test]
fn default_named_binding_export_reexport_and_struct_identity() {
    // `export default app` (a named binding) and a struct default-exported
    // the same way; cross-module identity holds through the default import
    // (dsc#280 items 1 and 3).
    let dir = scratch_dir();
    let root = dir.path();
    fs::write(
        root.join("app.ds"),
        r#"
struct Point { x: number; y: number }
export default Point;
fn make() Point { return Point { x: 3, y: 4 }; }
export { make };
"#,
    )
    .unwrap();
    fs::write(
        root.join("main.ds"),
        r#"
import Point from "./app.ds";
import { make } from "./app.ds";
fn sum(p: Point) number { return p.x + p.y; }
const value: number = sum(make());
"#,
    )
    .unwrap();

    run(
        root,
        "\nimport assert from 'node:assert/strict'; assert.equal(value, 7);",
    );
}

#[test]
fn mixed_default_and_named_import_keeps_both() {
    let dir = scratch_dir();
    let root = dir.path();
    fs::write(
        root.join("page.ds"),
        "export default fn Page() number { return 10; }\nexport fn helper() number { return 5; }\n",
    )
    .unwrap();
    fs::write(
        root.join("main.ds"),
        "import Page, { helper } from \"./page.ds\";\nconst value: number = Page() + helper();\n",
    )
    .unwrap();

    let graph = compile_module_graph(&root.join("main.ds"), &FsModuleLoader::new(root.into()))
        .unwrap_or_else(|errors| panic!("{errors:#?}"));
    let main_js = graph
        .modules
        .iter()
        .find(|(path, _)| path.file_name().unwrap() == "main.ds")
        .map(|(_, js)| js.clone())
        .expect("main.ds compiled");
    assert!(
        main_js.contains("import Page, { helper } from \"./page.ds\";"),
        "expected a mixed default+named import, got:\n{main_js}"
    );

    run(
        root,
        "\nimport assert from 'node:assert/strict'; assert.equal(value, 15);",
    );
}

#[test]
fn reexport_default_as_named_export() {
    // `export { default as json } from "./json"` (dsc#280 item 4).
    let dir = scratch_dir();
    let root = dir.path();
    fs::write(
        root.join("page.ds"),
        "export default fn Page() number { return 9; }\n",
    )
    .unwrap();
    fs::write(
        root.join("barrel.ds"),
        "export { default as Comp } from \"./page.ds\";\n",
    )
    .unwrap();
    fs::write(
        root.join("main.ds"),
        "import { Comp } from \"./barrel.ds\";\nconst value: number = Comp();\n",
    )
    .unwrap();

    run(
        root,
        "\nimport assert from 'node:assert/strict'; assert.equal(value, 9);",
    );
}

#[test]
fn anonymous_default_exports_are_rejected() {
    let dir = scratch_dir();
    let root = dir.path();
    for (name, source) in [
        ("fn.ds", "export default fn () { return 1; }\n"),
        ("obj.ds", "export default { a: 1 };\n"),
    ] {
        fs::write(root.join("main.ds"), source).unwrap();
        let errors = compile_module_graph(
            &root.join("main.ds"),
            &FsModuleLoader::new(root.into()),
        )
        .expect_err(&format!("{name}: anonymous default export must be rejected"));
        assert!(
            errors.iter().any(|d| d.message.contains("name")),
            "{name}: expected a diagnostic about naming the default export, got {errors:?}"
        );
    }
}
