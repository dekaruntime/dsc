//! `import type` (rfd#12 ESM alignment amendment, dsc#281): a type-only
//! import erases entirely from emitted JS, and a value use of a type-only
//! binding is a typeck error naming the fix.

use deka_compile::module_graph::{compile_module_graph, FsModuleLoader};
use std::{fs, path::Path, process::Command};

fn scratch_dir() -> tempfile::TempDir {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache/import-type-tests");
    fs::create_dir_all(&scratch).unwrap();
    tempfile::tempdir_in(scratch).unwrap()
}

const TYPES_DS: &str = r#"
struct Point { x: number; y: number }
export { Point }
export fn helper() number { return 3 }
export fn make() Point { return Point { x: 2, y: 5 }; }
"#;

#[test]
fn import_type_whole_statement_erases_and_typechecks() {
    let dir = scratch_dir();
    let root = dir.path();
    fs::write(root.join("types.ds"), TYPES_DS).unwrap();
    fs::write(
        root.join("main.ds"),
        r#"
import type { Point } from "./types.ds";
import { make } from "./types.ds";
fn sum(p: Point) number { return p.x + p.y; }
export fn run() number { return sum(make()); }
const value: number = run();
"#,
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
    // A whole-statement `import type` names no runtime binding: only the
    // `make` value import survives, and `Point` never appears in emitted JS
    // (dsc#281 item 3). The sibling value import (`make`) proves the source
    // itself is not dropped -- only the type-only statement is.
    assert_eq!(
        main_js.matches("from \"./types.ds\"").count(),
        1,
        "expected exactly one surviving import from ./types.ds:\n{main_js}"
    );
    assert!(
        main_js.contains("import { make } from \"./types.ds\";"),
        "value import for `make` missing:\n{main_js}"
    );
    assert!(
        !main_js.contains("Point"),
        "type-only `Point` leaked into emitted JS:\n{main_js}"
    );

    for (path, js) in &graph.modules {
        fs::write(
            path.with_extension("compiled.mjs"),
            format!(
                "{}\n{}{}",
                graph.prelude,
                js.replace(".ds\"", ".compiled.mjs\""),
                if path.file_name().unwrap() == "main.ds" {
                    "\nimport assert from 'node:assert/strict'; assert.equal(value, 7);"
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
fn import_type_inline_mixed_keeps_value_import() {
    let dir = scratch_dir();
    let root = dir.path();
    fs::write(root.join("types.ds"), TYPES_DS).unwrap();
    fs::write(
        root.join("main.ds"),
        r#"
import { type Point, helper, make } from "./types.ds";
fn sum(p: Point) number { return p.x + helper(); }
export fn run() number { return sum(make()); }
const value: number = run();
"#,
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
    // The inline `type` marker applies per-specifier: `helper`/`make` stay a
    // real import, `Point` does not appear in it (dsc#281 item 3, mixed form).
    assert!(
        main_js.contains("import { helper, make } from \"./types.ds\";"),
        "value specifiers dropped from mixed import:\n{main_js}"
    );
    assert!(
        !main_js.contains("Point"),
        "type-only specifier leaked into emitted JS:\n{main_js}"
    );

    for (path, js) in &graph.modules {
        fs::write(
            path.with_extension("compiled.mjs"),
            format!(
                "{}\n{}{}",
                graph.prelude,
                js.replace(".ds\"", ".compiled.mjs\""),
                if path.file_name().unwrap() == "main.ds" {
                    "\nimport assert from 'node:assert/strict'; assert.equal(value, 5);"
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
fn import_type_value_use_is_rejected() {
    let dir = scratch_dir();
    let root = dir.path();
    fs::write(root.join("types.ds"), TYPES_DS).unwrap();
    fs::write(
        root.join("main.ds"),
        r#"
import type { helper } from "./types.ds";
export fn run() number { return helper(); }
"#,
    )
    .unwrap();

    let errors = compile_module_graph(&root.join("main.ds"), &FsModuleLoader::new(root.into()))
        .expect_err("calling a type-only import must be rejected");
    assert!(
        errors
            .iter()
            .any(|d| d.message.contains("import type") && d.message.contains("value")),
        "expected a diagnostic naming `import type` and the value-use fix, got {errors:?}"
    );
}
