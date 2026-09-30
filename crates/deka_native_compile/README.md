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
The utility vocabulary is defined explicitly in ../deka_native_ir/src/style.rs; unknown utilities
are rejected. It is familiar authoring syntax, not a Tailwind interoperability claim.

`deka_native_ir` owns the shared contract. Its production surface is plain owned
UI values. Its optional `program` feature exposes the serializable development
program. Neither form depends on the parser, a JavaScript engine, or GPUI.

Existing JS emission and language rules are unchanged. The compiler front end
still enforces hooks and type rules. Native-target support is an explicit subset.

## Box contract (program format 2)

All containers use flex layout: column by default (`span`, `p` and `button` use
rows), start justification, cross-axis stretch, grow 0 and shrink 1. This is not
HTML inline/block flow. Unknown classes remain errors. Class tokens apply left
to right; a later declaration wins on the properties it sets.

Supported utilities:
- `flex`, `flex-row`, `flex-col`, `flex-wrap`, `flex-nowrap`.
- `items-start/center/end/stretch`, `self-auto/start/center/end/stretch`.
- `justify-start/center/end/between/around/evenly`.
- `grow`, `grow-0`, `shrink`, `shrink-0`, `flex-none`.
- `w-*`, `h-*`, `min-w-*`, `min-h-*`, `max-w-*`, `max-h-*`: nonnegative
  numeric spacing units (4 logical px each), `auto`, `full`, or a fraction such
  as `1/2`. Fractions must be between 0 and 1 with a nonzero denominator.
- `p-*`, `px-*`, `py-*`, `pt/pr/pb/pl-*` and the equivalent `m` utilities:
  nonnegative numeric spacing. No auto or negative margins.
- `gap-*`, `gap-x-*`, `gap-y-*`: numeric spacing.
- `overflow-visible`, `overflow-hidden`: rectangular clipping, not scrolling.
- `whitespace-normal`, `whitespace-nowrap`: inherited text wrapping policy.
- `rounded-none`, `rounded`, `rounded-lg`; existing text sizes and six-digit
  `bg-[#RRGGBB]` / `text-[#RRGGBB]` colours.

The renderer determines measured text and overflow behaviour. Normal Deka
components can compute class strings on each state update; this restricted
compiler still requires literal class strings. The changed style schema bumps
`FORMAT_VERSION` to 2 so old saved programs fail explicitly rather than acquire
incorrect defaults. Generated Rust uses the same typed style values.
