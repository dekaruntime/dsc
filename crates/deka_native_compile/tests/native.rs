use deka_native_compile::compile;
use deka_native_ir::{Number, Text};
const COUNTER: &str = include_str!("../examples/counter.dsx");
#[test]
fn source_produces_state_layout_and_executable_handler() {
    let p = compile(COUNTER).unwrap();
    assert_eq!(p.states[0].initial, 0.);
    assert_eq!(p.root.style.padding, 24.);
    assert_eq!(p.root.style.width, Some(384.));
    assert_eq!(
        p.handlers[0].value,
        Number::Add(Box::new(Number::State(0)), Box::new(Number::Literal(1.)))
    );
    assert!(matches!(
        &p.root.children[2].children[1].text,
        Some(Text::Number(Number::State(0)))
    ));
}
#[test]
fn unsupported_and_ill_typed_source_is_rejected() {
    for source in [
        COUNTER.replace("p-6", "position-magic"),
        COUNTER.replace("count + 1", "missing + 1"),
        COUNTER.replace("useState(0)", "useState(\"text\")"),
        COUNTER.replace("<span", "<input"),
        COUNTER.replace("count + 1", "count / 2"),
    ] {
        assert!(compile(&source).is_err(), "accepted: {source}");
    }
}
#[test]
fn wire_format_roundtrips() {
    let p = compile(COUNTER).unwrap();
    let decoded = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    assert_eq!(p, decoded);
}
