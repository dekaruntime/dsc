use deka_native_compile::compile;
use deka_native_ir::{Edges, Length, Number, Text};
const COUNTER: &str = include_str!("../examples/counter.dsx");
#[test]
fn source_produces_state_layout_and_executable_handler() {
    let p = compile(COUNTER).unwrap();
    assert_eq!(p.states[0].initial, 0.);
    assert_eq!(p.root.style.padding, Edges::all(24.));
    assert_eq!(p.root.style.width, Length::Px(384.));
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

#[test]
fn inline_text_keeps_spaces_and_can_override_its_flow() {
    let source = "export fn App() ReactNode { const [n, setN] = useState(0); return (<span>Count: {string(n)}</span>); }";
    let p = compile(source).unwrap();
    assert!(p.root.style.row);
    assert_eq!(
        p.root.children[0].text,
        Some(Text::Literal("Count: ".into()))
    );
    assert!(
        !compile(&source.replace("<span>", "<span className=\"flex flex-col\">"))
            .unwrap()
            .root
            .style
            .row
    );
    assert!(
        compile(&source.replace("<span>", "<span className=\"flex\">"))
            .unwrap()
            .root
            .style
            .row
    );
    let compact = compile(&source.replace("Count: ", "Count:")).unwrap();
    assert_eq!(
        compact.root.children[0].text,
        Some(Text::Literal("Count:".into()))
    );
}

#[test]
fn box_contract_parses_and_rejects_invalid_dimensions() {
    use deka_native_ir::{Align, Justify};
    let source = r#"export fn App() ReactNode { return (<div className="w-full h-64 min-w-12 max-w-96 flex-row flex-wrap items-center justify-between p-4 px-2 pt-1 m-2 gap-x-3 gap-y-1 overflow-hidden whitespace-nowrap"><span className="w-1/2 grow shrink-0 self-end">Hello</span></div>); }"#;
    let program = compile(source).unwrap();
    let s = &program.root.style;
    assert_eq!(s.width, Length::Percent(1.));
    assert_eq!(s.height, Length::Px(256.));
    assert_eq!(
        s.padding,
        Edges {
            top: 4.,
            right: 8.,
            bottom: 16.,
            left: 8.
        }
    );
    assert_eq!(s.align, Align::Center);
    assert_eq!(s.justify, Justify::Between);
    assert!(s.wrap && s.clip);
    assert_eq!(s.nowrap, Some(true));
    assert_eq!(program.root.children[0].style.width, Length::Percent(0.5));
    assert_eq!(program.root.children[0].style.align_self, Some(Align::End));
    for bad in [
        "w-NaN",
        "w-inf",
        "w-1/0",
        "w-2/1",
        "w--2",
        "overflow-scroll",
        "items-baseline",
        "p-auto",
    ] {
        assert!(
            compile(&source.replace("w-full", bad)).is_err(),
            "accepted {bad}"
        );
    }
    let decoded = serde_json::from_str(&serde_json::to_string(&program).unwrap()).unwrap();
    assert_eq!(program, decoded);
}

#[test]
fn conditional_animation_targets_are_checked_and_emitted() {
    let source = r#"export fn App() ReactNode {
        const [open, setOpen] = useState(0);
        return (<button className={open == 1 ? "w-48 opacity-100 translate-x-8 transition-all duration-400 ease-linear" : "w-24 opacity-0 -translate-x-8 transition-all duration-400 ease-linear"}
          onClick={fn() { setOpen(1 - open); }}>Toggle</button>);
    }"#;
    let program = compile(source).unwrap();
    let choice = program.root.style_when.as_ref().unwrap();
    assert_eq!(
        choice.condition,
        deka_native_ir::Condition::Equal(Number::State(0), Number::Literal(1.))
    );
    assert_eq!(choice.then_style.opacity, 1.);
    assert_eq!(choice.else_style.opacity, 0.);
    assert_eq!(choice.else_style.translate_x, -32.);
    assert_eq!(choice.then_style.transition, 15);
    assert_eq!(choice.then_style.duration_ms, 400.);
    let rust = deka_native_compile::emit_rust(&program);
    assert!(rust.contains("if (state[0]) == (1.0)"));
    for invalid in [
        "opacity-101",
        "duration-NaN",
        "translate-x-inf",
        "transition-magic",
    ] {
        assert!(compile(&source.replace("opacity-100", invalid)).is_err());
    }
    let decoded = serde_json::from_str(&serde_json::to_string(&program).unwrap()).unwrap();
    assert_eq!(program, decoded);
}
#[test]
fn presence_and_motion_contract_are_checked() {
    let source = r#"export fn App() ReactNode {
        const [open, setOpen] = useState(1);
        return (<div className="stagger-100">
            {open == 1 ? <button className="enter-slide exit-fade spring scale-110 rotate-15 transition-layout frames-x-[0:0,50:20,100:0] repeat-2 alternate" onClick={fn() {setOpen(0);}}>Hello</button> : None}
        </div>);
    }"#;
    let p = compile(source).unwrap();
    let child = p
        .root
        .children
        .iter()
        .find(|c| c.visible_when.is_some())
        .unwrap();
    assert_eq!(child.style.motion.enter, 2);
    assert_eq!(child.style.motion.frames.len(), 3);
    assert!(child.style.motion.layout);
    let decoded = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    assert_eq!(p, decoded);
    for invalid in [
        "spring-damping-0",
        "frames-x-[0:0,0:1,100:0]",
        "repeat-1.5",
        "scale-0",
        "-animate-spin",
    ] {
        assert!(
            compile(&source.replace("stagger-100", invalid)).is_err(),
            "accepted {invalid}"
        );
    }
}

#[test]
fn local_state_assignments_and_direct_bindings_need_no_react() {
    let source = include_str!("../examples/bindings.dsx");
    let p = compile(source).unwrap();
    assert_eq!(p.component, "Counter");
    assert_eq!(p.states.len(), 2);
    assert_eq!(p.root.style.width, Length::Percent(1.));
    assert_eq!(p.handlers[0].state, 0);
    assert_eq!(
        p.handlers[0].value,
        Number::Add(Box::new(Number::State(0)), Box::new(Number::Literal(1.)))
    );
    assert_eq!(p.handlers[2].state, 1);
    assert!(matches!(
        p.root.children[2].children[1].text,
        Some(Text::Number(Number::State(0)))
    ));
    for bad in [
        source.replace("let count", "const count"),
        source.replace("count +=", "missing +="),
        source.replace("count += 1", "count /= 2"),
        source.replace("let count = 0", "let count = \"hello\""),
        source
            .replace("<div className", "<view className")
            .replace("</div>", "</view>"),
    ] {
        assert!(compile(&bad).is_err(), "accepted {bad}");
    }
}
