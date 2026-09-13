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
    // Transitive coloring: `add` becomes hook-typed because it calls a hook
    // function. The diagnostic fires at the plain (module-level) call site.
    assert_teaches(
        "fn useCounter() [number, Setter<number>] { return useState(0) }\n\
         fn add(a: number, b: number) number {\n\
           const [count, setCount] = useCounter()\n\
           return a + b\n\
         }\n\
         add(1, 2)",
        "from a plain function",
    );
}

#[test]
fn multi_hop_custom_hook_ok() {
    // Forward-ref: useOuter is declared before useInner. Color is a
    // fixed-point on the function type, not a one-hop name table.
    assert_ok(
        "fn useOuter() [number, Setter<number>] {\n\
           return useInner()\n\
         }\n\
         fn useInner() [number, Setter<number>] {\n\
           return useState(0)\n\
         }\n\
         fn Counter() ReactNode {\n\
           const [count, setCount] = useOuter()\n\
           return <p>{string(count)}</p>\n\
         }",
    );
}

#[test]
fn alias_call_in_plain_fn_rejected() {
    // Rejected at check, so emission never runs (no missing-import crash).
    assert_teaches(
        "const f = useState\n\
         f(0)",
        "from a plain function",
    );
}

#[test]
fn alias_call_in_component_ok() {
    assert_ok(
        "fn Counter() ReactNode {\n\
           const f = useState\n\
           const [count, setCount] = f(0)\n\
           return <p>{string(count)}</p>\n\
         }",
    );
}

#[test]
fn closure_into_plain_hof_rejected() {
    assert_teaches(
        "fn apply(f: fn() number) number { return f() }\n\
         fn Counter() ReactNode {\n\
           const n = apply(fn() number { const [c, s] = useState(0); return c })\n\
           return <p>{string(n)}</p>\n\
         }",
        "this closure calls a hook; hooks run only during render — accept a hook-typed parameter or lift the hook to the component",
    );
}

#[test]
fn colored_fn_by_value_into_hook_typed_param_ok() {
    assert_ok(
        "fn run(f: Hook<fn() number>) number { return f() }\n\
         fn Counter() ReactNode {\n\
           const n = run(fn() number { const [c, s] = useState(0); return c })\n\
           return <p>{string(n)}</p>\n\
         }",
    );
}

#[test]
fn cannot_shadow_compiler_known_hook() {
    assert_teaches(
        "fn Counter() ReactNode {\n\
           const useState = fn(n: number) number { return n }\n\
           return <p>{string(useState(0))}</p>\n\
         }",
        "cannot shadow compiler-known hook `useState`",
    );
}

#[test]
fn name_shadowing_local_is_not_a_hook_call() {
    // A local binding that shares a custom-hook name is a plain value; a
    // conditional call must not trip the straight-line rule. Name-table
    // coloring false-positived this as a hook call.
    assert_ok(
        "fn useCounter() [number, Setter<number>] {\n\
           return useState(0)\n\
         }\n\
         fn Counter() ReactNode {\n\
           const [count, setCount] = useState(0)\n\
           if (count > 0) {\n\
             const useCounter = fn() number { return 1 }\n\
             const n = useCounter()\n\
             setCount(n)\n\
           }\n\
           return <p>{string(count)}</p>\n\
         }",
    );
}

#[test]
fn use_prefix_without_hook_call_is_plain() {
    // Coloring is the type, not the name: a `use*` function that never
    // calls a hook-typed function is an ordinary function.
    assert_ok(
        "fn useHelper(n: number) number { return n }\n\
         fn main() number { return useHelper(1) }",
    );
}

#[test]
fn usestate_none_requires_option_annotation() {
    // Pick: require an explicit Option annotation. `None` is the empty
    // Option payload; inferring `T = none` would be a silent nonsense type.
    assert_teaches(
        "fn Counter() ReactNode {\n\
           const [count, setCount] = useState(None)\n\
           return <p />\n\
         }",
        "explicit Option",
    );
    assert_ok(
        "alias NumOpt = Option<number>\n\
         fn Counter() ReactNode {\n\
           const [count, setCount] = useState<NumOpt>(None)\n\
           return <p />\n\
         }",
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
