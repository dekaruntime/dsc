//! `.d.ds` declaration files (rfd#39 2026-09-16 amendment, dsc#274).
//!
//! Each test proves its claim by reverting: before this change, `import { … }
//! from "./x.mjs"` did not resolve at all (`FsModuleLoader` only resolved
//! `.ds`/`.dsx`), so every positive test here fails to compile with the fix
//! reverted, and the two error tests assert diagnostics this change
//! introduces (they simply do not fire on `main` without it).

use deka_compile::module_graph::{compile_module_graph, FsModuleLoader};
use std::{fs, path::Path, process::Command};

fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, contents) in files {
        fs::write(dir.path().join(name), contents).unwrap();
    }
    dir
}

const LOG_MJS: &str = "export function log(value) { console.log(value); }\n";
const LOG_SUMMON: &str = "summon { total log(value: string) void } from \"./log.mjs\"\n";

/// Compile and run `entry`, asserting the compiled program's stdout.
fn run(root: &Path, entry: &str, expected: &str) {
    let graph = compile_module_graph(&root.join(entry), &FsModuleLoader::new(root.into()))
        .unwrap_or_else(|errors| panic!("{entry}: {errors:#?}"));
    for (path, js) in &graph.modules {
        // Only `.ds` dependency specifiers need renaming to their compiled
        // sibling; a `.mjs` specifier already names a real file on disk
        // (either authored directly, like `log.mjs`, or the module a `.d.ds`
        // describes, like `three.mjs`) and is left exactly as emitted —
        // "zero glue, direct import" (rfd#39).
        fs::write(
            path.with_extension("compiled.mjs"),
            format!(
                "{}\n{}",
                graph.prelude,
                js.replace(".ds\"", ".compiled.mjs\"")
                    .replace(".ds'", ".compiled.mjs'"),
            ),
        )
        .unwrap();
    }
    let output = Command::new("node")
        .arg(root.join(entry).with_extension("compiled.mjs"))
        .output()
        .expect("Node.js is required");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
}

fn compile_errors(root: &Path, entry: &str) -> Vec<String> {
    compile_module_graph(&root.join(entry), &FsModuleLoader::new(root.into()))
        .unwrap_err()
        .into_iter()
        .map(|d| d.message)
        .collect()
}

#[test]
fn import_from_declared_javascript_module() {
    let dir = dir_with(&[
        ("three.mjs", "export function revision() { return 'r1'; }\n"),
        ("three.d.ds", "export total fn revision() string\n"),
        ("log.mjs", LOG_MJS),
        (
            "main.ds",
            &format!(
                "import {{ revision }} from \"./three.mjs\"\n{LOG_SUMMON}log(revision())\n"
            ),
        ),
    ]);
    run(dir.path(), "main.ds", "r1\n");
}

#[test]
fn reexport_through_a_second_module() {
    let dir = dir_with(&[
        ("three.mjs", "export function revision() { return 'r1'; }\n"),
        ("three.d.ds", "export total fn revision() string\n"),
        ("log.mjs", LOG_MJS),
        (
            "barrel.ds",
            "import { revision } from \"./three.mjs\"\nexport { revision }\n",
        ),
        (
            "main.ds",
            &format!(
                "import {{ revision }} from \"./barrel.ds\"\n{LOG_SUMMON}log(revision())\n"
            ),
        ),
    ]);
    run(dir.path(), "main.ds", "r1\n");
}

#[test]
fn missing_declaration_names_both_fixes() {
    let dir = dir_with(&[
        ("nodecl.mjs", "export function f() { return 1; }\n"),
        (
            "main.ds",
            "import { f } from \"./nodecl.mjs\"\n",
        ),
    ]);
    let errors = compile_errors(dir.path(), "main.ds");
    assert!(
        errors
            .iter()
            .any(|m| m.contains("nodecl.d.ds") && m.contains("dsc summon infer")),
        "{errors:?}"
    );
}

#[test]
fn body_in_declaration_is_an_error() {
    let dir = dir_with(&[
        ("three.mjs", "export function revision() { return 'r1'; }\n"),
        (
            "three.d.ds",
            "export fn revision() string { return \"nope\" }\n",
        ),
        (
            "main.ds",
            "import { revision } from \"./three.mjs\"\n",
        ),
    ]);
    let errors = compile_errors(dir.path(), "main.ds");
    assert!(
        errors.iter().any(|m| m.contains("may not contain a function body")),
        "{errors:?}"
    );
}

#[test]
fn structural_mismatch_missing_export() {
    let dir = dir_with(&[
        ("three.mjs", "export function revision() { return 'r1'; }\n"),
        ("three.d.ds", "export total fn ghost() void\n"),
        (
            "main.ds",
            "import { revision } from \"./three.mjs\"\n",
        ),
    ]);
    let errors = compile_errors(dir.path(), "main.ds");
    assert!(
        errors
            .iter()
            .any(|m| m.contains("ghost") && m.contains("export does not exist")),
        "{errors:?}"
    );
}

#[test]
fn structural_mismatch_arity() {
    let dir = dir_with(&[
        ("three.mjs", "export function add(a) { return a; }\n"),
        (
            "three.d.ds",
            "export total fn add(a: number, b: number) number\n",
        ),
        (
            "main.ds",
            "import { add } from \"./three.mjs\"\n",
        ),
    ]);
    let errors = compile_errors(dir.path(), "main.ds");
    assert!(
        errors.iter().any(|m| m.contains("incompatible arity")),
        "{errors:?}"
    );
}

#[test]
fn total_vs_default_exception_at_a_call_site() {
    let dir = dir_with(&[
        (
            "three.mjs",
            "export function safe() { return 'ok'; }\nexport function risky() { return 'maybe'; }\n",
        ),
        (
            "three.d.ds",
            "export total fn safe() string\nexport fn risky() string\n",
        ),
        ("log.mjs", LOG_MJS),
        (
            "main.ds",
            &format!(
                "import {{ safe, risky }} from \"./three.mjs\"\n{LOG_SUMMON}const direct: string = safe()\nlog(direct)\nconst handled = match (risky()) {{ Ok(v) => v, Throw(e) => \"error\" }}\nlog(handled)\n"
            ),
        ),
    ]);
    // A non-`total` declared return is `Exception<T, JsError>`: calling it as
    // a bare `string` is a type error.
    let wrong = dir_with(&[
        (
            "three.mjs",
            "export function risky() { return 'maybe'; }\n",
        ),
        ("three.d.ds", "export fn risky() string\n"),
        (
            "main.ds",
            "import { risky } from \"./three.mjs\"\nconst direct: string = risky()\n",
        ),
    ]);
    let errors = compile_errors(wrong.path(), "main.ds");
    assert!(!errors.is_empty(), "expected a type error, got none");

    run(dir.path(), "main.ds", "ok\nmaybe\n");
}
