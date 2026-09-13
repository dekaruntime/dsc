use deka_compile::{compile_to_js, compile_to_js_with_options, CompileOptions};

fn dev(source: &str, file: &str) -> String {
    compile_to_js_with_options(
        source,
        file,
        CompileOptions {
            dev: true,
            ..Default::default()
        },
    )
    .unwrap()
    .js
}

const HEADER: &str =
    "\"use strict\";\nimport { jsxDEV, Fragment } from \"@js/react/jsx-dev-runtime\";\n\n";

#[test]
fn known_ds_source_position_and_single_child() {
    assert_eq!(dev(include_str!("fixtures/jsx_dev/known.dsx"), "fixtures/known.dsx"), format!(
        "{HEADER}export function Card() {{\nreturn jsxDEV(\"p\", {{\"children\": \"hello\"}}, undefined, false, {{fileName: \"fixtures/known.dsx\", lineNumber: 3, columnNumber: 10}}, this);\n}}"
    ));
}

#[test]
fn key_is_third_argument_and_multiple_children_are_static() {
    assert_eq!(dev("const view = <p key={7}>{1}{2}</p>;", "multi.dsx"), format!(
        "{HEADER}const view = jsxDEV(\"p\", {{\"children\": [1, 2]}}, 7, true, {{fileName: \"multi.dsx\", lineNumber: 1, columnNumber: 14}}, this);"
    ));
}

#[test]
fn nested_fragment_and_empty_element_have_individual_spans() {
    assert_eq!(dev("const view = <><p />{2}</>;", "nested.dsx"), format!(
        "{HEADER}const view = jsxDEV(Fragment, {{\"children\": [jsxDEV(\"p\", {{}}, undefined, false, {{fileName: \"nested.dsx\", lineNumber: 1, columnNumber: 16}}, this), 2]}}, undefined, true, {{fileName: \"nested.dsx\", lineNumber: 1, columnNumber: 14}}, this);"
    ));
}

#[test]
fn empty_and_single_child_fragments() {
    for (source, props) in [
        ("const view = <></>;", ""),
        ("const view = <>{1}</>;", "\"children\": 1"),
    ] {
        assert_eq!(dev(source, "fragment.dsx"), format!(
            "{HEADER}const view = jsxDEV(Fragment, {{{props}}}, undefined, false, {{fileName: \"fragment.dsx\", lineNumber: 1, columnNumber: 14}}, this);"
        ));
    }
}

#[test]
fn dev_runtime_resolution_and_production_default() {
    let source = "const view = <p />;";
    for (runtime, expected) in [
        ("react/jsx-runtime", "react/jsx-dev-runtime"),
        ("@js/custom/runtime", "@js/custom/jsx-dev-runtime"),
        ("./runtime.mjs", "./jsx-dev-runtime"),
        ("runtime", "jsx-dev-runtime"),
    ] {
        let js = compile_to_js_with_options(
            source,
            "runtime.dsx",
            CompileOptions {
                dev: true,
                jsx_runtime: Some(runtime.into()),
                ..Default::default()
            },
        )
        .unwrap()
        .js;
        assert!(js.contains(&format!("from \"{expected}\";")), "{js}");
    }
    let options = CompileOptions {
        jsx_dev_runtime: Some("./dev.mjs".into()),
        ..Default::default()
    };
    assert_eq!(
        compile_to_js_with_options(source, "runtime.dsx", options.clone())
            .unwrap()
            .js,
        compile_to_js(source, "runtime.dsx").unwrap().js
    );
    let js = compile_to_js_with_options(
        source,
        "runtime.dsx",
        CompileOptions {
            dev: true,
            ..options
        },
    )
    .unwrap()
    .js;
    assert!(js.contains("from \"./dev.mjs\";"));
    assert!(compile_to_js_with_options(
        source,
        "runtime.dsx",
        CompileOptions {
            dev: true,
            jsx_dev_runtime: Some(" ".into()),
            ..Default::default()
        }
    )
    .unwrap_err()[0]
        .message
        .contains("jsxDevRuntime"));
}

#[test]
fn nearest_manifest_and_explicit_override() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("src");
    std::fs::create_dir(&nested).unwrap();
    let path = nested.join("card.dsx");
    let file = path.to_str().unwrap();
    let manifest = dir.path().join("deka.json");
    for (config, expected) in [
        (
            r#"{"jsxRuntime":"react/jsx-runtime"}"#,
            "react/jsx-dev-runtime",
        ),
        (
            r#"{"jsxRuntime":"react/jsx-runtime","jsxDevRuntime":"./dev.mjs"}"#,
            "./dev.mjs",
        ),
    ] {
        std::fs::write(&manifest, config).unwrap();
        assert!(dev("const view = <p />;", file).contains(&format!("from \"{expected}\";")));
        let js = compile_to_js_with_options(
            "const view = <p />;",
            file,
            CompileOptions {
                dev: true,
                jsx_dev_runtime: Some("override".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .js;
        assert!(js.contains("from \"override\";"));
    }
    std::fs::write(&manifest, r#"{"jsxDevRuntime":false}"#).unwrap();
    assert!(compile_to_js_with_options(
        "const view = <p />;",
        file,
        CompileOptions {
            dev: true,
            ..Default::default()
        }
    )
    .unwrap_err()[0]
        .message
        .contains("jsxDevRuntime"));
}

#[test]
fn helper_names_do_not_capture_bindings_and_filename_is_escaped() {
    let js = dev(
        "const jsxDEV = 1; const __deka_jsxDEV = 2; const view = <p />;",
        "quoted\"file.dsx",
    );
    assert!(js.contains("jsxDEV as __deka_jsxDEV_"), "{js}");
    assert!(js.contains("const view = __deka_jsxDEV_("), "{js}");
    assert!(js.contains(r#"fileName: "quoted\"file.dsx""#), "{js}");
}
