use bumpalo::Bump;

fn errors(source: &str) -> Vec<String> {
    let arena = Bump::new();
    let parsed = crate::parse(source, &arena);
    assert!(
        parsed.errors.is_empty(),
        "parse: {:?}\n{source}",
        parsed.errors
    );
    super::check_program(&parsed.program.unwrap(), source)
        .errors
        .into_iter()
        .map(|e| e.message)
        .collect()
}

fn assert_ok(source: &str) {
    let got = errors(source);
    assert!(got.is_empty(), "{source}\n{got:?}");
}

fn assert_teaches(source: &str, needle: &str) {
    let got = errors(source);
    assert!(
        got.iter().any(|m| m.contains(needle)),
        "{source}\nexpected a diagnostic containing `{needle}`\ngot: {got:?}"
    );
}

#[test]
fn component_to_hook_fn_to_hook_ok() {
    assert_ok(
        "fn useCounter() [number, Setter<number>] {\n\
           return useState(0)\n\
         }\n\
         fn Counter() ReactNode {\n\
           const [count, setCount] = useCounter()\n\
           return <p>{string(count)}</p>\n\
         }",
    );
}

#[test]
fn custom_hook_needs_no_use_prefix() {
    assert_ok(
        "fn counter() [number, Setter<number>] {\n\
           return useState(0)\n\
         }\n\
         fn Counter() ReactNode {\n\
           const [count, setCount] = counter()\n\
           return <p>{string(count)}</p>\n\
         }",
    );
}

#[test]
fn plain_function_calling_hook_rejected() {
    assert_teaches(
        "const [count, setCount] = useState(0)",
        "from a plain function",
    );
}

#[test]
fn hook_fn_called_from_plain_fn_rejected() {
    assert_teaches(
        "fn useCounter() [number, Setter<number>] { return useState(0) }\n\
         fn add(a: number, b: number) number {\n\
           const [count, setCount] = useCounter()\n\
           return a + b\n\
         }",
        "from a plain function",
    );
}

#[test]
fn straight_line_if() {
    assert_teaches(
        "fn Counter() ReactNode {\n\
           if (true) { const [count, setCount] = useState(0) }\n\
           return <p />\n\
         }",
        "hooks run in a fixed order every render; move the condition inside the hook",
    );
}

#[test]
fn straight_line_loop() {
    assert_teaches(
        "fn Counter() ReactNode {\n\
           for (const x of [1]) { const [count, setCount] = useState(0) }\n\
           return <p />\n\
         }",
        "hooks run in a fixed order every render; move the condition inside the hook",
    );
}

#[test]
fn straight_line_after_early_return() {
    assert_teaches(
        "fn Counter() ReactNode {\n\
           if (true) { return <p /> }\n\
           const [count, setCount] = useState(0)\n\
           return <p>{string(count)}</p>\n\
         }",
        "hooks run in a fixed order every render; move the condition inside the hook",
    );
}

#[test]
fn setter_dual_call_shapes() {
    assert_ok(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           setCount(1)\n\
           setCount(fn(c: number) number { return c + 1 })\n\
           return <p>{string(count)}</p>\n\
         }",
    );
    assert_teaches(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           setCount(\"no\")\n\
           return <p />\n\
         }",
        "fn(number) number",
    );
}

#[test]
fn ref_current_is_mutable_on_const() {
    assert_ok(
        "fn Counter() ReactNode {\n\
           const r = useRef(0)\n\
           r.current = 1\n\
           const n: number = r.current\n\
           return <p>{string(n)}</p>\n\
         }",
    );
    assert_teaches(
        "fn Counter() ReactNode {\n\
           const r = useRef(0)\n\
           r.current = \"no\"\n\
           return <p />\n\
         }",
        "cannot assign type `string` to `number`",
    );
}

#[test]
fn usestate_infers_and_checks_type_argument() {
    assert_ok(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState<number>(0)\n\
           return <p>{string(count)}</p>\n\
         }",
    );
    assert_teaches(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState<number>(\"no\")\n\
           return <p />\n\
         }",
        "expected type `number`",
    );
}

#[test]
fn hook_before_return_is_in_order() {
    assert_ok(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           return <p>{string(count)}</p>\n\
         }",
    );
}
