//! End-to-end: `console` (rfd#44's console addition) compiles and runs.
//!
//! `console` needs no import and dsc emits its calls verbatim — the actual
//! WinterTC descriptor-based formatting of `Printable` values (structs,
//! `Option`, …) is the runtime's job (deka, not dsc; see rfd#44's
//! sequencing note). What dsc owns and this test proves: the program
//! compiles, the emitted JS is valid and runs under a real JS engine, the
//! literal `Printable` values it prints reach stdout, and `console.error`
//! reaches stderr rather than stdout — the WHATWG stream split, which any
//! JS host's own `console` (Node's included) already implements, so this
//! also proves dsc's emission does not fight that routing.

use deka_compile::compile_to_js;
use std::process::Command;

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

#[test]
fn console_log_and_error_run_natively_and_split_streams() {
    if !node_available() {
        eprintln!("skipping: node not available");
        return;
    }

    let source = r#"
console.log("hi", 1, Some(2));
console.error("boom");
"#;
    let result = compile_to_js(source, "console_e2e.ds").unwrap_or_else(|e| panic!("{e:#?}"));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("console_e2e.mjs");
    std::fs::write(&path, result.js).unwrap();

    let output = Command::new("node")
        .arg(&path)
        .output()
        .expect("node is required for this end-to-end test");
    assert!(
        output.status.success(),
        "program did not run: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(stdout.contains("hi"), "stdout should contain \"hi\": {stdout:?}");
    assert!(stdout.contains('1'), "stdout should contain 1: {stdout:?}");
    assert!(
        !stdout.contains("boom"),
        "console.error must not reach stdout: {stdout:?}"
    );
    assert!(
        stderr.contains("boom"),
        "console.error must reach stderr: {stderr:?}"
    );
}
