use deka_compile::module_graph::{compile_module_graph, FsModuleLoader};
use std::{fs, path::Path, process::Command};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claims_cross_module");
    for file in fs::read_dir(fixtures).unwrap() {
        let path = file.unwrap().path();
        fs::copy(&path, dir.path().join(path.file_name().unwrap())).unwrap();
    }
    dir
}

fn run(root: &Path, entry: &str, expected: &str) {
    let graph = compile_module_graph(&root.join(entry), &FsModuleLoader::new(root.into()))
        .unwrap_or_else(|errors| panic!("{entry}: {errors:#?}"));
    for (path, js) in &graph.modules {
        // Give each ES module its own helper scope, as the native module host
        // does. Keep the real foreign shims and DS import boundaries intact.
        fs::write(
            path.with_extension("compiled.mjs"),
            format!(
                "{}\n{}",
                graph.prelude,
                js.replace(".ds\"", ".compiled.mjs\"")
                    .replace(".ds'", ".compiled.mjs'")
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

#[test]
fn original_repro_explicit_and_inferred_and_single_module_control() {
    let dir = fixture();
    run(dir.path(), "single.ds", "alice\n");
    run(dir.path(), "main.ds", "alice\n");
    fs::copy(dir.path().join("explicit.ds"), dir.path().join("api.ds")).unwrap();
    run(dir.path(), "main.ds", "alice\n");
}

#[test]
fn exported_interface_alias_and_barrel_are_erased() {
    let dir = fixture();
    let api = fs::read_to_string(dir.path().join("api.ds")).unwrap();
    fs::write(
        dir.path().join("api.ds"),
        format!("{api}\nexport {{ Claims }}\n"),
    )
    .unwrap();
    fs::write(
        dir.path().join("barrel.ds"),
        "export { claims, Claims as Payload } from \"./api.ds\"\n",
    )
    .unwrap();
    let main = fs::read_to_string(dir.path().join("main.ds"))
        .unwrap()
        .replace("{ claims }", "{ claims, Payload as ImportedClaims }")
        .replace("./api.ds", "./barrel.ds");
    fs::write(
        dir.path().join("main.ds"),
        format!("{main}\nconst empty: ImportedClaims = {{}}\n"),
    )
    .unwrap();
    run(dir.path(), "main.ds", "alice\n");
}

#[test]
fn nested_recursive_fields_and_aliases_keep_the_declaring_namespace() {
    let dir = fixture();
    fs::write(dir.path().join("api.ds"), r#"
alias Subject = string
interface Child { sub?: Subject; next?: Child }
interface Claims { child: Child }
summon { parse(json: string) Exception<Claims, JsError> } from "./parse.mjs"
export fn claims() {
 return match (parse("{\"child\":{\"sub\":\"alice\",\"next\":{\"sub\":\"bob\"}}}")) { Ok(c) => Ok(c), Throw(e) => Err("bad JSON") }
}
"#).unwrap();
    fs::write(
        dir.path().join("barrel.ds"),
        "export { claims } from \"./api.ds\"\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("forward.ds"),
        "import { claims } from \"./barrel.ds\"\nexport fn forwarded() { return claims() }\n",
    )
    .unwrap();
    fs::write(dir.path().join("main.ds"), r#"
import { forwarded } from "./forward.ds"
alias Subject = number
interface Claims { wrong: boolean }
interface Child { sub: number }
summon { total log(value: string) void } from "./log.mjs"
const first = match (forwarded()) { Ok(c) => match (c.child.sub) { Some(s) => s, None => "missing" }, Err(e) => e }
const second = match (forwarded()) { Ok(c) => match (c.child.next) { Some(n) => match (n.sub) { Some(s) => s, None => "missing" }, None => "none" }, Err(e) => e }
log(first)
log(second)
"#).unwrap();
    run(dir.path(), "main.ds", "alice\nbob\n");
}

#[test]
fn private_interface_is_not_importable_and_missing_fields_still_fail() {
    let dir = fixture();
    fs::write(
        dir.path().join("bad.ds"),
        "import { Claims } from \"./api.ds\"\n",
    )
    .unwrap();
    assert!(compile_module_graph(
        &dir.path().join("bad.ds"),
        &FsModuleLoader::new(dir.path().into())
    )
    .is_err());
    let main = fs::read_to_string(dir.path().join("main.ds"))
        .unwrap()
        .replace("c.sub", "c.missing");
    fs::write(dir.path().join("main.ds"), main).unwrap();
    let errors = compile_module_graph(
        &dir.path().join("main.ds"),
        &FsModuleLoader::new(dir.path().into()),
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|d| d.message.contains("has no field `missing`")),
        "{errors:?}"
    );
}

#[test]
fn imported_parameter_checks_and_same_spelled_payloads_are_independent() {
    let dir = fixture();
    fs::write(
        dir.path().join("other.ds"),
        r#"
interface Claims { sub: number }
export fn count(c: Claims) number { return c.sub }
export fn other() Claims { return { sub: 42 } }
"#,
    )
    .unwrap();
    let main = fs::read_to_string(dir.path().join("main.ds")).unwrap();
    fs::write(dir.path().join("main.ds"), format!("{main}\nimport {{ count, other }} from \"./other.ds\"\nconst value: number = count(other())\nconst direct: number = other().sub\n")).unwrap();
    run(dir.path(), "main.ds", "alice\n");
    for invalid in ["count({})", "count({ sub: \"wrong\" })"] {
        fs::write(
            dir.path().join("bad.ds"),
            format!("import {{ count }} from \"./other.ds\"\n{invalid}\n"),
        )
        .unwrap();
        let errors = compile_module_graph(
            &dir.path().join("bad.ds"),
            &FsModuleLoader::new(dir.path().into()),
        )
        .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|d| d.message.contains("expected argument type")),
            "{errors:?}"
        );
    }
}

#[test]
fn summoned_explicit_result_missing_field_and_exception_paths() {
    let dir = fixture();
    let api = fs::read_to_string(dir.path().join("api.ds"))
        .unwrap()
        .replace("claims()", "claims() Result<Claims, string>");
    fs::write(dir.path().join("api.ds"), &api).unwrap();
    run(dir.path(), "main.ds", "alice\n");
    fs::write(
        dir.path().join("parse.mjs"),
        "export function parse(json) { return {}; }\n",
    )
    .unwrap();
    run(dir.path(), "main.ds", "missing\n");
    fs::write(
        dir.path().join("parse.mjs"),
        "export function parse(json) { throw new Error('invalid'); }\n",
    )
    .unwrap();
    run(dir.path(), "main.ds", "bad JSON\n");
}

#[test]
fn private_interface_methods_keep_types_and_mutability() {
    let dir = fixture();
    fs::write(
        dir.path().join("api.ds"),
        r#"
alias Text = string
interface Claims { fn subject(prefix: Text) Text; mut fn update() void }
summon { total make() Claims } from "./parse.mjs"
export fn claims() Claims { return make() }
"#,
    )
    .unwrap();
    fs::write(dir.path().join("parse.mjs"), "export function make() { return { subject(prefix) { return prefix + 'alice'; }, update() {} }; }\n").unwrap();
    fs::write(
        dir.path().join("main.ds"),
        r#"
import { claims } from "./api.ds"
alias Text = number
interface Claims { sub: number }
summon { total log(value: string) void } from "./log.mjs"
log(claims().subject("hello "))
"#,
    )
    .unwrap();
    run(dir.path(), "main.ds", "hello alice\n");
    for (invalid, diagnostic) in [
        ("claims().subject(42)", "expected argument type"),
        ("const c = claims()\nc.update()", "immutable receiver"),
    ] {
        fs::write(
            dir.path().join("bad.ds"),
            format!("import {{ claims }} from \"./api.ds\"\n{invalid}\n"),
        )
        .unwrap();
        let errors = compile_module_graph(
            &dir.path().join("bad.ds"),
            &FsModuleLoader::new(dir.path().into()),
        )
        .unwrap_err();
        assert!(
            errors.iter().any(|d| d.message.contains(diagnostic)),
            "{errors:?}"
        );
    }
}

#[test]
fn recursive_interface_arguments_terminate_and_check_nonrecursive_fields() {
    let dir = fixture();
    fs::write(dir.path().join("accept.ds"), "interface Claims { next?: Claims; sub: string }\nexport fn accept(c: Claims) string { return c.sub }\n").unwrap();
    for (field_type, value, valid) in [("string", "\"alice\"", true), ("number", "42", false)] {
        fs::write(dir.path().join("api.ds"), format!("interface Claims {{ next?: Claims; sub: {field_type} }}\nexport fn claims() Claims {{ return {{ sub: {value} }} }}\n")).unwrap();
        fs::write(dir.path().join("main.ds"), "import { accept } from \"./accept.ds\"\nimport { claims } from \"./api.ds\"\nsummon { total log(value: string) void } from \"./log.mjs\"\nlog(accept(claims()))\n").unwrap();
        if valid {
            run(dir.path(), "main.ds", "alice\n");
        } else {
            let errors = compile_module_graph(
                &dir.path().join("main.ds"),
                &FsModuleLoader::new(dir.path().into()),
            )
            .unwrap_err();
            assert!(
                errors
                    .iter()
                    .any(|d| d.message.contains("expected argument type")),
                "{errors:?}"
            );
        }
    }
}
