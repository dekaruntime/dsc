use deka_compile::module_graph::{compile_module_graph, FsModuleLoader};
use std::{fs, path::Path, process::Command};

#[test]
fn tuples_cross_module_reexports_and_captures() {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.cache/tuple-tests");
    fs::create_dir_all(&scratch).unwrap();
    let dir = tempfile::tempdir_in(scratch).unwrap();
    let root = dir.path();
    fs::write(
        root.join("pair.ds"),
        r#"
struct Label { text: string }
export fn pair() [number, Label] { return [7, Label { text: "seven" }]; }
const [n, label] = pair();
export { n, label };
export fn captured() number { return n; }
"#,
    )
    .unwrap();
    fs::write(
        root.join("barrel.ds"),
        "export { pair, n, label, captured } from \"./pair.ds\";",
    )
    .unwrap();
    fs::write(
        root.join("main.ds"),
        r#"
import { pair, n, label, captured } from "./barrel.ds";
const [count, text] = pair();
const result: string = text.text;
const value: number = captured();
const exported: number = n;
const name: string = label.text;
"#,
    )
    .unwrap();
    let graph = compile_module_graph(&root.join("main.ds"), &FsModuleLoader::new(root.into()))
        .unwrap_or_else(|e| panic!("{e:#?}"));
    for (path, js) in graph.modules {
        let assertions = if path.file_name().unwrap() == "main.ds" {
            "\nimport assert from 'node:assert/strict'; assert.equal(count, 7); assert.equal(result, 'seven'); assert.equal(value, 7); assert.equal(exported, 7); assert.equal(name, 'seven');"
        } else {
            ""
        };
        fs::write(
            path.with_extension("compiled.mjs"),
            format!(
                "{}\n{}{assertions}",
                graph.prelude,
                js.replace(".ds\"", ".compiled.mjs\"")
            ),
        )
        .unwrap();
    }
    let output = Command::new("node")
        .arg(root.join("main.compiled.mjs"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
