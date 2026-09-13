#[test]
fn tuples_format_idempotently() {
    let source = r#"
alias Pair=[number,fn(number) void];
fn make() [number,[string,Option<number>]] {return [1,["a",None]];}
const [n,pair]=make();
const [text,maybe]=pair;
const singleton:[number]=[1];
const empty:[]=[];
const []=empty;
summon {total fn foreign(value:[number,string]): [number,string],} from "./shim.mjs";
"#;
    let arena = bumpalo::Bump::new();
    let parsed = deka_syntax::parse(source, &arena);
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    let once = deka_fmt::format_ds(source).unwrap();
    assert!(once.contains("[number, fn(number) void]"), "{once}");
    assert!(once.contains("const [n, pair] = make()"), "{once}");
    assert_eq!(once, deka_fmt::format_ds(&once).unwrap());
}
