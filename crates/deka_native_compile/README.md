# Experimental native target

`dsc-native program source.dsx` emits the versioned development representation.
`dsc-native rust source.dsx` emits executable Rust against Deka's `deka_native_ui`.
Both use the existing Deka parser and type checker before a shared, deliberately
small native lowering. Unsupported constructs are errors, not silently discarded.

Build: `cargo build --release -p deka_native_compile`.
Example: `examples/counter.dsx` in this crate.
Runtime and executable host: companion dekaruntime/deka#1171.
Compiler issue: dekaruntime/dsc#307.
Plan: https://github.com/zegadb/staff/issues/32#issuecomment-5903417949.

The first slice supports a single exported zero-argument synchronous component,
numeric useState bindings, nested div/span/p/button elements, literal utilities,
inline numeric state setters, and numeric text expressions. There are no imports,
props, effects, async tasks, text inputs, DOM APIs or arbitrary CSS yet.
The utility vocabulary is defined explicitly in src/style.rs; unknown utilities
are rejected. It is familiar authoring syntax, not a Tailwind interoperability claim.

`deka_native_ir` owns the shared contract. Its production surface is plain owned
UI values. Its optional `program` feature exposes the serializable development
program. Neither form depends on the parser, a JavaScript engine, or GPUI.

Existing JS emission and language rules are unchanged. The compiler front end
still enforces hooks and type rules. Native-target support is an explicit subset.
