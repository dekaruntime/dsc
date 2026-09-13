//! DekaScript JavaScript emitter (Compiler v2).
//!
//! Emits reasonably formatted JavaScript from the v2 AST, erasing type
//! annotations. Structs become `deka.Struct` factories, enums become frozen
//! case objects, and receiver methods are registered on the factory prototype
//! so instance method calls work without a separate lowering pass.

mod emit;
pub mod prelude;
mod util;

pub use emit::{
    build_factory_names, collect_js_identifier_tokens, dev_slot_id,
    dev_slot_source_path, dev_uses_name, emit_dev_entry, emit_js, emit_js_module_with_options,
    emit_js_with_imports, emit_js_with_options, live_dev_uses_name, react_module_specifier,
    ModuleEmit,
};

#[cfg(test)]
mod tests {
    use super::*;
    use bumpalo::Bump;
    use deka_syntax::parse;

    fn parse_and_emit(source: &str) -> String {
        let arena = Bump::new();
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        emit_js(&program, source).expect("emit failed")
    }

    /// Union type-patterns are lowered by the typechecker, so emission of
    /// them needs the checker results — unlike the erase-only `parse_and_emit`.
    fn parse_check_and_emit(source: &str) -> String {
        let arena = Bump::new();
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = deka_syntax::typeck::check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        emit_js_with_options(
            &program,
            source,
            &std::collections::HashMap::new(),
            None,
            &typeck.exception_forms,
            &typeck.unwrap_calls,
            &typeck.operator_rewrites,
            &typeck.method_calls,
            &typeck.type_of_calls,
            &typeck.signature_calls,
            &typeck.json_calls,
            &typeck.array_builtin_calls,
            &typeck.number_math_calls,
            &typeck.static_type_calls,
            &typeck.super_trees,
            &typeck.enum_case_patterns,
            &typeck.union_type_patterns,
            &std::collections::HashSet::new(),
            "module.ds",
            None,
        )
        .expect("emit failed")
    }

    #[test]
    fn tuples_descriptors_and_json_node() {
        let out = parse_check_and_emit(
            r#"
const pair: [number, Option<string>] = [7, None];
const shape = pair.signature();
const encoded = pair.toJSON();
const decoded = encoded.parseJSON<[number, Option<string>]>();
const bad = "[1]".parseJSON<[number, string]>();
const wrong = "[1,2]".parseJSON<[number, string]>();
super struct Container { pair: [number, string] }
const descriptor = Container.type();
"#,
        );
        let js = format!(
            "{out}\n{}",
            r#"
import assert from 'node:assert/strict';
assert.equal(shape.kind, 'tuple');
assert.equal(shape.elements.length, 2);
assert.equal(shape.elements[1].kind, 'option');
assert.deepEqual(decoded, {ok: true, value: [7, undefined]});
assert.equal(bad.ok, false);
assert.equal(wrong.ok, false);
assert.equal(descriptor.fields[0].type.kind, 'tuple');
"#
        );
        let result = std::process::Command::new("node")
            .args(["--input-type=module", "-e", &js])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{out}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn tuples_exact_output() {
        assert_eq!(parse_check_and_emit("const pair: [number, string] = [1, \"ok\"]; const [n, s] = pair; const value = pair[1];"),
            "\"use strict\";\nconst pair = [1, \"ok\"];\nconst [n, s] = pair;\nconst value = pair[1];");
    }

    #[test]
    fn tuples_summon_node() {
        let shim = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/tuples/shim.mjs")
            .canonicalize()
            .unwrap();
        let source = format!(
            r#"summon {{ total fn pair() [number, string], total fn empty() [number, string], total fn optional() Option<[number, string]>, total fn consume(p: [number, string]) string, }} from "{}";
const [n, s] = pair();
const joined = consume([n, s]);
fn guarded() [number, string] {{ return empty(); }}
const absent = optional();
const p: [number, Option<string>] = [1, None];
const q: Option<[number, string]> = Some([2, "x"]);
"#,
            shim.display()
        );
        let out = parse_check_and_emit(&source);
        let js = format!(
            "{out}\n{}",
            r#"
import assert from 'node:assert/strict';
assert.equal(n, 7);
assert.equal(s, 'seven');
assert.equal(joined, '7:seven');
assert.deepEqual(p, [1, undefined]);
assert.deepEqual(q, [2, 'x']);
assert.equal(absent, undefined);
assert.throws(() => guarded());
"#
        );
        let result = std::process::Command::new("node")
            .args(["--input-type=module", "-e", &js])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{out}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn option_erasure_runtime() { run_hats_fixtures("option_erasure", 1); }

    #[test]
    fn option_struct_defaults_hats_fixtures() {
        run_hats_fixtures("option_struct_defaults", 4);
    }

    #[test]
    fn option_struct_defaults_exact_output() {
        let out = parse_check_and_emit(r#"
interface Options { path?: string; secure?: boolean }
const omitted: Options = {};
const partial: Options = { path: Some("/app") };
const explicit: Options = { path: None, secure: None };
fn defaults(options: Options = {}) Options { return options; }
interface Request { options: Options }
const nested: Request = { options: {} };
"#);
        assert_eq!(out, concat!(
            "\"use strict\";\n\n",
            "function defaults(options = {}) {\nreturn options;\n}\n\n",
            "const omitted = {};\n",
            "const partial = {path: (\"/app\")};\n",
            "const explicit = {path: undefined, secure: undefined};\n",
            "const nested = {options: {}};",
        ));
        // Keep the authored fields in named struct factory arguments too.
        let omitted = parse_check_and_emit(
            "struct Value { item: Option<number> } const value = Value {};",
        );
        let explicit = parse_check_and_emit(
            "struct Value { item: Option<number> } const value = Value { item: None };",
        );
        assert_eq!(omitted.lines().last(), Some("const value = Value({  });"));
        assert_eq!(explicit.lines().last(), Some("const value = Value({ item: undefined });"));
    }

    #[test]
    fn option_erasure_exact_output() {
        assert_eq!(parse_check_and_emit("const a = Some(0); const b: Option<number> = None; const c = isset(a);"),
            "\"use strict\";\nconst a = (0);\nconst b = undefined;\nconst c = (a !== undefined);");
        let out = parse_check_and_emit("fn f(v: Option<number>) number { return match v { Some(x) => x, None => 0 }; }");
        assert_eq!(out, "\"use strict\";\nfunction f(v) {\nlet __deka_match_result_1;\nconst __deka_match_scrutinee_1 = v;\nif (__deka_match_scrutinee_1 !== undefined) {\n const x = __deka_match_scrutinee_1;\n __deka_match_result_1 = x;\n}\nelse {\n __deka_match_result_1 = 0;\n}\nreturn __deka_match_result_1;\n}");
    }

    #[test]
    fn indexing_has_emits_inline_and_access_stays_bare() {
        let out = parse_check_and_emit(
            "fn f(scores: Array<number>, round: number) number { return scores.has(round) ? scores[round] : 0; }",
        );
        assert!(out.contains("return Number.isInteger(round) && round >= 0 && round < scores.length ? scores[round] : 0;"), "{out}");
        for forbidden in ["globalThis", "=>", "Some", "__deka_index$"] {
            assert!(!out.contains(forbidden), "{out}");
        }
    }

    #[test]
    fn indexing_hats_fixtures() {
        run_hats_fixtures("indexing", 7);
    }

    #[test]
    fn ternary_emits_authored_conditional_expressions() {
        for expression in [
            "a || b ? 1 : 2",
            "a && b ? 1 : 2",
            "a ? b ? 1 : 2 : 3",
            "a ? 1 : b ? 2 : 3",
            "(a ? b : a) ? 1 : 2",
        ] {
            let source = format!("fn f(a: boolean, b: boolean) number {{ return {expression}; }}");
            let out = parse_check_and_emit(&source);
            assert!(out.contains(&format!("return {expression};")), "{out}");
            assert!(!out.contains("if ("), "{out}");
            assert!(!out.contains("=>"), "{out}");
        }
    }

    #[test]
    fn ternary_hats_fixtures() {
        run_hats_fixtures("ternary", 5);
    }

    fn run_hats_fixtures(group: &str, expected_count: usize) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../tests/fixtures/{group}"));
        let mut count = 0;
        for entry in std::fs::read_dir(root).unwrap() {
            let dir = entry.unwrap().path();
            let name = dir.file_name().unwrap().to_str().unwrap();
            let metadata: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap(),
            )
            .unwrap();
            let source_path = std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    matches!(
                        path.extension().and_then(|e| e.to_str()),
                        Some("ds" | "dsx")
                    )
                })
                .unwrap();
            let source = std::fs::read_to_string(source_path).unwrap();
            let arena = Bump::new();
            let parsed = parse(&source, &arena);
            assert!(parsed.errors.is_empty(), "{name}: {:?}", parsed.errors);
            let program = parsed.program.unwrap();
            let checked = deka_syntax::typeck::check_program(&program, &source);
            if let Some(diagnostic) = metadata["expectedDiagnosticContains"].as_str() {
                assert_eq!(checked.errors.len(), 1, "{name}: {:?}", checked.errors);
                assert!(
                    checked.errors[0].message.contains(diagnostic),
                    "{name}: {:?}",
                    checked.errors
                );
            } else {
                let out = parse_check_and_emit(&source);
                if group == "ternary" {
                    assert!(out.contains(" ? "), "{name}: {out}");
                }
                // JSX imports the host UI runtime. Check its emitted JS syntax;
                // execute the pure DS fixtures, including lazy-arm assertions.
                let mut command = std::process::Command::new("node");
                command.arg("--input-type=module");
                if name == "jsx" {
                    command.arg("--check");
                }
                let mut child = command
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .expect("Node.js is required to execute ternary fixtures");
                std::io::Write::write_all(&mut child.stdin.take().unwrap(), out.as_bytes())
                    .unwrap();
                let result = child.wait_with_output().unwrap();
                let expected_code: i32 = std::fs::read_to_string(dir.join(format!("{name}.code")))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                assert_eq!(
                    result.status.code(),
                    Some(expected_code),
                    "{name}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
                assert_eq!(
                    String::from_utf8(result.stdout).unwrap(),
                    std::fs::read_to_string(dir.join(format!("{name}.stdout"))).unwrap(),
                    "{name}"
                );
            }
            count += 1;
        }
        assert_eq!(count, expected_count);
    }

    #[test]
    fn option_reflection_and_json_preserve_public_format() {
        let out = parse_check_and_emit(r#"
fn reflected(v: Option<number>) string { return v.getType().toString(); }
fn encoded(v: Option<number>) string { return v.toJSON(); }
alias Maybe = Option<number>;
alias Many = Array<Option<number>>;
alias Outcome = Result<Option<number>, string>;
const none = "{\"Option\":{\"case\":\"None\"}}".parseJSON<Maybe>();
const many = "[{\"Option\":{\"case\":\"None\"}},{\"Option\":{\"case\":\"Some\",\"values\":[0]}}]".parseJSON<Many>();
const outcome = "{\"Result\":{\"case\":\"Ok\",\"values\":[{\"Option\":{\"case\":\"None\"}}]}}".parseJSON<Outcome>();
const bad = "{\"Option\":{\"case\":\"Some\"}}".parseJSON<Maybe>();
const payload = Some({value: 7});
const n: Option<number> = None;
const t = n.signature();
"#);
        let js = format!("{out}\n{}", r#"
import assert from 'node:assert/strict';
assert.equal(reflected(0), 'Option');
assert.equal(reflected(undefined), 'Option');
assert.equal(t.kind, 'option');
assert.equal(t.inner.name, 'number');
assert.deepEqual(none, {ok: true, value: undefined});
assert.deepEqual(many, {ok: true, value: [undefined, 0]});
assert.deepEqual(outcome, {ok: true, value: {ok: true, value: undefined}});
assert.equal(bad.ok, false);
assert.deepEqual(payload, {value: 7});
assert.deepEqual(JSON.parse(encoded(undefined)), {Option: {case: 'None'}});
assert.deepEqual(JSON.parse(encoded(0)), {Option: {case: 'Some', values: [0]}});
"#);
        let result = std::process::Command::new("node")
            .args(["--input-type=module", "-e", &js]).output().unwrap();
        assert!(result.status.success(), "{}\n{out}", String::from_utf8_lossy(&result.stderr));
    }

    #[test]
    fn option_diagnostic_fixtures() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/diagnostics");
        for name in ["option_void", "option_nested", "option_bodyless"] {
            let dir = root.join(name);
            let source = std::fs::read_to_string(dir.join(format!("{name}.fail.ds"))).unwrap();
            let metadata: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap()).unwrap();
            let arena = Bump::new();
            let parsed = parse(&source, &arena);
            let errors = if !parsed.errors.is_empty() { parsed.errors } else {
                assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
                deka_syntax::typeck::check_program(&parsed.program.unwrap(), &source).errors
            };
            assert!(errors.iter().any(|e| e.message.contains(metadata["expectedDiagnosticContains"].as_str().unwrap())), "{name}: {errors:?}");
        }
    }

    #[test]
    fn result_reflection_and_json_keep_the_public_format() {
        let out = parse_check_and_emit(
            r#"
fn reflected(r: Result<number, string>) string { return r.getType().toString(); }
fn encoded(r: Result<number, string>) string { return r.toJSON(); }
const ordinary = {ok: true, value: 4};
const ordinary_type = ordinary.getType().toString();
alias Outcome = Result<number, string>;
const decoded = "{\"Result\":{\"case\":\"Err\",\"values\":[\"bad\"]}}".parseJSON<Outcome>();
"#,
        );
        let js = format!(
            "{out}\n{}",
            r#"
import assert from 'node:assert/strict';
assert.equal(reflected(Result.Err('bad')), 'Result');
assert.equal(ordinary_type, 'object');
assert.deepEqual(decoded, {ok: true, value: {ok: false, error: 'bad'}});
assert.deepEqual(JSON.parse(encoded(Result.Ok(3))), {Result: {case: 'Ok', values: [3]}});
assert.deepEqual(JSON.parse(encoded(Result.Err('bad'))), {Result: {case: 'Err', values: ['bad']}});
"#
        );
        let result = std::process::Command::new("node")
            .args(["--input-type=module", "-e", &js])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{out}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn result_lifting_loop_conditions_and_single_use_local() {
        let out = parse_check_and_emit(
            r#"
fn data(fail: boolean) Result<number, string> { return fail ? Err("bad") : Ok(3); }
fn local() number { const r = Ok(7); let x = 1; x = x + 1; return match r { Ok(v) => v + x, Err(e) => 0 }; }
fn looping(fail: boolean) Result<number, string> { let total = 0; for (let i = 0; i < (match data(fail) { Ok(v) => v, Err(e) }); i = i + 1) { if (i == 1) { continue; } total = total + i; } return Ok(total); }
fn branch(fail: boolean) Result<number, string> { if (match data(fail) { Ok(v) => true, Err(e) }) { return Ok(4); } return Ok(5); }
fn spread_copy(fail: boolean) Result<number, string> { let original = {x: 1}; const copy = {...original, y: match data(fail) { Ok(v) => original.x = 2, Err(e) }}; return Ok(match unsafe<number> { copy.x } { Ok(v) => v, Err(e) => 0 }); }
fn combined(fail: boolean) Result<number, string> { let n = 1; n += (match data(fail) { Ok(v) => n = 10, Err(e) }); return Ok(n); }
"#,
        );
        let local = out
            .split("function local(")
            .nth(1)
            .unwrap()
            .split("function looping")
            .next()
            .unwrap();
        assert!(
            !local.contains("Result.") && !local.contains("{ ok:"),
            "{local}"
        );
        let js = format!(
            "{out}\n{}",
            r#"
import assert from 'node:assert/strict';
assert.equal(local(), 9);
assert.deepEqual(looping(false), {ok: true, value: 2}); assert.deepEqual(looping(true), {ok: false, error: 'bad'});
assert.deepEqual(branch(false), {ok: true, value: 4}); assert.deepEqual(branch(true), {ok: false, error: 'bad'});
assert.deepEqual(spread_copy(false), {ok: true, value: 1}); assert.deepEqual(spread_copy(true), {ok: false, error: 'bad'});
assert.deepEqual(combined(false), {ok: true, value: 11}); assert.deepEqual(combined(true), {ok: false, error: 'bad'});
"#
        );
        let result = std::process::Command::new("node")
            .args(["--input-type=module", "-e", &js])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{out}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn result_erasure_nested_propagation_and_scalars() {
        let out = parse_check_and_emit(
            r#"
fn data(fail: boolean) Result<number, string> { return fail ? Err("bad") : Ok(7); }
fn native(fail: boolean) Exception<number, string> { if (fail) { return Throw("bad"); } return Ok(7); }
fn nested(fail: boolean) Result<number, string> { return Ok(1 + (match data(fail) { Ok(v) => v, Err(e) })); }
fn nested_exception(fail: boolean) Exception<number, string> { return Ok(1 + (match native(fail) { Ok(v) => v, Throw(e) })); }
fn scalar_unsafe(fail: boolean) number { return match unsafe<number> { if (fail) { throw new Error("bad"); } return 7; } { Ok(v) => v, Err(e) => 0 }; }
fn scalar(fail: boolean) number { return match (fail ? Err("bad") : Ok(7)) { Ok(7) => 9, Ok(v) => v, Err(e) => 0 }; }
fn scalar_conversion(fail: boolean) number { return match native(fail).to_result() { Ok(v) => v, Err(e) => 0 }; }
fn scalar_escape(fail: boolean) Result<number, string> { return Ok(match (fail ? Err("bad") : Ok(7)) { Ok(v) => v, Err(e) }); }
fn lazy(fail: boolean) Result<boolean, string> { return Ok(false && (match data(fail) { Ok(v) => true, Err(e) })); }
fn choice(fail: boolean) Result<number, string> { return Ok(true ? 2 : (match data(fail) { Ok(v) => v, Err(e) })); }
fn arg(a: number, b: number) number { return a + b; }
fn arg_result(a: number, b: number) Result<number, string> { return Ok(a + b); }
fn captured(fail: boolean) Result<number, string> { let seen = 0; const f = arg_result(_, match data(fail) { Ok(v) => v, Err(e) }); seen = 1; const r = f(2); return match r { Ok(v) => Ok(v + seen), Err(e) => Err(string(seen) + e) }; }
fn piped(fail: boolean) Result<number, string> { return Ok(2 |> arg(match data(fail) { Ok(v) => v, Err(e) })); }
fn raise_after(n: number) Exception<number, string> { return Throw("raised"); }
fn nested_conversion() Result<number, string> { return raise_after(match Ok(1) { Ok(v) => v, Err(e) }).to_result(); }
fn order(input: Array<number>, fail: boolean) Result<number, string> { let log = input; return Ok(arg((match Ok(log.push(1)) { Ok(v) => 3, Err(e) => 0 }), (match data(fail) { Ok(v) => v, Err(e) }))); }
fn converted(r: Result<number, string>) Exception<number, string> { return Exception.from(r); }
enum User { Ok(number), Err(string) }
enum Status { Good(number), Bad(string) }
fn user(r: Status) number { return match r { Good(v) => v, Bad(e) => 0 }; }
async fn later() Promise<number> { return 4; }
async fn async_nested(fail: boolean) Promise<Result<number, string>> { return Ok(await later() + (match data(fail) { Ok(v) => v, Err(e) })); }
"#,
        );
        let scalar = out
            .split("function scalar(")
            .nth(1)
            .unwrap()
            .split("function scalar_conversion")
            .next()
            .unwrap();
        assert!(
            !scalar.contains("Result.") && !scalar.contains("{ ok:"),
            "{scalar}"
        );
        assert!(
            scalar.contains("if (") && scalar.contains(".to_result") == false,
            "{scalar}"
        );
        let js = format!(
            "{out}\n{}",
            r#"
import assert from 'node:assert/strict';
assert.deepEqual(nested(false), {ok: true, value: 8});
assert.deepEqual(nested(true), {ok: false, error: 'bad'});
assert.equal(nested_exception(false), 8);
assert.throws(() => nested_exception(true), e => e === 'bad');
assert.deepEqual(captured(false), {ok: true, value: 10}); assert.deepEqual(captured(true), {ok: false, error: '1bad'});
assert.deepEqual(piped(false), {ok: true, value: 9}); assert.deepEqual(piped(true), {ok: false, error: 'bad'});
assert.deepEqual(nested_conversion(), {ok: false, error: 'raised'});
assert.equal(scalar(false), 9); assert.equal(scalar(true), 0);
assert.equal(scalar_unsafe(false), 7); assert.equal(scalar_unsafe(true), 0);
assert.equal(scalar_conversion(false), 7); assert.equal(scalar_conversion(true), 0);
assert.deepEqual(scalar_escape(true), {ok: false, error: 'bad'});
assert.deepEqual(lazy(true), {ok: true, value: false});
assert.deepEqual(choice(true), {ok: true, value: 2});
const log = []; assert.deepEqual(order(log, true), {ok: false, error: 'bad'}); assert.deepEqual(log, [1]);
assert.equal(converted({ok: true, value: 6}), 6);
assert.throws(() => converted({ok: false, error: 'bad'}), e => e === 'bad');
assert.equal(User.Ok(3).__enum, 'User'); assert.equal(user(Status.Good(3)), 3);
assert.deepEqual(await async_nested(false), {ok: true, value: 11});
assert.deepEqual(await async_nested(true), {ok: false, error: 'bad'});
"#
        );
        let result = std::process::Command::new("node")
            .args(["--input-type=module", "-e", &js])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{out}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn exception_native_frames_and_wysiwyg() {
        let out = parse_check_and_emit(
            r#"
fn inner(fail: boolean) Exception<number, string> { if (fail) { return Throw("bad"); } return Ok(7); }
fn delegated(fail: boolean) Exception<number, string> { return inner(fail); }
fn raised(fail: boolean) Exception<number, string> { return match inner(fail) { Ok(v) => Ok(v), Throw(e) }; }
fn as_data(fail: boolean) Result<number, string> { return inner(fail).to_result(); }
fn recovered(fail: boolean) number { try { const n = inner(fail); return n + 1; } catch (e) { return 0; } }
fn preserve(r: Result<number, string>) Result<number, string> { return match r { Ok(_), Err(_) }; }
fn wildcard() Exception<number, string> { return match inner(true) { Ok(v) => Ok(v), Throw("bad"), Throw(e) }; }
fn result_early(fail: boolean) Result<number, string> { const n = match as_data(fail) { Ok(v) => v, Err(e) }; return Ok(n + 1); }
alias Syntax = SyntaxError;
fn typed(e: SyntaxError | TypeError) Exception<number, TypeError> { try { return Throw(e); } catch (e: Syntax) { return Ok(2); } }
struct Trouble { message: string }
fn typed_struct() string { try { return Throw(Trouble { message: "struct" }); } catch (e: Trouble) { return e.message; } }
async fn plain() Promise<number> { return 5; }
async fn nested_await() Promise<number> { return 1 + (match inner(false) { Ok(v) => await plain(), Throw(e) => 0 }); }
async fn rejecting() Promise<Exception<number, string>> { return Throw("async"); }
async fn awaiting() Promise<string> { return match await rejecting() { Ok(v) => "ok", Throw(e) => e }; }
"#,
        );
        assert!(out.contains("throw e;"), "{out}");
        assert!(out.contains("instanceof SyntaxError"), "{out}");
        assert!(!out.contains("__enum: \"Exception\""), "{out}");
        let js = format!(
            "{out}\n{}",
            r#"
const assert = (v) => { if (!v) throw new Error("assertion failed"); };
assert(delegated(false) === 7);
try { delegated(true); throw new Error("not thrown"); } catch (e) { assert(e === "bad"); }
try { raised(true); throw new Error("not raised"); } catch (e) { assert(e === "bad"); }
assert(as_data(true).ok === false && as_data(true).error === "bad");
assert(recovered(true) === 0 && recovered(false) === 8);
assert(result_early(true).ok === false && result_early(false).value === 8);
assert(typed(new SyntaxError("s")) === 2);
const e = new TypeError("t"); try { typed(e); throw new Error("lost type"); } catch (actual) { assert(actual === e); }
assert(await awaiting() === "async");
assert(await nested_await() === 6);
assert(typed_struct() === "struct");
const original = Result.Err("identity"); assert(preserve(original) === original);
try { wildcard(); throw new Error("lost wildcard"); } catch (e) { assert(e === "bad"); }
"#
        );
        let result = std::process::Command::new("node")
            .args(["--input-type=module", "-e", &js])
            .output()
            .expect("Node is required for exception runtime tests");
        assert!(
            result.status.success(),
            "{}\n{out}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn emit_json_struct_round_trip_shape() {
        let out = parse_check_and_emit(
            "struct User { name: string; nickname: Option<string> }\nconst u = User { name: \"bo\", nickname: Some(\"\") }\nconst text = u.toJSON()\nconst back = text.parseJSON<User>()",
        );
        assert!(out.contains("function toJSON$User(v)"), "got: {out}");
        assert!(out.contains("function parseJSON$User(s)"), "got: {out}");
        assert!(out.contains("\"User\""), "got: {out}");
        assert!(out.contains("Option: { case:"), "got: {out}");
        assert!(!out.contains("globalThis"), "got: {out}");
        assert!(!out.contains("prototype.toJSON"), "got: {out}");
    }

    #[test]
    fn emit_union_type_pattern_primitive_predicates() {
        let out = parse_check_and_emit(
            "fn f(v: string | number) string { return match (v) { string(s) => s, number(n) => string(n) }; }",
        );
        assert!(
            out.contains("typeof __deka_match_scrutinee_1 === \"string\""),
            "expected typeof predicate, got: {}",
            out
        );
        assert!(
            out.contains("typeof __deka_match_scrutinee_1 === \"number\""),
            "expected typeof predicate, got: {}",
            out
        );
        // The payload is bound to the scrutinee itself.
        assert!(
            out.contains("const s = __deka_match_scrutinee_1;"),
            "got: {}",
            out
        );
        assert!(
            out.contains("const n = __deka_match_scrutinee_1;"),
            "got: {}",
            out
        );
        // Primitives have no __case tag; emitting one would mean the union
        // lookup was skipped.
        assert!(!out.contains("__case === \"string\""), "got: {}", out);
    }

    #[test]
    fn emit_union_type_pattern_struct_predicate() {
        let out = parse_check_and_emit(
            "struct Point { x: number; y: number }\nfn f(v: Point | string) number { return match (v) { Point(p) => p.x, string(s) => s.length }; }",
        );
        assert!(
            out.contains("__deka_match_scrutinee_1?.__deka_struct === \"Point\""),
            "expected brand-tag predicate, got: {}",
            out
        );
        // The struct factory is emitted because the struct is declared in
        // this module; the type-pattern itself reads the tag directly.
        assert!(out.contains("function __deka_struct"), "got: {}", out);
        assert!(
            out.contains("const p = __deka_match_scrutinee_1;"),
            "got: {}",
            out
        );
    }

    #[test]
    fn emit_union_enum_case_pattern_tests_brand_and_case() {
        let out = parse_check_and_emit(
            "enum Shape { Empty, Rect(number) }\n\
             fn describe(s: Shape | number) string {\n\
               return match (s) {\n\
                 Shape.Empty => \"empty\",\n\
                 Shape.Rect(n) => \"rect \" + string(n),\n\
                 number(n) => string(n),\n\
               }\n\
             }",
        );
        assert!(
            out.contains("__deka_match_scrutinee_1.__enum === \"Shape\" && __deka_match_scrutinee_1.__case === \"Rect\""),
            "expected enum brand and case predicate, got: {out}"
        );
        assert!(
            out.contains("const n = __deka_match_scrutinee_1.value;"),
            "enum payload must bind from the constructor value: {out}"
        );
    }

    #[test]
    fn emit_struct_module_has_no_global_write() {
        // deka#551: the prelude's `const deka = globalThis.deka = {...}`
        // write is gone. Struct machinery is module-local, and the brand is
        // a string id compared by value, so nothing needs a shared global.
        let out =
            parse_and_emit("struct Point { x: number; y: number } const p = Point { x: 1, y: 2 };");
        assert!(
            out.contains("const Point = __deka_struct(\"Point\")"),
            "got: {}",
            out
        );
        assert!(!out.contains("globalThis"), "got: {}", out);
        // A user binding named `deka` must not collide with emitted helpers.
        let out = parse_and_emit("struct Point { x: number } const deka = Point { x: 1 };");
        assert!(!out.contains("globalThis"), "got: {}", out);
    }

    #[test]
    fn emit_struct_brand_is_on_prototype_not_each_instance() {
        let out = parse_and_emit("struct Point { x: number } const p = Point { x: 1 };");
        assert!(
            out.contains("Object.defineProperty(f.prototype,'__deka_struct',{value:id,enumerable:false,writable:false,configurable:false})"),
            "brand must be installed once on the factory prototype: {out}"
        );
        assert!(
            !out.contains("Object.defineProperty(o,'__deka_struct'"),
            "struct construction must not define the brand per instance: {out}"
        );
    }

    #[test]
    fn emit_newtype_module_has_no_global_write() {
        // deka#551: the newtype payload key is shared through Symbol.for's
        // registry, not a globalThis merge.
        let out = parse_and_emit("type Cents number\nconst c = Cents(500);");
        assert!(
            out.contains("const __p = Symbol.for('deka.nt');"),
            "got: {}",
            out
        );
        assert!(!out.contains("globalThis"), "got: {}", out);
    }

    #[test]
    fn emit_newtype_payload_is_not_defined_per_instance() {
        let out = parse_and_emit("type Cents number\nconst c = Cents(500);");
        assert!(out.contains("Cents$values = new WeakMap()"), "got: {out}");
        assert!(out.contains("Cents$values.set(o, v)"), "got: {out}");
        assert!(!out.contains("Object.defineProperty(o, __p"), "got: {out}");
    }

    #[test]
    fn emit_enum_module_has_no_global_write() {
        // Enum machinery was always module-local; pin it so the prelude
        // change cannot drag a global in (deka#551).
        let out = parse_and_emit("enum Color { Red, Green } const c = Color.Red;");
        assert!(out.contains("const Color = Object.freeze"), "got: {}", out);
        assert!(!out.contains("globalThis"), "got: {}", out);
    }

    #[test]
    fn emit_template_interpolation_is_emitted_expression() {
        // dsc#89: `${...}` must be emitted as an evaluated JavaScript
        // interpolation compiled from the AST, not a text passthrough. The
        // expression is written with irregular spacing: only an AST round-trip
        // normalises it to `1 + 2`, so a verbatim passthrough would fail this.
        let out = parse_and_emit("const s = `sum: ${ 1 + 2 }`;");
        assert!(out.contains("${1 + 2}"), "got: {}", out);
    }

    #[test]
    fn emit_template_nested_and_escaped() {
        // Nested template: the inner literal must be emitted inside the
        // outer interpolation so the emitted JS parses as nested templates.
        let out = parse_and_emit("const s = `${`inner ${1}`}`;");
        assert!(out.contains("${`inner ${1}`}"), "got: {}", out);

        // Escaped `\${` stays text and must keep its backslash so the JS
        // template renders a literal `${` at run time.
        let out = parse_and_emit(r"const s = `\${literal}`;");
        assert!(out.contains("\\${literal}"), "got: {}", out);
    }

    #[test]
    fn emit_union_type_pattern_boolean_and_bytes_predicates() {
        let out = parse_check_and_emit(
            "fn f(v: boolean | bytes) number { return match (v) { boolean(b) => 1, bytes(raw) => 2 }; }",
        );
        assert!(
            out.contains("typeof __deka_match_scrutinee_1 === \"boolean\""),
            "got: {}",
            out
        );
        assert!(
            out.contains("__deka_match_scrutinee_1 instanceof Uint8Array"),
            "got: {}",
            out
        );
    }

    #[test]
    fn emit_const_number() {
        let out = parse_and_emit("const x = 42;");
        assert!(out.contains("const x = 42;"), "got: {}", out);
    }

    #[test]
    fn emit_function_with_return() {
        let out = parse_and_emit("fn add(a: number, b: number) number { return a + b; }");
        assert!(out.contains("function add(a, b) {"), "got: {}", out);
        assert!(out.contains("return a + b;"), "got: {}", out);
    }

    #[test]
    fn emit_call_expression() {
        let out = parse_and_emit("console.log(\"hello\");");
        assert!(out.contains("console.log(\"hello\");"), "got: {}", out);
    }

    #[test]
    fn emit_match_expression() {
        let out = parse_check_and_emit("const o = Some(5); const x = match o { Some(n) => n, None => 0 };");
        assert!(out.contains("!== undefined"), "{out}");
        assert!(!out.contains("__case"), "{out}");
    }

    #[test]
    fn emit_struct_tuple_and_nested_pattern_conditions_and_bindings() {
        let out = parse_check_and_emit(
            "struct Point { x: number; y: number }\n\
             enum Message { Move(Point), Stop }\n\
             fn point_x(message: Message) number {\n\
               return match (message) { Move(Point { x }) => x, Stop => 0 };\n\
             }\n\
             fn first(pair: Array<number>) number {\n\
               return match (pair) { (first, second) => first, _ => 0 };\n\
             }",
        );
        assert!(
            out.contains("__deka_match_scrutinee_1.__case === \"Move\" && __deka_match_scrutinee_1.value?.__deka_struct === \"Point\""),
            "nested enum/struct condition missing: {out}"
        );
        assert!(
            out.contains("const x = __deka_match_scrutinee_1.value.x;"),
            "nested field binding missing: {out}"
        );
        assert!(
            out.contains("Array.isArray(__deka_match_scrutinee_2) && __deka_match_scrutinee_2.length === 2"),
            "tuple array/arity condition missing: {out}"
        );
        assert!(
            out.contains("const first = __deka_match_scrutinee_2[0];")
                && out.contains("const second = __deka_match_scrutinee_2[1];"),
            "tuple bindings missing: {out}"
        );
        assert!(!out.contains("=> false"), "false fallback remains: {out}");
    }

    #[test]
    fn emit_match_statement_without_iife() {
        let out = parse_check_and_emit(
            "fn log(n: number) void {} const o = Some(5); match o { Some(n) => log(n), None => log(0) };",
        );
        assert!(
            out.contains("const __deka_match_scrutinee_1 = o;"),
            "got: {}",
            out
        );
        assert!(
            out.contains("if (__deka_match_scrutinee_1 !== undefined)"),
            "got: {}",
            out
        );
        assert!(
            !out.contains("=> {"),
            "match statement still has an IIFE: {}",
            out
        );
        assert!(
            !out.contains("throw new Error(\"non-exhaustive match\")"),
            "got: {}",
            out
        );
        assert!(
            !out.contains("((__deka_scrutinee) =>"),
            "match statement still has an IIFE: {}",
            out
        );
    }

    #[test]
    fn emit_enum_constructor() {
        let out = parse_and_emit("const o = Some(5);");
        assert_eq!(out, "\"use strict\";\nconst o = (5);");
    }

    #[test]
    fn emit_struct_literal() {
        let out = parse_and_emit(
            "struct Point { x: number\n  y: number }\nconst p = Point { x: 1, y: 2 };",
        );
        assert!(
            out.contains("Point({"),
            "expected factory call, got: {}",
            out
        );
        assert!(out.contains("x: 1"), "got: {}", out);
        assert!(out.contains("y: 2"), "got: {}", out);
    }

    #[test]
    fn emit_user_defined_enum_constructor() {
        let out = parse_and_emit("enum Color { Red, Green, Blue } const c = Color.Red;");
        assert!(out.contains("const Color = Object.freeze"), "got: {}", out);
        assert!(out.contains("Color.Red"), "got: {}", out);
    }

    #[test]
    fn emit_user_defined_enum_payload_constructor() {
        let out = parse_and_emit("enum Shape { Circle(number) } const s = Shape.Circle(5);");
        assert!(out.contains("Shape.Circle(5)"), "got: {}", out);
    }

    #[test]
    fn emit_receiver_method() {
        let out = parse_and_emit(
            "struct Point { x: number\n  y: number }\nfn (p Point) distance(other: Point) number { return 0; }\nconst p1 = Point { x: 0, y: 0 };\nconst p2 = Point { x: 3, y: 4 };\nconst d = p1.distance(p2);",
        );
        assert!(out.contains("const Point = __deka_struct"), "got: {}", out);
        assert!(out.contains("Point.impl(\"distance\""), "got: {}", out);
        assert!(out.contains("p1.distance(p2)"), "got: {}", out);
    }

    /// deka#595, module-local granularity: the `__deka_struct` helper carries
    /// only the members this module actually uses. A plain struct factory is
    /// emitted without `impl`, `implMut`, the `MutationError` class, or the
    /// embeds loop.
    #[test]
    fn emit_struct_helper_omits_undemanded_members() {
        let out = parse_and_emit(
            "struct Point { x: number; y: number }\nconst p = Point { x: 1, y: 2 };\nconst n = p.x;",
        );
        assert!(out.contains("function __deka_struct"), "got: {}", out);
        assert!(!out.contains("implMut"), "got: {}", out);
        assert!(!out.contains("f.impl="), "got: {}", out);
        assert!(!out.contains("MutationError"), "got: {}", out);
        assert!(!out.contains("Object.entries(embeds)"), "got: {}", out);
    }

    /// deka#595, module-local granularity: an immutable receiver method
    /// forces `impl` but not `implMut` (and therefore no `MutationError`).
    #[test]
    fn emit_struct_helper_impl_member_gated_by_method_kind() {
        let out = parse_and_emit(
            "struct Counter { n: number }\nfn (c Counter) peek() number { return c.n; }\nconst v = Counter { n: 1 }.peek();",
        );
        assert!(out.contains("f.impl="), "got: {}", out);
        assert!(!out.contains("implMut"), "got: {}", out);
        assert!(!out.contains("MutationError"), "got: {}", out);
        // A mutable method flips exactly the other member on.
        let mut_out = parse_and_emit(
            "struct Counter { n: number }\nfn (c mut Counter) bump() number { return c.n; }\nlet v = Counter { n: 1 };\nv.bump();",
        );
        assert!(mut_out.contains("f.implMut="), "got: {}", mut_out);
        assert!(mut_out.contains("MutationError"), "got: {}", mut_out);
        // Both kinds: both members.
        let both = parse_and_emit(
            "struct Counter { n: number }\nfn (c Counter) peek() number { return c.n; }\nfn (c mut Counter) bump() number { return c.n; }\nlet v = Counter { n: 1 };\nv.peek();\nv.bump();",
        );
        assert!(both.contains("f.impl="), "got: {}", both);
        assert!(both.contains("f.implMut="), "got: {}", both);
    }

    /// deka#595, module-local granularity: only a struct actually declared
    /// with embeds forces the helper's embeds loop.
    #[test]
    fn emit_struct_helper_embeds_member_gated_by_declaration() {
        let with_embed = parse_and_emit(
            "struct Legs {}\nstruct Robot { Legs }\nconst r = Robot { Legs: Legs {} };",
        );
        assert!(
            with_embed.contains("Object.entries(embeds)"),
            "got: {}",
            with_embed
        );
        let without = parse_and_emit("struct Box { w: number }\nconst b = Box { w: 1 };");
        assert!(
            !without.contains("Object.entries(embeds)"),
            "got: {}",
            without
        );
    }

    #[test]
    fn emit_import_named() {
        let out = parse_and_emit("import { add } from \"./math.ds\";");
        assert!(
            out.contains("import { add } from \"./math.ds\";"),
            "got: {}",
            out
        );
    }

    #[test]
    fn emit_import_aliased() {
        let out = parse_and_emit("import { add as plus } from \"./math.ds\";");
        assert!(
            out.contains("import { add as plus } from \"./math.ds\";"),
            "got: {}",
            out
        );
    }

    #[test]
    fn emit_import_side_effect() {
        let out = parse_and_emit("import \"./side-effects.ds\";");
        assert!(
            out.contains("import \"./side-effects.ds\";"),
            "got: {}",
            out
        );
    }

    #[test]
    fn emit_export_const() {
        let out = parse_and_emit("export const x: number = 42;");
        assert!(out.contains("export const x = 42;"), "got: {}", out);
    }

    #[test]
    fn emit_export_function() {
        let out = parse_and_emit("export fn add(a: number, b: number) number { return a + b; }");
        assert!(out.contains("export function add(a, b) {"), "got: {}", out);
        assert!(out.contains("return a + b;"), "got: {}", out);
    }

    #[test]
    fn emit_export_named_group() {
        let out = parse_and_emit("const answer = 42; export { answer };");
        assert!(out.contains("export { answer };"), "got: {}", out);
    }

    #[test]
    fn emit_array_object_index() {
        let out =
            parse_and_emit("const a = [1, 2, 3]; const o = { x: 1 }; const v = a[0] + o[\"x\"];");
        // deka#590 step 2: const literals are no longer frozen at emit; the
        // checker (deka#591) rejects mutation of a const-bound collection.
        assert!(out.contains("const a = [1, 2, 3];"), "got: {}", out);
        assert!(!out.contains("Object.freeze([1, 2, 3])"), "got: {}", out);
        assert!(out.contains("const o = {x: 1};"), "got: {}", out);
        assert!(!out.contains("Object.freeze({x: 1})"), "got: {}", out);
        assert!(out.contains("a[0] + o[\"x\"]"), "got: {}", out);
    }

    #[test]
    fn emit_await_and_pipe() {
        let out = parse_and_emit(
            "async fn fetch() Promise<number> { return 1; } fn double(n: number) number { return n * 2; } const y = await fetch() |> double;",
        );
        assert!(out.contains("await fetch()"), "got: {}", out);
        assert!(out.contains("(double)("), "got: {}", out);
    }

    #[test]
    fn emit_unsafe_expression() {
        let out = parse_and_emit("const r = unsafe { JSON.parse('{}') };");
        assert!(out.contains("ok: true"), "got: {}", out);
        assert!(out.contains("JSON.parse('{}')"), "got: {}", out);
    }

    /// deka#622 finding F: the Ok/Err values `unsafe { }` produces must BE the
    /// shared prelude constructors (deka#582), spliced — not a second,
    /// unbranded transcription that happens to share `__case`/`value`/`error`
    /// keys. The `__case: "Ok"` assertion above passes for either spelling;
    /// if a consumer ever tightens to also require the `__enum` brand or
    /// `name`, an unbranded literal silently stops matching (wrong answer,
    /// not a crash). These assertions fail in that scenario: they require the
    /// exact constructor expressions to appear in the output and the bare
    /// `{ __case }` literals not to. The module prelude is gated behind
    /// `uses_prelude_enums`, which `unsafe { }` never sets, so a hit here can
    /// only come from the `emit_unsafe` splice itself.
    #[test]
    fn emit_unsafe_splices_shared_result_constructors() {
        let out = parse_and_emit("const r = unsafe { JSON.parse('{}') };");
        assert!(
            out.contains(crate::prelude::RESULT_OK),
            "Ok arm must splice the shared constructor, got: {}",
            out
        );
        assert!(
            out.contains(crate::prelude::RESULT_ERR),
            "Err arm must splice the shared constructor, got: {}",
            out
        );
        assert!(
            !out.contains("{ __case: \"Ok\", value:"),
            "unsafe must not transcribe its own Ok literal, got: {}",
            out
        );
        assert!(
            !out.contains("{ __case: \"Err\", error:"),
            "unsafe must not transcribe its own Err literal, got: {}",
            out
        );
    }

    #[test]
    fn emit_unsafe_async_await() {
        let out = parse_and_emit("const r = unsafe { await fetch(url) };");
        assert!(
            out.contains("async function"),
            "expected async wrapper, got: {}",
            out
        );
        assert!(
            out.contains("await fetch(url)"),
            "expected raw await, got: {}",
            out
        );
    }

    /// dsc#60/dsc#103: the bare legacy form types as `Result<Infer, string>`,
    /// so an Error object in its Err payload would be silently accepted as any
    /// type. The payload is normalized to the thrown value's string
    /// representation at the boundary instead, matching the `string` Err side
    /// the checker now types it with.
    #[test]
    fn emit_unsafe_bare_err_payload_is_string() {
        let out = parse_and_emit("const r = unsafe { throw new Error(\"boom\") };");
        assert!(
            out.contains("(err instanceof Error ? (err.message || String(err)) : String(err))"),
            "bare unsafe must normalize the Err payload to a string, got: {}",
            out
        );
    }

    /// dsc#60 negative fixture: the annotated form types as
    /// `Result<T, JsError>` (deka#460), and the `JsError` member table
    /// (`.message`/`.name`) is only sound because the payload stays an
    /// Error object. It must not be stringified.
    #[test]
    fn emit_unsafe_annotated_err_payload_stays_jserror() {
        let out = parse_and_emit("const r = unsafe<string> { throw new Error(\"boom\") };");
        assert!(
            out.contains("(err instanceof Error ? err : new Error(String(err)))"),
            "annotated unsafe must keep the JsError payload, got: {}",
            out
        );
    }

    #[test]
    fn emit_unsafe_statement_block() {
        let out = parse_and_emit("const r = unsafe { const x = 1; return x + 2; };");
        assert!(
            out.contains("const x = 1;"),
            "expected raw JS statements, got: {}",
            out
        );
        assert!(
            out.contains("return x + 2;"),
            "expected raw JS statements, got: {}",
            out
        );
    }

    /// Collapse runs of whitespace so wrapper-selection assertions do not
    /// depend on the emitter's line breaks. deka#424 put the body's delimiters
    /// on their own lines to stop a trailing `//` comment swallowing them,
    /// which broke every assertion here that spelled the spacing out.
    fn squeeze(source: &str) -> String {
        source.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn emit_unsafe_ignores_string_punctuation_and_keywords() {
        let out = parse_and_emit("const a = unsafe { \"a;b\" }; const b = unsafe { \"await\" };");
        let flat = squeeze(&out);
        assert!(
            flat.contains("return ( \"a;b\" )"),
            "string semicolon changed shape: {}",
            out
        );
        assert!(
            flat.contains("return ( \"await\" )"),
            "string await changed shape: {}",
            out
        );
        assert!(
            !out.contains("async function"),
            "string await changed wrapper asyncness: {}",
            out
        );
    }

    #[test]
    fn emit_unsafe_ignores_comment_punctuation() {
        let out = parse_and_emit("const r = unsafe { 1 + 1 /* ; await */ };");
        let flat = squeeze(&out);
        assert!(
            flat.contains("return ( 1 + 1 /* ; await */ )"),
            "comment changed expression shape: {}",
            out
        );
        assert!(
            !out.contains("async function"),
            "comment await changed wrapper asyncness: {}",
            out
        );
    }

    #[test]
    fn emit_unsafe_ignores_regex_punctuation() {
        let out = parse_and_emit("const r = unsafe { /a;b/.test(value) };");
        let flat = squeeze(&out);
        assert!(
            flat.contains("return ( /a;b/.test(value) )"),
            "regex semicolon changed shape: {}",
            out
        );
    }

    #[test]
    fn emit_unsafe_detects_automatic_semicolon_insertion() {
        let out = parse_and_emit("const r = unsafe { 1\n2 };");
        let flat = squeeze(&out);
        // Statement wrapper: no `return (`, the body is spliced as statements.
        assert!(
            flat.contains("function() { 1 2 }"),
            "ASI statements were treated as an expression: {}",
            out
        );
    }

    /// The bug that started deka#423: a body whose last line is a `//` comment
    /// used to swallow the closing delimiters. Both halves are needed -- the
    /// scanner picks the wrapper, deka#424 emits its delimiters on own lines.
    #[test]
    fn emit_unsafe_survives_a_trailing_line_comment() {
        let out = parse_and_emit("const r = unsafe { 1 + 1 // trailing\n };");
        let opens = out.matches('{').count();
        let closes = out.matches('}').count();
        assert_eq!(
            opens, closes,
            "unbalanced braces from trailing comment: {}",
            out
        );
        assert!(
            out.contains("// trailing\n"),
            "comment must stay on its own line: {}",
            out
        );
    }

    #[test]
    fn emit_jsx_element() {
        let out = parse_and_emit("const el = <div class=\"box\" />;");
        assert!(
            out.contains("import { jsx, jsxs, Fragment } from \"@js/react/jsx-runtime\""),
            "got: {}",
            out
        );
        assert!(out.contains("jsx("), "expected jsx call, got: {}", out);
        assert!(out.contains("\"div\""), "expected tag, got: {}", out);
        assert!(
            out.contains("\"class\": \"box\""),
            "expected class prop, got: {}",
            out
        );
    }

    #[test]
    fn emit_jsx_does_not_live_wrap_conditional_elements() {
        let out = parse_and_emit("const el = <div>{cond && <b>hi</b>}</div>;");
        assert!(
            !out.contains("live(function() { return cond &&"),
            "JSX-producing interpolations must not be live() text bindings: {out}"
        );
        assert!(
            out.contains("cond &&"),
            "conditional jsx child should still emit: {out}"
        );
    }

    #[test]
    fn emit_jsx_automatic_runtime_exact_output() {
        for (source, expression) in [
            ("const el = <p>hi</p>;", r#"jsx("p", {"children": "hi"})"#),
            ("const el = <p>hello {name}</p>;", r#"jsxs("p", {"children": ["hello ", name]})"#),
            ("const el = <p>{items}</p>;", r#"jsx("p", {"children": items})"#),
            ("const el = <><Card title=\"ok\" /><p>x</p></>;", r#"jsxs(Fragment, {"children": [jsx(Card, {"title": "ok"}), jsx("p", {"children": "x"})]})"#),
            ("const el = <p key={id} ref={ref} onClick={click} data-deka-id=\"forwarded\">x</p>;", r#"jsx("p", {"ref": ref, "onClick": click, "data-deka-id": "forwarded", "children": "x"}, id)"#),
            ("const el = <>x</>;", r#"jsx(Fragment, {"children": "x"})"#),
        ] {
            let out = parse_and_emit(source);
            assert_eq!(out, format!("\"use strict\";\nimport {{ jsx, jsxs, Fragment }} from \"@js/react/jsx-runtime\";\n\nconst el = {expression};"), "{source}");
        }
    }

    #[test]
    fn emit_jsx_plain_component_props_are_only_user_props() {
        let out = parse_and_emit(
            "interface Props { title: string } fn Card(props: Props) ReactNode { return <p>{props.title}</p>; }",
        );
        assert!(
            !out.contains("data-deka-id"),
            "plain components must not inject hydration markers: {out}"
        );
        assert!(
            out.contains("return jsx(\"p\", {\"children\": props.title});"),
            "{out}"
        );
    }

    #[test]
    fn emit_jsx_islands_inject_data_deka_id_on_host_elements() {
        let out = parse_and_emit(
            "fn Counter() ReactNode { return <button onClick={click}>x</button>; }\nfn click() {}\nconst page = <Counter client:load />;",
        );
        assert!(
            out.contains("\"client:load\": true"),
            "island directive must emit: {out}"
        );
        assert!(
            out.contains("jsx(\"button\", {\"data-deka-id\": \"module:Counter/i0\", \"onClick\": click, \"children\": \"x\"})"),
            "hydration walk matches host elements by data-deka-id: {out}"
        );
        assert!(
            out.contains("jsx(Counter, {\"client:load\": true})"),
            "component tags are not host elements: {out}"
        );
    }

    #[test]
    fn emit_jsx_element_without_children() {
        let out = parse_and_emit("const el = <div class=\"box\" />;");
        assert!(
            out.contains("jsx(\"div\", {\"class\": \"box\"})"),
            "childless elements must emit a two-argument call: {out}"
        );
    }

    #[test]
    fn emit_does_not_duplicate_live_import_when_user_imports_live() {
        // deka#744 F3: `live` is a public export of ui/reactive, so importing
        // it explicitly is legitimate. The compiler's injected `live` import
        // must be skipped — a second binding is a SyntaxError at module load,
        // not a warning.
        let out = parse_and_emit(
            "import { signal, live } from \"ui/reactive\";\n\
             export fn Page() {\n\
               const s = signal(7);\n\
               return <p id=\"sig\">{live(fn() { return s[0](); })}</p>;\n\
             }",
        );
        assert_eq!(
            out.matches("\"ui/reactive\"").count(),
            1,
            "the user import and the injected import must collapse into one: {out}"
        );
        assert!(
            out.contains("import { signal, live } from \"ui/reactive\""),
            "the user's own import must be emitted intact: {out}"
        );
    }

    #[test]
    fn emit_jsx_runtime_names_cannot_capture_user_bindings() {
        for name in ["jsx", "jsxs", "Fragment"] {
            let source = format!("const {name} = 1; const __deka_{name} = 2; const el = <><p>hi</p><p>x</p></>;");
            let out = parse_and_emit(&source);
            assert!(out.contains(&format!("{name} as __deka_{name}_")), "{out}");
            assert!(out.contains(&format!("const {name} = 1;")), "{out}");
        }
    }

    #[test]
    fn emit_injects_missing_jsx_helpers_alongside_user_import() {
        // Importing only `jsx` must suppress just `jsx`: emitted JSX still
        // references jsxs/Fragment, so those injections must remain.
        let out = parse_and_emit(
            "import { jsx } from \"@js/react/jsx-runtime\";\n\
             export fn Page() {\n\
               return <p>hi</p>;\n\
             }\n\
             export fn Raw() {\n\
               return jsx(\"span\", {}, \"x\");\n\
             }",
        );
        assert!(
            out.contains("import { jsx } from \"@js/react/jsx-runtime\""),
            "the user's own import must be emitted intact: {out}"
        );
        assert!(
            out.contains("import { jsx as __deka_jsx, jsxs, Fragment } from \"@js/react/jsx-runtime\""),
            "only the bound name may be skipped; jsxs and Fragment must still be injected: {out}"
        );
    }

    #[test]
    fn emit_binds_live_once_when_user_imports_live_without_using_it() {
        // Single-file emit keeps every import specifier, so the user's line
        // binds `live` and the injection is skipped — the module must declare
        // `live` exactly once either way (deka#744 F3).
        let out = parse_and_emit(
            "import { signal, live } from \"ui/reactive\";\n\
             export fn Page() {\n\
               const s = signal(7);\n\
               return <p id=\"sig\">{s[0]()}</p>;\n\
             }",
        );
        assert_eq!(
            out.matches("\"ui/reactive\"").count(),
            1,
            "exactly one ui/reactive import may exist: {out}"
        );
        assert!(
            out.contains("import { signal, live } from \"ui/reactive\""),
            "the user's own import must be emitted intact: {out}"
        );
    }

    #[test]
    fn emit_skips_css_imports() {
        let out = parse_and_emit("import \"./card.css\";\nconst x = 1;");
        assert!(
            !out.contains("card.css"),
            "CSS imports must not emit JS import: {out}"
        );
        assert!(out.contains("const x = 1;"), "got: {out}");
    }

    #[test]
    fn emit_jsx_css_import_does_not_inject_retired_scope_prop() {
        let out = parse_and_emit("import \"./card.css\"; const el = <div />;");
        assert!(!out.contains("data-deka-cid"), "{out}");
        assert!(!out.contains("data-deka-id"), "{out}");
    }

    #[test]
    fn emit_jsx_omits_cid_without_component_css() {
        let out = parse_and_emit("const el = <div class=\"box\" />;");
        assert!(
            !out.contains("data-deka-cid"),
            "style-free modules must not carry a scope stamp: {out}"
        );
    }

    #[test]
    fn emit_jsx_does_not_stamp_component_tags() {
        let out = parse_and_emit("import \"./card.css\";\nconst el = <Card />;");
        assert!(
            !out.contains("data-deka-cid"),
            "component tags render host elements themselves; stamping the tag itself is dead weight: {out}"
        );
    }

    #[test]
    fn emit_keeps_css_module_specifier_imports() {
        let out =
            parse_and_emit("import { styles } from \"./card.module.css\";\nconst x = styles;");
        assert!(
            out.contains("card.module.css"),
            "CSS module specifier imports must stay in the JS graph: {out}"
        );
    }

    #[test]
    fn emit_jsx_client_directive() {
        let out = parse_and_emit("const el = <Cart client:load userId={id} />;");
        assert!(
            out.contains("\"client:load\": true"),
            "namespaced client directive must emit as a prop: {out}"
        );
        assert!(
            out.contains("\"userId\": id"),
            "island props must emit: {out}"
        );
        assert!(
            !out.contains("..."),
            "island emit must not spread props: {out}"
        );
    }

    #[test]
    fn emit_jsx_fragment() {
        let out = parse_and_emit("const el = <><span>a</span><span>b</span></>;");
        assert!(out.contains("jsxs("), "expected jsxs call, got: {}", out);
        assert!(out.contains("Fragment"), "expected Fragment, got: {}", out);
    }

    #[test]
    fn emit_jsx_does_not_concat_html() {
        let out = parse_and_emit("const el = <div class=\"box\" />;");
        assert!(!out.contains("__deka_ui"), "got: {}", out);
        assert!(!out.contains("`<${tag}"), "got: {}", out);
        assert!(
            !out.contains("\"data-deka-id\""),
            "plain host elements must not inject a hydration marker: {}",
            out
        );
    }

    #[test]
    fn emit_template_literal() {
        let out = parse_and_emit("const s = `hello ${x}`;");
        assert!(
            out.contains("const s = `hello ${x}`;"),
            "expected backtick output, got: {}",
            out
        );
    }

    #[test]
    fn emit_fn_expression_literal() {
        let out = parse_and_emit("const double = fn (x: number) number { return x * 2 };");
        assert!(
            out.contains("const double = function(x) {"),
            "expected function expression, got: {}",
            out
        );
        assert!(
            out.contains("return x * 2;"),
            "expected return body, got: {}",
            out
        );
    }

    #[test]
    fn emit_for_loop() {
        let out = parse_and_emit("for (let i = 0; i < 10; i = i + 1) { break; }");
        assert!(
            out.contains("for (let i = 0; i < 10; i = i + 1) {"),
            "expected for header, got: {}",
            out
        );
        assert!(out.contains("break;"), "expected break, got: {}", out);
    }

    #[test]
    fn emit_async_function() {
        let out = parse_and_emit("async fn value() Promise<number> { return 1 }");
        assert!(
            out.contains("async function value()"),
            "expected async function, got: {}",
            out
        );
        assert!(out.contains("return 1;"), "expected return, got: {}", out);
    }

    #[test]
    fn emit_struct_embed_method() {
        let out = parse_and_emit(
            "struct Legs {} fn (l Legs) move() string { return \"walk\" } struct Robot { Legs } const r = Robot { Legs: Legs {} }; const m = r.move();",
        );
        assert!(out.contains("const Legs = __deka_struct"), "got: {}", out);
        assert!(
            out.contains("const Robot = __deka_struct(\"Robot\", { Legs: Legs })"),
            "got: {}",
            out
        );
        assert!(out.contains("Legs.impl(\"move\""), "got: {}", out);
        assert!(out.contains("r.move()"), "got: {}", out);
    }

    #[test]
    fn emit_struct_embed_promoted_field_literal() {
        // deka#496: promoted fields in a struct literal are routed into the
        // embedded struct's constructor.
        let out = parse_and_emit(
            "struct Person { name: string } struct Employee { Person } const e = Employee { name: \"Bob\" };",
        );
        assert!(
            out.contains("Employee({ Person: Person({ name: \"Bob\" }) })"),
            "got: {}",
            out
        );
    }

    #[test]
    fn emit_struct_embed_nested_promoted_field_literal() {
        let out = parse_and_emit(
            "struct Legs { count: number } struct Robot { Legs } struct Cyborg { Robot } const c = Cyborg { count: 4 };",
        );
        assert!(
            out.contains("Cyborg({ Robot: Robot({ Legs: Legs({ count: 4 }) }) })"),
            "got: {}",
            out
        );
    }

    #[test]
    fn emit_primitive_extension_free_function() {
        // deka#527: primitive extensions are module-local free functions;
        // there is no prototype to hang them on.
        let out = parse_check_and_emit(
            "fn (s string) slugify() string { return s.toLowerCase(); } const title = \"Hello World\"; const slug = title.slugify();",
        );
        assert!(out.contains("function slugify$string(s)"), "got: {}", out);
        assert!(out.contains("slugify$string(title)"), "got: {}", out);
        // Never touch JS prototypes or globalThis: primitives cannot be branded.
        assert!(!out.contains("prototype"), "got: {}", out);
        assert!(!out.contains("globalThis"), "got: {}", out);
        // Primitive extensions must not force the struct factory either.
        assert!(!out.contains("__deka_struct"), "got: {}", out);
    }

    #[test]
    fn emit_primitive_extension_with_params() {
        let out = parse_check_and_emit(
            "fn (n number) add_tax(rate: number) number { return n * (1 + rate); } const total = 100.add_tax(0.2);",
        );
        assert!(
            out.contains("function add_tax$number(n, rate)"),
            "got: {}",
            out
        );
        assert!(out.contains("add_tax$number(100, 0.2)"), "got: {}", out);
    }

    #[test]
    fn emit_primitive_extension_chaining() {
        let out = parse_check_and_emit(
            "fn (s string) a() string { return s; } fn (s string) b() string { return s; } const x = \"v\".a().b();",
        );
        assert!(out.contains("b$string(a$string(\"v\"))"), "got: {}", out);
    }

    #[test]
    fn emit_primitive_extension_shadows_builtin_call() {
        // User extension wins for call-shaped access...
        let out = parse_check_and_emit(
            "fn (s string) toUpperCase() string { return s; } const u = \"x\".toUpperCase();",
        );
        assert!(
            out.contains("function toUpperCase$string(s)"),
            "got: {}",
            out
        );
        assert!(out.contains("toUpperCase$string(\"x\")"), "got: {}", out);
        // ...while builtin members stay verbatim.
        let out = parse_check_and_emit(
            "fn (s string) slugify() string { return s; } const n = \"abc\".length; const t = \"abc\".toUpperCase();",
        );
        assert!(out.contains("\"abc\".length"), "got: {}", out);
        assert!(out.contains("\"abc\".toUpperCase()"), "got: {}", out);
    }

    #[test]
    fn emit_gettype_rewrite() {
        // rfd#41, deka#529: `.getType()` is a compile-time rewrite to the
        // module-local free function `__deka_type_of(x)`, emitted exactly
        // the way deka#527 emits primitive extensions.
        let out = parse_check_and_emit("const t = \"hi\".getType();");
        assert!(out.contains("__deka_type_of(\"hi\")"), "got: {}", out);
        // The descriptor helper and its interning cache are emitted.
        assert!(
            out.contains("function __deka_type_of(v,result=false,option=false)"),
            "got: {}",
            out
        );
        assert!(out.contains("__deka_type_cache"), "got: {}", out);
        // Never touch JS prototypes, the struct factory, or globalThis.
        assert!(!out.contains("prototype"), "got: {}", out);
        assert!(!out.contains("globalThis"), "got: {}", out);
        // `.getType()` alone must not force the struct factory (the tag read
        // inside __deka_type_of is fine; the factory is what must stay out).
        assert!(!out.contains("function __deka_struct"), "got: {}", out);
    }

    #[test]
    fn emit_gettype_toString_chaining() {
        // The outer `.toString()` is an ordinary method call on the real
        // descriptor object; it emits verbatim.
        let out = parse_check_and_emit("const s = \"hi\".getType().toString();");
        assert!(
            out.contains("__deka_type_of(\"hi\").toString()"),
            "got: {}",
            out
        );
    }

    #[test]
    fn emit_gettype_union_receiver() {
        let out = parse_check_and_emit("fn f(v: number | string) Type { return v.getType(); }");
        assert!(out.contains("__deka_type_of(v)"), "got: {}", out);
    }

    #[test]
    fn emit_gettype_user_extension_shadows() {
        // A user extension named `getType` keeps the deka#527 free-function
        // rewrite; the builtin helper stays absent.
        let out = parse_check_and_emit(
            "fn (s string) getType() string { return s; } const u = \"x\".getType();",
        );
        assert!(out.contains("function getType$string(s)"), "got: {}", out);
        assert!(out.contains("getType$string(\"x\")"), "got: {}", out);
        assert!(!out.contains("__deka_type_of"), "got: {}", out);
    }

    #[test]
    fn emit_gettype_helper_absent_without_gettype() {
        let out = parse_check_and_emit("const s = \"hi\".toUpperCase();");
        assert!(!out.contains("__deka_type_of"), "got: {}", out);
        assert!(!out.contains("__deka_type_cache"), "got: {}", out);
    }

    // ------------------------------------------------------------------
    // `first`/`last` array methods (deka#561)
    // ------------------------------------------------------------------

    #[test]
    fn emit_array_first_last_rewrite() {
        // JS arrays have no `first`/`last`. The rewrite evaluates the
        // receiver once, works in expression position, and reads (never
        // mutates), so it is safe on frozen (const) arrays too. pop/shift
        // get the same Option-construction treatment plus the mutation
        // (deka#566, tested separately below).
        let out = parse_check_and_emit(
            "const a: Array<number> = [1, 2, 3];\nlet f = unwrap(a.first()) or { 0 };\nlet l = unwrap(a.last()) or { 0 };",
        );
        assert!(
            out.contains("((v) => v.length > 0 ? v[0] : undefined)(a)"),
            "got: {}",
            out
        );
        assert!(
            out.contains("((v) => v.length > 0 ? v[v.length - 1] : undefined)(a)"),
            "got: {}",
            out
        );
        // The rewrite needs `Some`/`None`, so the enum prelude is forced.
        assert!(!out.contains("const Some = Option.Some;"), "got: {}", out);
        assert!(!out.contains("const None = Option.None;"), "got: {}", out);
    }

    #[test]
    fn emit_array_first_on_empty_returns_none() {
        // Runtime shape, asserted on emitted JS: empty array -> None branch.
        let out = parse_check_and_emit("const e: Array<string> = [];\nconst h = e.first();");
        assert!(
            out.contains("((v) => v.length > 0 ? v[0] : undefined)(e)"),
            "got: {}",
            out
        );
    }

    // ------------------------------------------------------------------
    // Math-backed `number` methods (deka#378 step 2, rfd#40 phase 2)
    // ------------------------------------------------------------------

    #[test]
    fn emit_number_math_total_rewrite() {
        // JS numbers have no `floor`/`max`; verbatim passthrough would be a
        // runtime lie, so the call rewrites to a plain `Math.*` expression.
        let out =
            parse_check_and_emit("const f: number = (3.7).floor();\nconst m: number = (1).max(2);");
        assert!(out.contains("Math.floor((3.7))"), "got: {}", out);
        assert!(out.contains("Math.max((1), 2)"), "got: {}", out);
        // Total calls produce no `Option`, so the enum prelude is not forced.
        assert!(!out.contains("const Some = Option.Some;"), "got: {}", out);
    }

    #[test]
    fn emit_number_math_partial_wraps_nan_as_none() {
        // The honest-values contract (rfd#13): where JS would hand back
        // `NaN`, the emitted wrapper answers `None`.
        let out = parse_check_and_emit(
            "const s: Option<number> = (4).sqrt();\nconst p: Option<number> = (2).pow(10);",
        );
        assert!(
            out.contains("((v) => isNaN(v) ? undefined : v)(Math.sqrt((4)))"),
            "got: {}",
            out
        );
        assert!(
            out.contains("((v) => isNaN(v) ? undefined : v)(Math.pow((2), 10))"),
            "got: {}",
            out
        );
        // The wrapper needs `Some`/`None`, so the enum prelude is forced.
        assert!(!out.contains("const Some = Option.Some;"), "got: {}", out);
        assert!(!out.contains("const None = Option.None;"), "got: {}", out);
    }

    #[test]
    fn emit_number_math_extension_shadows_builtin() {
        // A user extension named `floor` keeps the deka#527 free-function
        // rewrite; the Math-backed builtin must not fire (deka#378 step 2).
        let out = parse_check_and_emit(
            "fn (n number) floor() string { return \"x\"; }\nconst u: string = (3.7).floor();",
        );
        assert!(out.contains("floor$number((3.7))"), "got: {}", out);
        assert!(!out.contains("Math.floor"), "got: {}", out);
    }

    #[test]
    fn emit_array_first_last_absent_without_use() {
        let out =
            parse_check_and_emit("const a: Array<number> = [1];\nconst n: number = a.length;");
        assert!(!out.contains("Some(v["), "got: {}", out);
    }

    #[test]
    fn emit_prelude_cases_without_ephemeral_freezes() {
        let out = parse_check_and_emit(
            "const result = Result.Ok(1);\nconst option = Option.Some(2);\nconst none = Option.None;",
        );
        assert!(
            out.contains("Ok: (value) => ({ ok: true"),
            "Result.Ok should return a plain ephemeral value: {out}"
        );
        assert!(
            out.contains("Err: (error) => ({ ok: false"),
            "Result.Err should return a plain ephemeral value: {out}"
        );
        assert!(
            out.contains("Some: (value) => value"),
            "Option.Some should return a plain ephemeral value: {out}"
        );
        assert!(
            out.contains("None: undefined"),
            "Option.None should be a plain ephemeral value: {out}"
        );
        assert!(
            !out.contains("Object.freeze({ __enum: \"Result\""),
            "Result cases must not be frozen: {out}"
        );
        assert!(
            !out.contains("Object.freeze({ __enum: \"Option\""),
            "Option cases must not be frozen: {out}"
        );
        assert!(
            out.contains("const Result = Object.freeze({")
                && out.contains("const Option = Object.freeze({"),
            "shared constructor tables should remain frozen: {out}"
        );
    }

    #[test]
    fn emit_array_pop_shift_construct_real_option() {
        // deka#566: pop/shift are typed Option<T>, so the emitted JS must
        // construct a real Some/None — a bare `v.pop()` returns the raw
        // element with no `__case` tag and unwrap read a present value as
        // absent. The length guard also turns empty-array pop/shift into
        // None instead of Some(undefined). Asserted on emitted JS.
        let out = parse_check_and_emit(
            "let a: Array<number> = [1, 2, 3];\nlet p = unwrap(a.pop()) or { -1 };\nlet s = unwrap(a.shift()) or { -1 };",
        );
        assert!(
            out.contains("((v) => v.length > 0 ? v.pop() : undefined)(a)"),
            "got: {}",
            out
        );
        assert!(
            out.contains("((v) => v.length > 0 ? v.shift() : undefined)(a)"),
            "got: {}",
            out
        );
        assert!(!out.contains("const Some = Option.Some;"), "got: {}", out);
        assert!(!out.contains("const None = Option.None;"), "got: {}", out);
    }

    #[test]
    fn emit_array_pop_shift_absent_without_use() {
        let out = parse_check_and_emit("let a: Array<number> = [1];\nconst n: number = a.length;");
        assert!(!out.contains("v.pop()"), "got: {}", out);
        assert!(!out.contains("v.shift()"), "got: {}", out);
    }

    // ------------------------------------------------------------------
    // Descriptor const emission (kept for super declarations, PR B)
    // ------------------------------------------------------------------

    #[test]
    fn emit_super_decl_consts_from_handbuilt_maps() {
        // Direct coverage of the emitter's super-decl const interning,
        // feeding emit_js_with_options hand-built maps through the
        // deka_syntax::typeck descriptor public API (the typechecker's own
        // output is covered end-to-end by the parse_check_and_emit tests
        // below). Asserts: one name-keyed const per referenced declaration,
        // shared across call sites of the same declaration, no const for an
        // unreferenced declaration, and a recursive group expanding to both
        // cycle members with lazy composite getters.
        use deka_syntax::typeck::{DescriptorField, DescriptorTree, StaticTypeCall};

        let source = "const a = 1; const b = 2; const c = 3;";
        let arena = Bump::new();
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");

        let mut expr_ptrs: Vec<*const deka_syntax::Expr> = Vec::new();
        for stmt in program.statements.iter() {
            if let deka_syntax::Stmt::Const { value, .. } = stmt {
                expr_ptrs.push(value as *const deka_syntax::Expr);
            }
        }
        assert_eq!(expr_ptrs.len(), 3, "expected three const value exprs");

        let user_tree = DescriptorTree::Struct {
            name: "User",
            fields: vec![DescriptorField {
                name: "id",
                optional: false,
                ty: DescriptorTree::Leaf {
                    kind: "string",
                    name: "string".to_string(),
                },
            }],
        };
        // Unused is in the decl map but referenced by no call site: no const.
        let unused_tree = DescriptorTree::Struct {
            name: "Unused",
            fields: vec![],
        };
        // Node is recursive: next is Option<Node>. The group walk must pull
        // Node's const in alongside Root's.
        let node_tree = DescriptorTree::Struct {
            name: "Node",
            fields: vec![DescriptorField {
                name: "next",
                optional: false,
                ty: DescriptorTree::Option {
                    inner: Box::new(DescriptorTree::Recurse { name: "Node" }),
                },
            }],
        };

        let mut static_type_calls: std::collections::HashMap<
            *const deka_syntax::Expr,
            StaticTypeCall,
        > = std::collections::HashMap::new();
        static_type_calls.insert(
            expr_ptrs[0],
            StaticTypeCall {
                tree: Some(user_tree.clone()),
                param: None,
            },
        );
        // A second call site of the SAME declaration shares the const.
        static_type_calls.insert(
            expr_ptrs[1],
            StaticTypeCall {
                tree: Some(user_tree),
                param: None,
            },
        );
        static_type_calls.insert(
            expr_ptrs[2],
            StaticTypeCall {
                tree: Some(node_tree.clone()),
                param: None,
            },
        );

        let mut super_decl_trees: std::collections::HashMap<&str, DescriptorTree> =
            std::collections::HashMap::new();
        super_decl_trees.insert("User", user_tree_for_map());
        super_decl_trees.insert("Unused", unused_tree);
        super_decl_trees.insert("Node", node_tree);

        fn user_tree_for_map() -> DescriptorTree<'static> {
            DescriptorTree::Struct {
                name: "User",
                fields: vec![DescriptorField {
                    name: "id",
                    optional: false,
                    ty: DescriptorTree::Leaf {
                        kind: "string",
                        name: "string".to_string(),
                    },
                }],
            }
        }

        let out = emit_js_with_options(
            &program,
            source,
            &std::collections::HashMap::new(),
            None,
            &Default::default(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &static_type_calls,
            &super_decl_trees,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            "module.ds",
            None,
        )
        .expect("emit failed");

        // Exactly two consts: User (deduped across both call sites) and the
        // recursive Node — its self-reference expands to a single group
        // const, not an infinite tree. Unused emitted nothing.
        assert_eq!(
            out.matches("const __deka_super_desc$").count(),
            2,
            "got: {}",
            out
        );
        assert!(out.contains("const __deka_super_desc$User"), "got: {}", out);
        assert!(out.contains("const __deka_super_desc$Node"), "got: {}", out);
        assert!(
            !out.contains("__deka_super_desc$Unused"),
            "unused super declaration must not emit a const: {}",
            out
        );
        // The #550 shape triple survives serialization.
        assert!(out.contains("kind: \"struct\""), "got: {}", out);
        assert!(out.contains("name: \"User\""), "got: {}", out);
        assert!(
            out.contains("toString() { return this.name; }"),
            "got: {}",
            out
        );
        // The recursive declaration uses the lazy getter form and references
        // its own const inside it.
        assert!(out.contains("get fields()"), "got: {}", out);
        assert!(
            out.contains("get inner() { return __deka_super_desc$Node; }"),
            "recursive reference must point at the interned const: {}",
            out
        );
    }

    // ------------------------------------------------------------------
    // `super` declarations: `super struct` / `super enum` + `Name.type()`
    // (rfd#41, deka#561 PR B)
    // ------------------------------------------------------------------

    #[test]
    fn emit_super_struct_type_call() {
        let out = parse_check_and_emit(
            "super struct User { id: number; name: string }\nconst t = User.type();",
        );
        // Call site rewrites to the interned const.
        assert!(
            out.contains("const t = __deka_super_desc$User;"),
            "got: {}",
            out
        );
        assert!(out.contains("const __deka_super_desc$User"), "got: {}", out);
        assert!(out.contains("kind: \"struct\""), "got: {}", out);
        assert!(out.contains("name: \"User\""), "got: {}", out);
        assert!(
            out.contains("{ name: \"id\", optional: false"),
            "got: {}",
            out
        );
        // The drift guard: no prototype mutation, no globalThis.
        assert!(!out.contains("globalThis"), "got: {}", out);
    }

    #[test]
    fn emit_super_enum_type_call() {
        let out = parse_check_and_emit(
            "super enum Status { Active, Archived(number) }\nconst t = Status.type();",
        );
        assert!(
            out.contains("const t = __deka_super_desc$Status;"),
            "got: {}",
            out
        );
        assert!(out.contains("kind: \"enum\""), "got: {}", out);
        assert!(out.contains("name: \"Active\""), "got: {}", out);
        assert!(out.contains("name: \"Archived\""), "got: {}", out);
        assert!(!out.contains("globalThis"), "got: {}", out);
    }

    #[test]
    fn emit_super_decl_unused_marking_emits_nothing() {
        // The factory is used (so the struct is live) but `.type()` is never
        // called: no descriptor const, no drag.
        let out =
            parse_check_and_emit("super struct User { id: number }\nconst u = User { id: 1 };");
        assert!(
            !out.contains("__deka_super_desc$"),
            "unused super marking must emit no descriptor const: {}",
            out
        );
        assert!(!out.contains("globalThis"), "got: {}", out);
    }

    #[test]
    fn emit_super_decl_const_forced_by_shaken_call_is_documented_bloat() {
        // A `.type()` call inside a function DCE removes still forces its
        // const: prelude interning is driven by the typechecker's recorded
        // call sites, exactly the "entries pointing into shaken code force
        // their const (harmless bloat, never incorrectness)" guarantee the
        // type_of_calls gate documents. What MUST hold is that the call site
        // itself is gone (no dangling reference) and the const is inert.
        let source = "super struct User { id: number }\nfn describe() Type { return User.type(); }";
        let arena = Bump::new();
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = deka_syntax::typeck::check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        let out = emit_js_with_options(
            &program,
            source,
            &std::collections::HashMap::new(),
            None,
            &typeck.exception_forms,
            &typeck.unwrap_calls,
            &typeck.operator_rewrites,
            &typeck.method_calls,
            &typeck.type_of_calls,
            &typeck.signature_calls,
            &typeck.json_calls,
            &typeck.array_builtin_calls,
            &typeck.number_math_calls,
            &typeck.static_type_calls,
            &typeck.super_trees,
            &typeck.enum_case_patterns,
            &typeck.union_type_patterns,
            &std::collections::HashSet::new(),
            "module.ds",
            Some(&std::collections::HashSet::new()),
        )
        .expect("emit failed");
        assert!(
            !out.contains("return __deka_super_desc$User"),
            "the shaken call site must not survive: {}",
            out
        );
    }

    #[test]
    fn emit_super_struct_recursive_lazy_const() {
        // `super struct Node { next: Option<Node> }` cannot be an eager
        // frozen literal; it must emit the lazy-getter form referencing its
        // own const.
        let out = parse_check_and_emit(
            "super struct Node { next: Option<Node> }\nconst t = Node.type();",
        );
        assert!(out.contains("const __deka_super_desc$Node"), "got: {}", out);
        assert!(out.contains("get fields()"), "got: {}", out);
        assert!(
            out.contains("get inner() { return __deka_super_desc$Node; }"),
            "self-reference must resolve to the interned const: {}",
            out
        );
        assert!(!out.contains("globalThis"), "got: {}", out);
    }

    #[test]
    fn emit_signature_uses_declared_type_and_not_runtime_type() {
        let out = parse_check_and_emit(
            "fn f(v: number | string) Type { return v.signature(); } const s = f(42);",
        );
        assert!(out.contains("kind: \"union\""), "got: {}", out);
        assert!(out.contains("name: \"number | string\""), "got: {}", out);
        assert!(
            out.contains("return Object.freeze({ kind: \"union\""),
            "got: {}",
            out
        );
        assert!(!out.contains("__deka_type_of"), "got: {}", out);
        assert!(!out.contains("globalThis"), "got: {}", out);
        assert!(!out.contains("prototype"), "got: {}", out);
    }

    #[test]
    fn emit_signature_descriptor_is_shaken_with_unused_function() {
        let source = "fn describe(v: number | string) Type { return v.signature(); }";
        let arena = Bump::new();
        let result = parse(source, &arena);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let program = result.program.expect("parse produced no program");
        let typeck = deka_syntax::typeck::check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        let live: std::collections::HashSet<String> = ["main".to_string()].into_iter().collect();
        let out = emit_js_with_options(
            &program,
            source,
            &std::collections::HashMap::new(),
            None,
            &typeck.exception_forms,
            &typeck.unwrap_calls,
            &typeck.operator_rewrites,
            &typeck.method_calls,
            &typeck.type_of_calls,
            &typeck.signature_calls,
            &typeck.json_calls,
            &typeck.array_builtin_calls,
            &typeck.number_math_calls,
            &typeck.static_type_calls,
            &typeck.super_trees,
            &typeck.enum_case_patterns,
            &typeck.union_type_patterns,
            &std::collections::HashSet::new(),
            "module.ds",
            Some(&live),
        )
        .expect("emit failed");
        assert!(!out.contains("kind: \"union\""), "got: {}", out);
    }

    #[test]
    fn emit_bridge_sync_op_is_plain_tagged_result_call() {
        // deka#578: sync catalog ops dispatch to a plain value. The Result
        // tagging is a shared host helper (no per-call IIFE), and the
        // envelope carries __enum exactly like the prelude's Result.
        let out = parse_and_emit("const r = bridge crypto.random_bytes(16)");
        assert!(
            out.contains(
                "__deka_result_from_host(__deka_host(\"crypto\", \"random_bytes\", [16]))"
            ),
            "got: {}",
            out
        );
        assert!(
            !out.contains("(function()"),
            "bridge emit must not wrap the call in an IIFE: {}",
            out
        );
    }

    #[test]
    fn emit_bridge_async_op_returns_promise_for_source_await() {
        // deka#578: async catalog ops (fs.*) return a Promise; the
        // source-level `await` drives the resolution (rfd#27), and the
        // emitted chain tags the envelope through the shared helper.
        let out = parse_and_emit("const r = await bridge fs.read_file(path)");
        assert!(
            out.contains(
                "await __deka_host(\"fs\", \"read_file\", [path]).then(__deka_result_from_host)"
            ),
            "got: {}",
            out
        );
        assert!(
            !out.contains("(function()"),
            "bridge emit must not wrap the call in an IIFE: {}",
            out
        );
    }

    #[test]
    fn emit_bridge_sync_and_async_share_no_per_call_closure() {
        // Two bridge calls must not each mint a closure (the old non-arrow
        // IIFE did, twice per call site).
        let out = parse_and_emit(
            "const a = bridge crypto.random_bytes(8)\nconst b = await bridge fs.mkdirs(\"out\")",
        );
        assert!(
            out.contains("__deka_result_from_host(__deka_host("),
            "got: {}",
            out
        );
        assert!(
            out.contains(".then(__deka_result_from_host)"),
            "got: {}",
            out
        );
        assert!(!out.contains("function("), "got: {}", out);
        assert_eq!(
            out.matches("const __deka_result_from_host =").count(),
            1,
            "{out}"
        );
    }

    #[test]
    fn react_module_specifier_follows_jsx_runtime_family() {
        assert_eq!(react_module_specifier("@js/react/jsx-runtime"), "@js/react");
        assert_eq!(react_module_specifier("react/jsx-runtime"), "react");
        assert_eq!(react_module_specifier("react/jsx-dev-runtime"), "react");
        assert_eq!(react_module_specifier("./jsx-runtime.mjs"), "./react.mjs");
        assert_eq!(react_module_specifier("./runtime.mjs"), "./react.mjs");
        assert_eq!(react_module_specifier("runtime"), "react");
    }

    #[test]
    fn emit_usestate_is_byte_idiomatic() {
        let out = parse_check_and_emit(
            "fn Counter() ReactNode {\n\
               const [count, setCount] = useState(0);\n\
               setCount(1);\n\
               setCount(fn(c: number) number { return c + 1 });\n\
               return <p>{string(count)}</p>;\n\
             }",
        );
        assert!(
            out.contains("import { useState } from \"@js/react\";"),
            "got: {out}"
        );
        assert!(
            out.contains("const [count, setCount] = useState(0);"),
            "got: {out}"
        );
        assert!(out.contains("setCount(1);"), "got: {out}");
        assert!(
            out.contains("setCount(function(c) {\n    return c + 1;\n  });")
                || out.contains("setCount(function(c) {\nreturn c + 1;\n});"),
            "got: {out}"
        );
        assert!(
            !out.contains("function useState") && !out.contains("__deka_useState"),
            "hooks must not be wrapped: {out}"
        );
    }

    #[test]
    fn emit_usestate_alias_imports_resolved_reference() {
        let out = parse_check_and_emit(
            "fn Counter() ReactNode {\n\
               const f = useState;\n\
               const [count, setCount] = f(0);\n\
               return <p>{string(count)}</p>;\n\
             }",
        );
        assert!(
            out.contains("import { useState } from \"@js/react\";"),
            "alias must still emit the resolved builtin import, got: {out}"
        );
        assert!(
            out.contains("const f = useState;") && out.contains("f(0)"),
            "got: {out}"
        );
    }

    #[test]
    fn emit_useref_is_byte_idiomatic() {
        let out = parse_check_and_emit(
            "fn Counter() ReactNode {\n\
               const r = useRef(0);\n\
               r.current = 1;\n\
               return <p>{string(r.current)}</p>;\n\
             }",
        );
        assert!(
            out.contains("import { useRef } from \"@js/react\";"),
            "got: {out}"
        );
        assert!(out.contains("const r = useRef(0);"), "got: {out}");
        assert!(out.contains("r.current = 1;"), "got: {out}");
    }

    #[test]
    fn emit_counter_component_end_to_end() {
        let out = parse_check_and_emit(
            "fn Counter() ReactNode {\n\
               const [count, setCount] = useState(0);\n\
               return <button onClick={fn() { setCount(count + 1) }}>{string(count)}</button>;\n\
             }\n\
             const page = <Counter client:load />;",
        );
        assert!(
            out.contains("import { jsx, jsxs, Fragment } from \"@js/react/jsx-runtime\";"),
            "got: {out}"
        );
        assert!(
            out.contains("import { useState } from \"@js/react\";"),
            "got: {out}"
        );
        assert!(
            out.contains("const [count, setCount] = useState(0);"),
            "got: {out}"
        );
        assert!(out.contains("setCount(count + 1)"), "got: {out}");
    }

    /// SSR of a useState component through real React 19.1.1, matching the
    /// #189 oracle pattern: vendor CJS into workspace `.cache/` (never /tmp)
    /// and point `jsxRuntime` at `react/jsx-runtime` so the derived React
    /// specifier is `react`.
    #[test]
    fn hooks_react_ssr_renders_initial_state() {
        let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.cache/react-hooks-ssr");
        if let Err(err) = ensure_react_ssr_harness(&cache) {
            eprintln!("React SSR harness unavailable ({err}); exact emission still covers the lowering");
            return;
        }
        let arena = bumpalo::Bump::new();
        let source = "export fn Counter() ReactNode {\n\
               const [count, setCount] = useState(7);\n\
               return <p>{string(count)}</p>;\n\
             }";
        let parsed = parse(source, &arena);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let program = parsed.program.expect("parse produced no program");
        let typeck = deka_syntax::typeck::check_program(&program, source);
        assert!(typeck.errors.is_empty(), "{:?}", typeck.errors);
        let out = emit_js_module_with_options(
            &program,
            source,
            &std::collections::HashMap::new(),
            None,
            &typeck.exception_forms,
            &typeck.unwrap_calls,
            &typeck.operator_rewrites,
            &typeck.method_calls,
            &typeck.type_of_calls,
            &typeck.signature_calls,
            &typeck.json_calls,
            &typeck.array_builtin_calls,
            &typeck.number_math_calls,
            &typeck.static_type_calls,
            &typeck.super_trees,
            &typeck.enum_case_patterns,
            &typeck.union_type_patterns,
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            "module.ds",
            None,
            None,
            false,
            Some("react/jsx-runtime".into()),
            false,
            false,
        )
        .expect("emit failed")
        .js;
        assert!(
            out.contains("import { useState } from \"react\";"),
            "got: {out}"
        );
        assert!(
            out.contains("const [count, setCount] = useState(7);"),
            "got: {out}"
        );
        std::fs::write(cache.join("counter.mjs"), &out).unwrap();
        let runner = r#"
import { createElement } from 'react';
import { renderToString } from 'react-dom/server';
import { Counter } from './counter.mjs';
import assert from 'node:assert/strict';
const html = renderToString(createElement(Counter));
assert.ok(html.includes('7'), html);
"#;
        std::fs::write(cache.join("runner.mjs"), runner).unwrap();
        let result = std::process::Command::new("node")
            .arg("runner.mjs")
            .current_dir(&cache)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "stdout: {}\nstderr: {}\nemitted: {out}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn ensure_react_ssr_harness(cache: &std::path::Path) -> Result<(), String> {
        let react_dir = cache.join("node_modules/react");
        let react_dom_dir = cache.join("node_modules/react-dom");
        let scheduler_dir = cache.join("node_modules/scheduler");
        std::fs::create_dir_all(react_dir.join("cjs")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(react_dom_dir.join("cjs")).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(scheduler_dir.join("cjs")).map_err(|e| e.to_string())?;
        fetch_if_missing(
            "https://unpkg.com/react@19.1.1/cjs/react.production.js",
            &react_dir.join("cjs/react.production.js"),
        )?;
        fetch_if_missing(
            "https://unpkg.com/react@19.1.1/cjs/react-jsx-runtime.production.js",
            &react_dir.join("cjs/react-jsx-runtime.production.js"),
        )?;
        fetch_if_missing(
            "https://unpkg.com/react-dom@19.1.1/cjs/react-dom.production.js",
            &react_dom_dir.join("cjs/react-dom.production.js"),
        )?;
        fetch_if_missing(
            "https://unpkg.com/react-dom@19.1.1/cjs/react-dom-server-legacy.node.production.js",
            &react_dom_dir.join("cjs/react-dom-server-legacy.node.production.js"),
        )?;
        fetch_if_missing(
            "https://unpkg.com/scheduler@0.26.0/cjs/scheduler.production.js",
            &scheduler_dir.join("cjs/scheduler.production.js"),
        )?;
        std::fs::write(
            react_dir.join("package.json"),
            r#"{"name":"react","version":"19.1.1","main":"./cjs/react.production.js","exports":{".":"./cjs/react.production.js","./jsx-runtime":"./cjs/react-jsx-runtime.production.js"}}"#,
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(
            react_dom_dir.join("package.json"),
            r#"{"name":"react-dom","version":"19.1.1","main":"./cjs/react-dom.production.js","exports":{".":"./cjs/react-dom.production.js","./server":"./cjs/react-dom-server-legacy.node.production.js"}}"#,
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(
            scheduler_dir.join("package.json"),
            r#"{"name":"scheduler","version":"0.26.0","main":"./cjs/scheduler.production.js"}"#,
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn fetch_if_missing(url: &str, dest: &std::path::Path) -> Result<(), String> {
        if dest.exists() && dest.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            return Ok(());
        }
        let status = std::process::Command::new("curl")
            .args(["-fsSL", "-o", dest.to_str().unwrap(), url])
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("curl {url} failed: {status}"))
        }
    }
}

#[cfg(test)]
mod slot_path_tests {
    use super::dev_slot_source_path;
    use std::path::Path;

    #[test]
    fn dev_slot_source_path_relativizes_under_root() {
        assert_eq!(
            dev_slot_source_path("/a/proj/app/page.ds", Some(Path::new("/a/proj"))),
            "app/page.ds"
        );
    }

    #[test]
    fn dev_slot_source_path_keeps_absolute_outside_root() {
        assert_eq!(
            dev_slot_source_path("/elsewhere/page.ds", Some(Path::new("/a/proj"))),
            "/elsewhere/page.ds"
        );
    }

    #[test]
    fn dev_slot_source_path_keeps_absolute_without_root() {
        assert_eq!(
            dev_slot_source_path("/a/proj/app/page.ds", None),
            "/a/proj/app/page.ds"
        );
    }
}
