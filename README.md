# dsc

DekaScript compiler. Emits JavaScript. Does not run it.

The host is [`deka`](https://github.com/dekaruntime/deka). See [rfd#38](https://github.com/dekaruntime/rfd/issues/38).

## CLI

Same shape as `deka`: a `Registry` in `crates/core`, commands that `register(&mut registry)`, `crates/cli` builds the list and dispatches.

```
dsc --help
```

Commands (`check`, `transpile`, `fmt`, `lsp`) are registered in `crates/cli/src/lib.rs`.

## CI

Self-hosted only. No V8. sccache buckets are `dsc-sccache-{linux-x64,darwin-x64,darwin-arm64}`.

Browser artifacts: `scripts/build-wasm.sh` → `dsc.wasm` / `dsc_diagnostics.wasm` for `https://dsc-wasm.deka.gg` (publish on release).

## Versioning

Independent of deka. dsc starts at **v0.1.0** and iterates `v0.2.0`, `v0.3.0`, … — it does not inherit the runtime's `0.42` line. See [VERSIONING.md](VERSIONING.md).
