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

#[test]
fn tuples_context_and_consumption() {
    for source in [
        "const pair: [number, string] = [1, \"ok\"]; const [n, s] = pair; const a: number = n; const b: string = s;",
        "fn pair() [number, string] { return [1, \"ok\"]; } const [a, b] = pair();",
        "const pair: [[number, string], boolean] = [[1, \"x\"], true]; const [inner, flag] = pair; const [n, s] = inner; const x: string = pair[0][1];",
        "struct S { pair: [number, string] } const s = S { pair: [1, \"ok\"] }; const [n, text] = s.pair;",
        "fn take(p: [number, string]) string { return p[1]; } const s = take([1, \"ok\"]);",
        "const p: [number, Option<string>] = [1, None]; const q: Option<[number, string]> = Some([1, \"ok\"]);",
        "let p: [number, string] = [1, \"a\"]; p[0] = 2; p[1] = \"b\";",
        "const empty: [] = []; const [] = empty; const singleton: [number] = [1]; const [n] = singleton;",
        "const [n, s]: [number, string] = [1, \"ok\"];",
        "alias Pair = [number, string]; fn f(p: Pair = [1, \"a\"]) Pair { return p; } const [n, s] = f();",
        "fn id<T>(p: [T, string]) [T, string] { return p; } const p: [number, string] = [1, \"a\"]; const q: [number, string] = id(p);",
        "summon { total fn pair() [number, string], total fn consume(p: [number, string]) void, } from \"./shim.mjs\"; const [n, s] = pair(); consume([n, s]);",
    ] {
        assert!(errors(source).is_empty(), "{source}\n{:?}", errors(source));
    }
}

#[test]
fn tuples_rejections_teach_the_fact() {
    for (source, diagnostic) in [
        (
            "const p: [number, string] = [1];",
            "arity must match exactly",
        ),
        (
            "const p: [number, string] = [1, 2];",
            "position 1 expects `string`",
        ),
        (
            "const p: [number, string] = [1, \"a\"]; const [n] = p;",
            "bind every position exactly once",
        ),
        (
            "const p: [number, string] = [1, \"a\"]; const [n, s, extra] = p;",
            "bind every position exactly once",
        ),
        (
            "const p: [number, string] = [1, \"a\"]; const n = p[2];",
            "out of range",
        ),
        (
            "const p: [number, string] = [1, \"a\"]; const n = p[0.5];",
            "out of range",
        ),
        (
            "const p: [number, string] = [1, \"a\"]; const i = 0; const n = p[i];",
            "use destructuring",
        ),
        (
            "let p: [number, string] = [1, \"a\"]; p[0] = \"bad\";",
            "cannot assign type `string` to `number`",
        ),
        (
            "const p: [number, string] = [1, \"a\"]; p[0] = 2;",
            "immutable",
        ),
        (
            "const xs = [1, 2]; const n = xs[0];",
            "not proven in bounds",
        ),
        ("const xs = [1, \"a\"];", "mixed element types"),
        ("const [x, y] = [1, 2];", "destructuring requires a tuple"),
    ] {
        let found = errors(source);
        assert!(
            found.iter().any(|e| e.contains(diagnostic)),
            "{source}\nexpected {diagnostic:?}, got {found:?}"
        );
    }
}

#[test]
fn tuples_nested_contexts_and_intrinsic_proofs() {
    for source in [
        "fn id<T>(p: [T, string]) [T, string] { return p; } const p: [number, string] = id([1, \"ok\"]);",
        "const [v]: [Option<number>] = [None]; const x = isset(v);",
        "const xs: Array<[number, string]> = [[1, \"a\"], [2, \"b\"]]; if (xs.has(0)) { const [n, s] = xs[0]; }",
        "fn pair() Exception<[number, string], string> { return Ok([1, \"a\"]); } fn consume() [number, string] { return match pair() { Ok(p) => p, Throw(e) => [0, e] }; } const [n, s] = consume();",
        "fn touch() void {} let p: [number, string] = [1, \"a\"]; touch(); p[0] = 2; const s: string = p[(1)];",
        "struct Box<T> { pair: [T, string] } const b = Box { pair: [1, \"a\"] }; const n: number = b.pair[0];",
    ] { assert!(errors(source).is_empty(), "{source}\n{:?}", errors(source)); }
}
