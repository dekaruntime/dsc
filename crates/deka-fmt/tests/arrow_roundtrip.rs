//! `deka fmt` on arrow functions (rfd#67 part 2, dsc#252): arrows format
//! back as arrows, and a literal body with more than one statement is laid
//! out as a block — the previous single-line join wrote `/* stmt */` for an
//! `if` and joined statements with a space, which did not parse back.

fn format(source: &str) -> String {
    let arena = bumpalo::Bump::new();
    let parsed = deka_syntax::parse(source, &arena);
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    let once = deka_fmt::format_ds(source).unwrap();
    let arena = bumpalo::Bump::new();
    let reparsed = deka_syntax::parse(&once, &arena);
    assert!(reparsed.errors.is_empty(), "formatted output must parse:\n{once}\n{:?}", reparsed.errors);
    assert_eq!(once, deka_fmt::format_ds(&once).unwrap(), "not idempotent:\n{once}");
    once
}

#[test]
fn arrows_format_as_arrows() {
    let once = format(
        "const doubled=xs.map((x)=>x*2)\n\
         const strs=xs.map((x)=>{return string(x)})\n\
         const typed=xs.map((x:number)=>x+1)\n\
         const later=async ()=>{return 1}\n\
         const grouped=()=>(\n  1+2\n)\n\
         run(()=>{})\n",
    );
    assert!(once.contains("xs.map((x) => x * 2)"), "{once}");
    assert!(once.contains("xs.map((x) => { return string(x) })"), "{once}");
    assert!(once.contains("xs.map((x: number) => x + 1)"), "{once}");
    assert!(once.contains("async () => { return 1 }"), "{once}");
    assert!(once.contains("() => (1 + 2)"), "{once}");
    assert!(once.contains("run(() => {})"), "{once}");
    assert!(!once.contains("fn("), "arrows must not be rewritten as fn literals:\n{once}");
}

#[test]
fn multi_statement_literal_bodies_become_blocks() {
    let once = format(
        "useEffect(() => { setA(1); setB(2) })\n\
         run(fn() {\n  if (n > 0) {\n    n = 1\n  }\n  n = 2\n})\n\
         fn outer() void {\n  run(() => {\n    step()\n    run(() => {\n      deeper()\n      n = 3\n    })\n  })\n}\n",
    );
    assert_eq!(
        once,
        "useEffect(() => {\n  setA(1)\n  setB(2)\n})\n\
         run(fn() {\n  if (n > 0) {\n    n = 1\n  }\n  n = 2\n})\n\
         fn outer() void {\n  run(() => {\n    step()\n    run(() => {\n      deeper()\n      n = 3\n    })\n  })\n}\n"
    );
    assert!(!once.contains("/* stmt */"), "{once}");
}

#[test]
fn multiline_jsx_expression_body_keeps_its_parens() {
    let once = format(
        "const items = xs.map((i) => (\n  <li>\n    {i}\n  </li>\n))\n",
    );
    // dsc#245's layout, same as `return (…)`: never collapsed to `(<li>`.
    assert_eq!(once, "const items = xs.map((i) => (\n  <li>\n    {i}\n  </li>\n))\n");
    let nested = format(
        "fn View() ReactNode {\n  const items = xs.map((i) => (\n    <li>\n      {i}\n    </li>\n  ))\n  return <ul>{items}</ul>\n}\n",
    );
    assert!(nested.contains("xs.map((i) => (\n    <li>"), "{nested}");
    assert!(nested.contains("</li>\n  ))\n"), "{nested}");
}
