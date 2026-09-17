use std::fs;
use std::process::Command;

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dsc")
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../deka_compile/tests/fixtures/summon")
        .join(name)
}

#[test]
fn infer_writes_the_draft_beside_the_module_by_default() {
    let temp = tempfile::tempdir().unwrap();
    let module = temp.path().join("infer_basic.mjs");
    fs::copy(fixture("infer_basic.mjs"), &module).unwrap();
    let output = Command::new(cli_bin())
        .args(["summon", "infer"])
        .arg(&module)
        .output()
        .expect("run dsc summon infer");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // `<module>.d.ds` beside the module (rfd#39 2026-09-16 amendment,
    // dsc#274) — no `--out` needed for the common case.
    let got = fs::read_to_string(temp.path().join("infer_basic.d.ds")).unwrap();
    let expected = fs::read_to_string(fixture("infer_basic.draft.ds")).unwrap();
    assert_eq!(got, expected);
}

#[test]
fn infer_out_writes_the_draft_to_an_explicit_path() {
    let temp = tempfile::tempdir().unwrap();
    let module = temp.path().join("vendor/scene.mjs");
    fs::create_dir_all(module.parent().unwrap()).unwrap();
    fs::copy(fixture("infer_throw.mjs"), &module).unwrap();
    let out = temp.path().join("types/scene.d.ds");

    let output = Command::new(cli_bin())
        .args(["summon", "infer"])
        .arg(&module)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("run dsc summon infer --out");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let got = fs::read_to_string(&out).unwrap();
    assert!(got.contains("DRAFT — review before committing"), "{got}");
    assert!(
        got.lines()
            .map(str::trim)
            .any(|line| line.starts_with("export fn boom(") && line.contains("Exception<")),
        "throwing export must not be drafted total:\n{got}"
    );
    // The declaration file has no `from` clause: its own path pairs it with
    // the module positionally (dsc#274).
    assert!(!got.contains("from \""), "{got}");
}

#[test]
fn infer_rejects_non_mjs() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("scene.js");
    fs::write(&source, "export function f() {}").unwrap();
    let output = Command::new(cli_bin())
        .args(["summon", "infer"])
        .arg(&source)
        .output()
        .expect("run dsc summon infer");
    assert!(!output.status.success());
}
