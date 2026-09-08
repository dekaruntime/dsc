use std::fs;
use std::path::Path;
use std::process::Command;

fn cli_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dsc")
}

fn write(path: &Path, source: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, source).expect("write fixture");
}

fn run_in(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(cli_bin())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run dsc")
}

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn help_and_version_still_work_without_emitting() {
    let help = Command::new(cli_bin())
        .arg("--help")
        .output()
        .expect("dsc --help");
    assert!(help.status.success(), "{}", combined(&help));
    let text = combined(&help);
    assert!(text.contains("dist/"), "missing dist/ in help: {text}");
    assert!(
        text.contains("transpile"),
        "missing transpile in help: {text}"
    );
    assert!(text.contains("plan"), "missing plan in help: {text}");

    let version = Command::new(cli_bin())
        .arg("--version")
        .output()
        .expect("dsc --version");
    assert!(version.status.success(), "{}", combined(&version));
    let text = combined(&version);
    assert!(
        text.contains("dsc [version"),
        "unexpected version output: {text}"
    );
}

#[test]
fn plan_prints_dev_slots_without_executing_them() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(
        &root.join("main.ds"),
        "import { load } from \"./dev-data.ds\"\nconst labels: Array<string> = build { return Ok(load()) }\n",
    );
    write(
        &root.join("dev-data.ds"),
        "export fn load() Array<string> { return [\"Ada\"] }\n",
    );

    let output = run_in(root, &["plan", "main.ds"]);
    assert!(output.status.success(), "{}", combined(&output));
    let plan: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("plan must be JSON");
    assert_eq!(plan["version"], 2);
    assert!(plan.get("prerender").is_none(), "{plan}");
    assert_eq!(plan["slots"].as_array().map(Vec::len), Some(1));
    assert_eq!(plan["slots"][0]["binding"], "labels");
    assert_eq!(plan["slots"][0]["descriptor"]["node"], "array");
    assert!(
        plan["slots"][0]["entry"]
            .as_str()
            .is_some_and(|entry| entry.contains("./dev-data.js")),
        "dev entry did not use the emitted peer module: {plan}"
    );
}

#[test]
fn plan_reports_literal_prerender_export() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(&root.join("main.ds"), "export const prerender = false\n");

    let output = run_in(root, &["plan", "main.ds"]);
    assert!(output.status.success(), "{}", combined(&output));
    let plan: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("plan must be JSON");
    assert_eq!(plan["version"], 2);
    assert_eq!(plan["prerender"], false);
}

#[test]
fn plan_rejects_non_literal_prerender_export() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(
        &root.join("main.ds"),
        "const flag: boolean = true\nexport const prerender = flag\n",
    );

    let output = run_in(root, &["plan", "main.ds"]);
    assert!(!output.status.success(), "{}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("literal boolean"),
        "missing literal diagnostic: {text}"
    );
}

#[test]
fn no_project_trees_errors_with_guidance() {
    let temp = tempfile::tempdir().expect("tempdir");
    write(&temp.path().join("deka.json"), "{}\n");
    let output = run_in(temp.path(), &[]);
    assert!(!output.status.success(), "{}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("looked for app/, api/, src/"),
        "missing folder guidance: {text}"
    );
    assert!(
        text.contains("set a file arg") && text.contains("create one of those folders"),
        "missing file-arg guidance: {text}"
    );
}

#[test]
fn emits_app_and_api_as_preserve_js_trees() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root.join("deka.json").as_path(), "{}\n");
    write(
        &root.join("app/nested/math.ds"),
        "export const value = 41\n",
    );
    write(
        &root.join("app/main.ds"),
        "import { value } from './nested/math.ds'\nexport const answer = value + 1\n",
    );
    write(&root.join("app/readme.txt"), "do not copy\n");
    write(&root.join("api/handler.ds"), "export const ok = true\n");

    let output = run_in(root, &[]);
    assert!(output.status.success(), "{}", combined(&output));

    let app_main = fs::read_to_string(root.join("dist/app/main.js")).expect("app main");
    assert!(
        app_main.starts_with("// Generated by dsc transpile."),
        "{app_main}"
    );
    assert!(
        app_main.contains("./nested/math.js"),
        "relative import not rewritten: {app_main}"
    );
    assert!(root.join("dist/app/nested/math.js").is_file());
    assert!(
        !root.join("dist/app/readme.txt").exists(),
        "app/ should not copy non-source files"
    );
    assert!(root.join("dist/api/handler.js").is_file());
    assert!(!root.join("app/main.js").exists());
}

#[test]
fn src_is_one_to_one_compile_and_copy() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root.join("deka.json").as_path(), "{}\n");
    write(&root.join("src/lib.ds"), "export const n = 1\n");
    write(&root.join("src/nested/note.txt"), "keep me\n");
    write(&root.join("src/data.json"), "{\"ok\":true}\n");
    write(&root.join("src/node_modules/secret.txt"), "skip\n");
    write(
        &root.join("src/ds_modules/pkg/index.ds"),
        "export const x = 1\n",
    );
    write(&root.join("src/php_modules/pkg.txt"), "skip\n");
    write(&root.join("src/.deka/links.json"), "{}\n");
    write(&root.join("src/target/out.txt"), "skip\n");
    write(&root.join("src/dist/old.js"), "skip\n");
    write(&root.join("src/.git/HEAD"), "skip\n");

    let output = run_in(root, &[]);
    assert!(output.status.success(), "{}", combined(&output));

    let lib = fs::read_to_string(root.join("dist/src/lib.js")).expect("src lib");
    assert!(lib.contains("n"), "{lib}");
    assert_eq!(
        fs::read_to_string(root.join("dist/src/nested/note.txt")).expect("note"),
        "keep me\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("dist/src/data.json")).expect("json"),
        "{\"ok\":true}\n"
    );
    assert!(!root.join("dist/src/lib.ds").exists());
    assert!(!root.join("dist/src/node_modules").exists());
    assert!(!root.join("dist/src/ds_modules").exists());
    assert!(!root.join("dist/src/php_modules").exists());
    assert!(!root.join("dist/src/.deka").exists());
    assert!(!root.join("dist/src/target").exists());
    assert!(!root.join("dist/src/dist").exists());
    assert!(!root.join("dist/src/.git").exists());
}

#[test]
fn missing_bare_import_hard_fails_without_writing_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root.join("deka.json").as_path(), "{}\n");
    write(
        &root.join("app/main.ds"),
        "import { parse } from \"json\"\nexport const value = parse\n",
    );

    let output = run_in(root, &[]);
    assert!(!output.status.success(), "{}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("deka install") && text.contains("deka add json"),
        "missing install guidance: {text}"
    );
    assert!(
        text.contains("ds_modules") || text.contains("Unresolved Import"),
        "missing unresolved import framing: {text}"
    );
    assert!(
        !root.join("deka.lock").exists(),
        "dsc must not write deka.lock"
    );
    assert!(!root.join("dist/app/main.js").exists());
}

fn json_lock_without_hash() -> &'static str {
    r#"{
  "lockfileVersion": 1,
  "packages": {
    "@deka/json": [
      "@deka/json@0.0.0",
      "local",
      {},
      ""
    ]
  }
}
"#
}

fn json_lock_with_fs_hash(hash: &str) -> String {
    format!(
        r#"{{
  "lockfileVersion": 1,
  "packages": {{
    "@deka/json": [
      "@deka/json@0.0.0",
      "local",
      {{ "fsGraph": {{ "algo": "sha256", "hash": "{hash}" }} }},
      ""
    ]
  }}
}}
"#
    )
}

fn write_json_package(root: &Path) {
    write(
        &root.join("ds_modules/@deka/json/index.ds"),
        "export fn parse(s: string) string {\n  return s\n}\n",
    );
    write(
        &root.join("app/main.ds"),
        "import { parse } from \"json\"\nexport const value = parse(\"ok\")\n",
    );
}

#[test]
fn installed_bare_import_emits() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root.join("deka.json").as_path(), "{}\n");
    write(&root.join("deka.lock"), json_lock_without_hash());
    write_json_package(root);
    let lock_before = fs::read_to_string(root.join("deka.lock")).expect("lock");

    let output = run_in(root, &[]);
    assert!(output.status.success(), "{}", combined(&output));
    let emitted = fs::read_to_string(root.join("dist/app/main.js")).expect("emitted");
    assert!(emitted.contains("parse"), "{emitted}");
    assert_eq!(
        fs::read_to_string(root.join("deka.lock")).expect("lock after"),
        lock_before,
        "dsc must not write deka.lock"
    );
}

#[test]
fn installed_package_without_lock_entry_hard_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root.join("deka.json").as_path(), "{}\n");
    write_json_package(root);

    let output = run_in(root, &[]);
    assert!(!output.status.success(), "{}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("deka.lock")
            && text.contains("deka install")
            && text.contains("deka add json"),
        "missing lock-entry guidance: {text}"
    );
    assert!(
        !root.join("deka.lock").exists(),
        "dsc must not write deka.lock"
    );
    assert!(!root.join("dist/app/main.js").exists());
}

#[test]
fn lock_entry_without_ds_modules_hard_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root.join("deka.json").as_path(), "{}\n");
    write(&root.join("deka.lock"), json_lock_without_hash());
    write(
        &root.join("app/main.ds"),
        "import { parse } from \"json\"\nexport const value = parse(\"ok\")\n",
    );

    let output = run_in(root, &[]);
    assert!(!output.status.success(), "{}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("ds_modules")
            && text.contains("deka install")
            && text.contains("deka add json"),
        "missing ds_modules guidance: {text}"
    );
    assert!(!root.join("dist/app/main.js").exists());
}

#[test]
fn lock_integrity_mismatch_hard_fails() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root.join("deka.json").as_path(), "{}\n");
    write(
        &root.join("deka.lock"),
        &json_lock_with_fs_hash("0000000000000000000000000000000000000000000000000000000000000000"),
    );
    write_json_package(root);
    let lock_before = fs::read_to_string(root.join("deka.lock")).expect("lock");

    let output = run_in(root, &[]);
    assert!(!output.status.success(), "{}", combined(&output));
    let text = combined(&output);
    assert!(
        text.contains("Integrity Mismatch")
            && text.contains("deka install")
            && text.contains("deka add json"),
        "missing integrity guidance: {text}"
    );
    assert_eq!(
        fs::read_to_string(root.join("deka.lock")).expect("lock after"),
        lock_before,
        "dsc must not write deka.lock"
    );
    assert!(!root.join("dist/app/main.js").exists());
}

#[test]
fn file_arg_writes_adjacent_js() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("answer.ds");
    write(&source, "export const answer = 42\n");

    let output = run_in(temp.path(), &["answer.ds"]);
    assert!(output.status.success(), "{}", combined(&output));
    let emitted = fs::read_to_string(source.with_extension("js")).expect("adjacent js");
    assert!(
        emitted.starts_with("// Generated by dsc transpile."),
        "{emitted}"
    );
    assert!(emitted.contains("answer"), "{emitted}");
}

#[test]
fn outdir_flag_overrides_dist() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    write(root.join("deka.json").as_path(), "{}\n");
    write(&root.join("src/main.ds"), "export const n = 2\n");

    let output = run_in(root, &["--outdir", "build"]);
    assert!(output.status.success(), "{}", combined(&output));
    assert!(root.join("build/src/main.js").is_file());
    assert!(!root.join("dist/src/main.js").exists());
}

#[test]
fn transpile_file_is_unchanged() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("answer.ds");
    write(&source, "export const answer = 42\n");

    let output = Command::new(cli_bin())
        .arg("transpile")
        .arg(&source)
        .output()
        .expect("transpile");
    assert!(output.status.success(), "{}", combined(&output));
    let emitted = fs::read_to_string(source.with_extension("js")).expect("emitted");
    assert!(
        emitted.starts_with("// Generated by dsc transpile."),
        "{emitted}"
    );
}
