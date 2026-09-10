use std::fs;
use std::process::Command;

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dsc")
}

#[test]
fn single_file_and_project_check_reject_undeclared_virtual_imports() {
    let project = tempfile::tempdir().expect("tempdir");
    let source = project.path().join("page.dsx");
    fs::write(
        project.path().join("deka.json"),
        r#"{"name":"unresolved-import-fixture","version":"0.1.0"}"#,
    )
    .expect("project manifest");
    fs::write(
        &source,
        "import { Form } from \"ui/form\";\nconst page = <Form />;\n",
    )
    .expect("source fixture");

    for args in [
        vec!["check", "--single-file", source.to_str().unwrap()],
        vec!["check", source.to_str().unwrap()],
    ] {
        let output = Command::new(cli_bin())
            .args(&args)
            .current_dir(project.path())
            .output()
            .expect("run dsc check");
        assert!(
            !output.status.success(),
            "check unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("imported name `Form`"), "{stderr}");
        assert!(stderr.contains("ui/form"), "{stderr}");
    }
}
