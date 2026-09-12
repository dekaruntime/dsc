use deka_compile::{compile_to_js, compile_to_js_with_options, CompileOptions};
use std::process::Command;
fn compile(source: &str) -> Result<deka_compile::CompileResult, Vec<deka_syntax::Diagnostic>> {
    compile_to_js(
        source,
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/summon/main.ds"),
    )
}
fn reject(source: &str, message: &str) {
    let errors = compile(source).unwrap_err();
    assert!(
        errors.iter().any(|d| d.message.contains(message)),
        "{source}: {errors:?}"
    );
}
#[test]
fn nominal_and_contained() {
    let prefix = "opaque type Handle\nsummon { total handle(): Handle, total read(h: Handle): number } from \"./foreign.mjs\"\n";
    compile(&format!("{prefix}struct Stored {{ h: Handle }}\nconst h = handle()\nconst stored = Stored {{ h: h }}\nconst n = read(stored.h)\n")).unwrap();
    for (body, diagnostic) in [
        ("const h = Handle {}", "cannot construct opaque"),
        ("const h = Handle()", "cannot construct opaque"),
        ("const n = handle().secret", "cannot access field"),
        ("const n = handle().getType()", "cannot inspect opaque"),
        ("const n = handle() == handle()", "cannot apply operators"),
        (
            "const n = match handle() { Handle { secret } => secret }",
            "cannot destructure opaque",
        ),
        ("const f = handle", "cannot escape"),
        ("export { handle }", "file-private"),
        (
            "opaque type Other\nconst h: Other = handle()",
            "expected type `Other`",
        ),
    ] {
        reject(&format!("{prefix}{body}"), diagnostic);
    }
    reject(
        "summon { number(): number } from \"./foreign.mjs\"",
        "explicit `total`",
    );
    reject(
        "summon { fail(): Exception<number, string> } from \"./foreign.mjs\"\nconst x = fail()",
        "match",
    );
}
#[test]
fn structural_gate() {
    for (signature, diagnostic) in [
        ("total nonexistent(): number", "export does not exist"),
        (
            "total scalar(): number",
            "not a statically verifiable function",
        ),
        ("total read(): number", "incompatible arity"),
        ("total number(n: number): number", "incompatible arity"),
    ] {
        let errors =
            compile(&format!("summon {{ {signature} }} from \"./foreign.mjs\"")).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|d| d.message.contains("foreign.mjs") && d.message.contains(diagnostic)),
            "{errors:?}"
        );
    }
    compile("summon { total defaulted(): number, total variadic(a: number, b: number): number, total renamed(v: number): number } from \"./foreign.mjs\"").unwrap();
    let options = CompileOptions {
        foreign_modules: [("./foreign.mjs".into(), "export function broken( {".into())].into(),
        ..Default::default()
    };
    assert!(compile_to_js_with_options(
        "summon { total broken(): number } from \"./foreign.mjs\"",
        "virtual.ds",
        options
    )
    .unwrap_err()[0]
        .message
        .contains("cannot verify summoned module"));
}
#[test]
fn native_marshaling_and_exception_identity() {
    let source = r#"
opaque type Handle
summon {
 total handle(): Handle,
 total read(h: Handle): number,
 total absent(): Option<number>,
 total missing(): Option<number>,
 total number(): Option<number>,
 total noop(): void,
 total optional(v: Option<number>): number,
 fail(): Exception<number, string>,
 total renamed(n: number): number,
 total later(): Promise<number>
} from "./foreign.mjs"
fn (h Handle) value() number { return read(h); }
export fn inspect() number { return handle().value(); }
export fn none_null() Option<number> { return absent(); }
export fn none_undefined() Option<number> { return missing(); }
export fn some() Option<number> { return number(); }
export fn void_call() { noop(); }
export fn outbound() number { return optional(None) + optional(Some(3)); }
export fn thrown() Exception<number, string> { return fail(); }
export async fn asynchronous() Promise<number> { return renamed(await later()); }
"#;
    let js = compile(source).unwrap().js;
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/summon/foreign.mjs"
        ),
        dir.path().join("foreign.mjs"),
    )
    .unwrap();
    std::fs::write(dir.path().join("main.mjs"), js).unwrap();
    std::fs::write(
        dir.path().join("test.mjs"),
        r#"
import assert from 'node:assert/strict';
import * as m from './main.mjs';
assert.equal(m.inspect(), 42);
assert.equal(m.none_null(), undefined);
assert.equal(m.none_undefined(), undefined);
assert.equal(typeof m.some(), 'number');
assert.equal(m.some(), 7);
assert.equal(m.void_call(), undefined);
assert.equal(m.outbound(), 3);
assert.throws(m.thrown, e => e === 'foreign failure');
assert.equal(await m.asynchronous(), 9);
"#,
    )
    .unwrap();
    let output = Command::new("node")
        .arg(dir.path().join("test.mjs"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn contract_bug_bypasses_checked_ledger() {
    let source = r#"
opaque type Handle
summon { total handle(): Handle, total absent(): number, missing(): Exception<number, string> } from "./foreign.mjs"
fn (h Handle) wrong() number { return absent(); }
export fn receiver_lie() number { return handle().wrong(); }
export fn total_lie() number { return absent(); }
export fn checked_lie() number { return match missing() { Ok(v) => v, Throw(e) => 0 }; }
export fn as_data() Result<number, string> { return missing().to_result(); }
"#;
    let js = compile(source).unwrap().js;
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/summon/foreign.mjs"
        ),
        dir.path().join("foreign.mjs"),
    )
    .unwrap();
    std::fs::write(dir.path().join("main.mjs"), js).unwrap();
    std::fs::write(dir.path().join("test.mjs"), "import assert from 'node:assert/strict'; import * as m from './main.mjs'; for (const f of Object.values(m)) assert.throws(f, e => e.name === 'BoundaryError');").unwrap();
    let output = Command::new("node")
        .arg(dir.path().join("test.mjs"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn opaque_types_across_files_keep_identity() {
    use deka_compile::module_graph::{compile_module_graph, FsModuleLoader};
    let dir = tempfile::tempdir().unwrap();
    let foreign = include_str!("fixtures/summon/foreign.mjs");
    std::fs::write(dir.path().join("foreign.mjs"), foreign).unwrap();
    let a = "opaque type Handle\nexport { Handle }\nsummon { total handle(): Handle } from \"./foreign.mjs\"\nexport fn get() Handle { return handle(); }";
    std::fs::write(dir.path().join("a.ds"), a).unwrap();
    std::fs::write(dir.path().join("b.ds"), a).unwrap();
    let entry = dir.path().join("main.ds");
    std::fs::write(
        &entry,
        "import { Handle as A, get } from \"./a.ds\"\nconst h: A = get()",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("reexport.ds"),
        "export { Handle as PublicHandle } from \"./a.ds\"\nexport { get } from \"./a.ds\"",
    )
    .unwrap();
    let loader = FsModuleLoader::new(dir.path().into());
    let graph = compile_module_graph(&entry, &loader).unwrap();
    assert!(!graph.modules[&entry.canonicalize().unwrap()].contains("Handle as A"));
    assert!(
        !graph.modules[&dir.path().join("a.ds").canonicalize().unwrap()]
            .contains("export { Handle }")
    );
    std::fs::write(
        &entry,
        "import { PublicHandle, get } from \"./reexport.ds\"\nconst h: PublicHandle = get()",
    )
    .unwrap();
    let reexported = compile_module_graph(&entry, &loader).unwrap();
    assert!(
        !reexported.modules[&dir.path().join("reexport.ds").canonicalize().unwrap()]
            .contains("PublicHandle")
    );
    std::fs::write(&entry, "import { get } from \"./a.ds\"\nimport { Handle } from \"./b.ds\"\nconst h: Handle = get()").unwrap();
    assert!(
        compile_module_graph(&entry, &loader).is_err(),
        "same-spelled opaque declarations in different modules must differ"
    );
}

#[test]
fn verification_is_repeated_and_reassignment_is_not_callable_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.ds");
    let module = dir.path().join("foreign.mjs");
    let source = "summon { total f(): number } from \"./foreign.mjs\"";
    std::fs::write(&module, "export function f() { return 1; }").unwrap();
    compile_to_js(source, path.to_str().unwrap()).unwrap();
    std::fs::write(&module, "export function f() { return 1; } f = 3;").unwrap();
    assert!(compile_to_js(source, path.to_str().unwrap())
        .unwrap_err()
        .iter()
        .any(|d| d.message.contains("not a statically verifiable function")));
    std::fs::write(&module, "export function other() { return 1; }").unwrap();
    assert!(compile_to_js(source, path.to_str().unwrap())
        .unwrap_err()
        .iter()
        .any(|d| d.message.contains("export `f`")));
}
