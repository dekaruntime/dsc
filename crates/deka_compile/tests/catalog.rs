use deka_compile::{compile_to_js, compile_to_js_with_options, CompileOptions};

fn official(source: &str) -> Result<deka_compile::CompileResult, Vec<deka_syntax::Diagnostic>> {
    compile_to_js_with_options(
        source,
        "virtual.ds",
        CompileOptions {
            package_name: Some("@deka/catalog-test".into()),
            ..Default::default()
        },
    )
}

#[test]
fn diagnostic_fixtures_keep_original_spans() {
    for (source, expected) in [
        (
            include_str!("fixtures/catalog/unknown.ds"),
            "unknown catalog helper",
        ),
        (include_str!("fixtures/catalog/arity.ds"), "argument"),
        (
            include_str!("fixtures/catalog/unsafe_in_safe.ds"),
            "requires `unsafe",
        ),
    ] {
        let errors = official(source).unwrap_err();
        assert!(
            errors.iter().any(|e| e.message.contains(expected)),
            "{errors:?}"
        );
        assert_eq!(errors[0].line, 1);
        assert_eq!(errors[0].column, 22);
        assert!(errors[0].underline_length > 4);
    }
}

#[test]
fn user_packages_and_unknown_names_are_rejected() {
    for source in [
        "const x = safe { deka.time.now() }",
        "const x = unsafe { deka.time.now() }",
    ] {
        assert!(compile_to_js(source, "user.ds")
            .unwrap_err()
            .iter()
            .any(|e| e.message.contains("stdlib-only")));
    }
    for source in [
        "const x = safe { deka.unknown.nope() }",
        "const x = unsafe { deka.unknown.nope() }",
        "const x = deka.time.now()",
        "const x = deka.bytes.len",
        "const x = unsafe { const d = deka; d.time.now() }",
        "const x = unsafe { deka['time']['now']() }",
        "const x = unsafe { deka.time.now(...[]) }",
        "const x = unsafe { `${deka.time.missing()}` }",
        "const x = `a ${deka.time.now()}`",
        "const x = safe { deka.time.now(deka.time.now()) }",
        "fn f(x: number = deka.time.now()) {}",
    ] {
        assert!(official(source).is_err(), "accepted {source}");
    }
}

#[test]
fn unsafe_checks_nested_calls_and_preserves_unicode_spans() {
    let errors =
        official("// café ☕\nconst x = unsafe { /*comment*/ deka.json.parse() }").unwrap_err();
    assert_eq!(errors[0].line, 2);
    assert_eq!(errors[0].column, 32);
    assert!(
        official("const x = unsafe { const x = deka.time.now(); deka.json.missing(x) }").is_err()
    );
    assert!(official("const x = unsafe { /deka.nope\\(\\)/.test('deka.bad()') }").is_ok());
}

#[test]
fn emitted_helpers_execute_without_installation_or_globals() {
    let output = official(include_str!("fixtures/catalog/valid.ds")).unwrap();
    assert!(output.js.contains("const __dsc_catalog"));
    assert!(!output.js.contains("deka.json."));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.mjs");
    std::fs::write(&path, output.js).unwrap();
    let script = r#"
      import assert from 'node:assert/strict';
      const before = Reflect.ownKeys(globalThis);
      const m = await import(process.argv[1]);
      assert.equal(m.valid, true);
      assert.equal(m.decoded.ok, true);
      assert.equal(m.absent, undefined);
      assert.deepEqual(Reflect.ownKeys(globalThis), before);
      assert.equal(globalThis.deka, undefined);
      assert.equal(globalThis.__dsc_catalog, undefined);
    "#;
    let result = std::process::Command::new("node")
        .args(["--input-type=module", "-e", script])
        .arg(path)
        .output()
        .expect("node is required for emitted-output verification");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn nearest_manifest_controls_disk_sources() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("deka.json"), r#"{"name":"@deka/test"}"#).unwrap();
    let path = root.path().join("test.ds");
    let source = "const x = safe { deka.time.now() }";
    std::fs::write(&path, source).unwrap();
    assert!(compile_to_js(source, path.to_str().unwrap()).is_ok());
    let child = root.path().join("ds_modules/user");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("deka.json"), r#"{"name":"@user/test"}"#).unwrap();
    let path = child.join("test.ds");
    std::fs::write(&path, source).unwrap();
    assert!(compile_to_js(source, path.to_str().unwrap()).is_err());
    std::fs::remove_file(child.join("deka.json")).unwrap();
    assert!(compile_to_js(source, path.to_str().unwrap()).is_err());
    assert!(compile_to_js_with_options(
        source,
        path.to_str().unwrap(),
        CompileOptions {
            package_name: Some("@deka/forged-virtual-identity".into()),
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn escaped_identifiers_do_not_bypass_the_gate() {
    for source in [
        r"const x = unsafe { d\u0065ka.time.now() }",
        r"const x = unsafe { d\u0065ka.time.missing() }",
    ] {
        assert!(compile_to_js(source, "user.ds").is_err());
    }
    let output = official(r"const x = unsafe { d\u0065ka /* gap */ .time.now() }").unwrap();
    assert!(output.js.contains("__dsc_catalog /* gap */ .time.now()"));
}

#[test]
fn catalog_walks_unwrap_alternatives_and_bridge_arguments() {
    for source in [
        "const x = unwrap(Some(1)) or { deka.time.now() }",
        "const x = bridge fs.read_file(deka.time.now())",
        "struct S { x: number = deka.time.now() }",
    ] {
        let errors = official(source).unwrap_err();
        assert!(
            errors.iter().any(|e| e.message.contains("catalog")),
            "{source}: {errors:?}"
        );
    }
}

#[test]
fn graph_and_build_entries_bundle_helpers() {
    use deka_compile::module_graph::{compile_module_graph, FsModuleLoader};
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("deka.json"), r#"{"name":"@deka/test"}"#).unwrap();
    let path = root.path().join("main.ds");
    std::fs::write(&path, include_str!("fixtures/catalog/valid.ds")).unwrap();
    let loader = FsModuleLoader::new(root.path().to_owned());
    let graph = compile_module_graph(&path, &loader).unwrap();
    assert!(graph.prelude.contains("const Option"));
    let modules = graph.self_contained_modules();
    assert!(modules[&path.canonicalize().unwrap()].contains("const __dsc_catalog"));
    let emitted = root.path().join("main.mjs");
    std::fs::write(&emitted, &modules[&path.canonicalize().unwrap()]).unwrap();
    let result = std::process::Command::new("node")
        .arg(&emitted)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let output =
        official("const value: boolean = build { return Ok(safe { deka.json.validate(\"{}\") }) }")
            .unwrap();
    assert_eq!(output.dev_plan.slots.len(), 1);
    let entry = &output.dev_plan.slots[0].entry;
    assert!(entry.contains("const __dsc_catalog"));
    assert!(!entry.contains("deka.json.validate"));
}

#[test]
fn safe_types_and_formatting_are_preserved() {
    assert!(official("const x: string = safe { deka.time.now() }").is_err());
    assert!(official("const x = safe { deka.json.validate(42) }").is_err());
    assert!(official("const x: boolean = safe {\n deka.json.validate(\"{}\")\n }").is_ok());
}

#[test]
fn raw_span_points_past_comments_to_the_call() {
    let source = "const x = unsafe { /* deka.time.missing() */ deka.time.missing() }";
    let errors = official(source).unwrap_err();
    assert_eq!(errors[0].column, source.rfind("deka").unwrap() + 1);
    assert!(errors[0].message.contains("unknown catalog helper"));
}
