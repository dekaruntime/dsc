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
    ModuleEmit, css_scope_hash, emit_js, emit_js_module_with_options, emit_js_with_imports,
    emit_js_with_options,
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
            &typeck.jsx_optional_props,
            &typeck.enum_case_patterns,
            &typeck.union_type_patterns,
            "module.ds",
            None,
        )
        .expect("emit failed")
    }

    #[test]
    fn emit_json_struct_round_trip_shape() {
        let out = parse_check_and_emit(
            "struct User { name: string; nickname: Option<string> }\nconst u = User { name: \"bo\", nickname: Some(\"\") }\nconst text = u.toJSON()\nconst back = text.parseJSON<User>()",
        );
        assert!(out.contains("function toJSON$User(v)"), "got: {out}");
        assert!(out.contains("function parseJSON$User(s)"), "got: {out}");
        assert!(out.contains("\"User\""), "got: {out}");
        assert!(out.contains("\"Option\""), "got: {out}");
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
        let out =
            parse_and_emit("const o = Some(5); const x = match o { Some(n) => n, None => 0 };");
        assert!(
            out.contains("__case"),
            "expected case dispatch, got: {}",
            out
        );
        assert!(out.contains("Some"), "got: {}", out);
        assert!(out.contains("None"), "got: {}", out);
        assert!(out.contains("let __deka_match_result_1;"), "got: {}", out);
        assert!(
            !out.contains("((__deka_scrutinee) =>"),
            "match expression still has an IIFE: {}",
            out
        );
    }

    #[test]
    fn emit_match_statement_without_iife() {
        let out = parse_and_emit(
            "const o = Some(5); match o { Some(n) => console.log(n), None => console.log(0) };",
        );
        assert!(
            out.contains("const __deka_match_scrutinee_1 = o;"),
            "got: {}",
            out
        );
        assert!(
            out.contains("if (__deka_match_scrutinee_1.__case === \"Some\")"),
            "got: {}",
            out
        );
        assert!(
            !out.contains("=> {"),
            "match statement still has an IIFE: {}",
            out
        );
        assert!(
            out.contains("throw new Error(\"non-exhaustive match\")"),
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
        assert!(out.contains("__case"), "expected case tag, got: {}", out);
        assert!(out.contains("Some"), "got: {}", out);
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
        let out = parse_and_emit("struct Point { x: number; y: number }\nconst p = Point { x: 1, y: 2 };\nconst n = p.x;");
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
        let out = parse_and_emit("const a = [1, 2, 3]; const o = { x: 1 }; const v = a[0] + o[\"x\"];");
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
        assert!(out.contains("__case: \"Ok\""), "got: {}", out);
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
            out.contains("import { jsx, jsxs, Fragment } from \"ui/jsx\""),
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
    fn emit_jsx_with_children() {
        let out = parse_and_emit("const el = <p>hello {name}</p>;");
        assert!(out.contains("jsxs("), "expected jsxs call, got: {}", out);
        assert!(
            out.contains("}, ["),
            "children must be a separate argument, not a props field: {out}"
        );
        assert!(
            !out.contains("\"children\":"),
            "children must not be emitted inside the props object: {out}"
        );
        assert!(
            out.contains("import { live } from \"ui/reactive\""),
            "non-literal interpolations must import live: {out}"
        );
        assert!(
            out.contains("live(function() { return name; })"),
            "non-literal interpolations must wrap live(): {out}"
        );
    }

    #[test]
    fn emit_jsx_single_child_is_a_scalar_argument() {
        let out = parse_and_emit("const el = <p>hi</p>;");
        assert!(out.contains("jsx("), "expected jsx call, got: {}", out);
        assert!(
            out.contains("}, \"hi\")"),
            "a single child must be passed as the third argument: {out}"
        );
    }

    #[test]
    fn emit_jsx_element_without_children() {
        let out = parse_and_emit("const el = <div class=\"box\" />;");
        assert!(
            out.contains("jsx(\"div\", {\"data-deka-id\": \"module:_/i0\", \"class\": \"box\"})"),
            "childless elements must emit a two-argument call: {out}"
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
    fn css_scope_hash_is_stable_and_distinct() {
        assert_eq!(css_scope_hash("greeting {}"), "e1c9193cd172");
        assert_eq!(css_scope_hash("greeting {}"), css_scope_hash("greeting {}"));
        assert_ne!(
            css_scope_hash("greeting {}"),
            css_scope_hash("greeting { }")
        );
        // 12 hex chars, the Astro cid shape.
        assert!(css_scope_hash("x").chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(css_scope_hash("x").len(), 12);
    }

    #[test]
    fn emit_jsx_stamps_cid_when_module_imports_css() {
        let source = "import \"./card.css\";\nconst el = <div class=\"box\" />;";
        let out = parse_and_emit(source);
        let cid = css_scope_hash(source);
        assert!(
            out.contains(&format!("\"data-deka-cid-{cid}\": true")),
            "component CSS must stamp host elements with the scope id: {out}"
        );
        assert!(
            out.contains("\"data-deka-id\": \"module:_/i0\""),
            "the hydration id must stay alongside the scope stamp: {out}"
        );
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
            out.contains("\"data-deka-id\""),
            "expected tagged id, got: {}",
            out
        );
        assert!(out.contains("i0"), "expected i0 path segment, got: {}", out);
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
        assert!(out.contains("function __deka_type_of(v)"), "got: {}", out);
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
            out.contains("((v) => v.length > 0 ? Some(v[0]) : None)(a)"),
            "got: {}",
            out
        );
        assert!(
            out.contains("((v) => v.length > 0 ? Some(v[v.length - 1]) : None)(a)"),
            "got: {}",
            out
        );
        // The rewrite needs `Some`/`None`, so the enum prelude is forced.
        assert!(out.contains("const Some = Option.Some;"), "got: {}", out);
        assert!(out.contains("const None = Option.None;"), "got: {}", out);
    }

    #[test]
    fn emit_array_first_on_empty_returns_none() {
        // Runtime shape, asserted on emitted JS: empty array -> None branch.
        let out = parse_check_and_emit("const e: Array<string> = [];\nconst h = e.first();");
        assert!(
            out.contains("((v) => v.length > 0 ? Some(v[0]) : None)(e)"),
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
        let out = parse_check_and_emit("const f: number = (3.7).floor();\nconst m: number = (1).max(2);");
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
            out.contains("((v) => isNaN(v) ? None : Some(v))(Math.sqrt((4)))"),
            "got: {}",
            out
        );
        assert!(
            out.contains("((v) => isNaN(v) ? None : Some(v))(Math.pow((2), 10))"),
            "got: {}",
            out
        );
        // The wrapper needs `Some`/`None`, so the enum prelude is forced.
        assert!(out.contains("const Some = Option.Some;"), "got: {}", out);
        assert!(out.contains("const None = Option.None;"), "got: {}", out);
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
            out.contains("Ok: (value) => ({ __enum: \"Result\""),
            "Result.Ok should return a plain ephemeral value: {out}"
        );
        assert!(
            out.contains("Err: (error) => ({ __enum: \"Result\""),
            "Result.Err should return a plain ephemeral value: {out}"
        );
        assert!(
            out.contains("Some: (value) => ({ __enum: \"Option\""),
            "Option.Some should return a plain ephemeral value: {out}"
        );
        assert!(
            out.contains("None: ({ __enum: \"Option\""),
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
            out.contains("((v) => v.length > 0 ? Some(v.pop()) : None)(a)"),
            "got: {}",
            out
        );
        assert!(
            out.contains("((v) => v.length > 0 ? Some(v.shift()) : None)(a)"),
            "got: {}",
            out
        );
        assert!(out.contains("const Some = Option.Some;"), "got: {}", out);
        assert!(out.contains("const None = Option.None;"), "got: {}", out);
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
            &std::collections::HashMap::new(),
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
            &typeck.jsx_optional_props,
            &typeck.enum_case_patterns,
            &typeck.union_type_patterns,
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
        let out = emit_js_with_options(&program, source, &std::collections::HashMap::new(), None,
            &typeck.unwrap_calls, &typeck.operator_rewrites, &typeck.method_calls,
            &typeck.type_of_calls, &typeck.signature_calls, &typeck.json_calls,
            &typeck.array_builtin_calls,
            &typeck.number_math_calls,
            &typeck.static_type_calls, &typeck.super_trees,
            &typeck.jsx_optional_props, &typeck.enum_case_patterns,
            &typeck.union_type_patterns, "module.ds", Some(&live)).expect("emit failed");
        assert!(!out.contains("kind: \"union\""), "got: {}", out);
    }

    #[test]
    fn emit_bridge_sync_op_is_plain_tagged_result_call() {
        // deka#578: sync catalog ops dispatch to a plain value. The Result
        // tagging is a shared host helper (no per-call IIFE), and the
        // envelope carries __enum exactly like the prelude's Result.
        let out = parse_and_emit("const r = bridge crypto.random_bytes(16)");
        assert!(
            out.contains("__deka_to_result(__deka_host(\"crypto\", \"random_bytes\", [16]))"),
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
            out.contains("await __deka_host(\"fs\", \"read_file\", [path]).then(__deka_to_result)"),
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
            out.contains("__deka_to_result(__deka_host("),
            "got: {}",
            out
        );
        assert!(out.contains(".then(__deka_to_result)"), "got: {}", out);
        assert!(!out.contains("function("), "got: {}", out);
        assert!(!out.contains("=>"), "got: {}", out);
    }
}
