# dsc

DekaScript compiler. Emits JavaScript. Does not run it.

The host is [`deka`](https://github.com/dekaruntime/deka). See [rfd#38](https://github.com/dekaruntime/rfd/issues/38).

## CLI

Same shape as `deka`: a `Registry` in `crates/core`, commands that `register(&mut registry)`, `crates/cli` builds the list and dispatches.

```
dsc --help
```

Commands land as compiler crates are pulled over. They are registered in `crates/cli/src/lib.rs`.
