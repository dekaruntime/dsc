# Pairing handoff

Worktree: `/Volumes/Projects/codex/react-emit/dsc-react-jsx-runtime`
Branch: `feat/react-jsx-runtime`
Base: origin/main at `9bb4265` (includes dsc#186).
Build output: `/Volumes/Projects/codex/react-emit/target-react-jsx-runtime`.

1. Apply the two owner patches against the exact bases in [README.md](README.md),
   using `git apply --check` before `git apply`. Ava coordinates the testsuite
   and tour owner PRs, releases, and checksum pin updates.
2. Keep this compiler PR draft while either original pin is still in place.
   The unchanged pins intentionally expose the required pairing. Do not add
   expected failures or skips to hide it.
3. After owner releases, update the two compiler pin files to reviewed tags /
   revisions and archive checksums, fetch afresh, and rerun the gates below.
4. This lane opens one dsc PR and stops. It does not merge, tag, publish owner
   changes, change the spike, or implement hooks/state.

```sh
cd /Volumes/Projects/codex/react-emit/dsc-react-jsx-runtime
export CARGO_TARGET_DIR=/Volumes/Projects/codex/react-emit/target-react-jsx-runtime
scripts/ci-fetch-testsuite-corpus.sh
scripts/ci-fetch-tour.sh
scripts/ci-install-deka-runtime.sh "$PWD/.cache/deka-runtime/deka"
scripts/ci-seed-tour-io.sh "$PWD/.cache/deka-runtime/deka"
cargo test --locked --workspace --no-fail-fast
cargo build --locked -p cli
DSC="$CARGO_TARGET_DIR/debug/dsc" bun .cache/tour/run.mjs
DEKA_NATIVE="$PWD/.cache/deka-runtime/deka" \
  DEKA_DSC="$CARGO_TARGET_DIR/debug/dsc" bun .cache/testsuite-corpus/run.mjs
cargo clippy --locked --all-features
cargo check --locked --target wasm32-unknown-unknown \
  -p deka_compiler_wasm -p dekascript_lsp_wasm --no-default-features
node --test crates/deka_ui/js/server.test.mjs
```

## Spike oracle

`deka#922` findings 2–5 are the oracle. A scratch copy of
`spikes/react-919/Component.dsx`, changing only its old return annotation to
`ReactNode`, is rendered by the spike's frozen React/react-dom/server 19.1.1.
The actual React 19.1.1 production JSX runtime comes from
`https://unpkg.com/react@19.1.1/cjs/react-jsx-runtime.production.js`, SHA-256
`1e46f15002696985e80c61d47aaa30dabb03c954270a690f5f7dfbbacfa9002b`.
Its CommonJS export object is packaged as ESM in the scratch directory; no
JSX factory is reimplemented. `jsxRuntime` points to that local module.
The Node resolver rejects either legacy `ui/jsx` or `ui/reactive` import.

Preserved, bundled, and minified emission are executed through real SSR and
assert both child strings and `data-deka-id`. The emitted source is not
rewritten. Scratch output is under `.cache/react-oracle/` (not committed).

The spike adapter's `jsx` / `jsxs` → `React.createElement` conversion, its
`Fragment` re-export, `live(read) { return read(); }`, and the loader redirects
for `ui/jsx` / `ui/reactive` become dead. Its `stack`/`text` vocabulary mapping
is a separate renderer concern: direct React DOM SSR produces custom
`<stack>` / `<text>` tags. This check does not claim unchanged Ink execution,
new package-manager subpath support, or a completed renderer vocabulary.

The real-runtime minification probe exposed a missing post-compression SWC
fixer: React's `&& (key = …)` lost required parentheses. The shared minifier
now runs the fixer, and the permanent bundle test executes that key expression.

`data-deka-cid-*` is removed: its documented consumer was the deleted
`runtime_core::framework` CSS writer, absent from current Deka main.
`data-deka-id` is the only injected prop. Optional omissions stay omitted;
React owns key, while ref/events remain ordinary checked props. Event-based,
transitive hydration diagnostics stay; reactive imports and a function merely
named `signal` no longer trigger them.
