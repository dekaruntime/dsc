# Public conformance suite

Hats layout. This is the source of truth for https://testsuite.deka.gg.

```
tests/testsuite/<category>/<name>/
  <name>.pass.ds | <name>.fail.ds
  <name>.stdout          # optional exact stdout
  <name>.code            # optional formatter output
  <name>.json            # title, stage, hosts, diagnostics, packages
```

The testsuite **website** repo does not own these files. It displays them
(live browser playground; CACHED RESULTS for native-only cases).

## Run locally

```sh
./run.sh
./run.sh --filter json
cargo build --release -p cli
bun tests/testsuite/run.mjs
bun tests/testsuite/run.mjs --filter json
```

`./run.sh` is the one-command gate (tour + Hats snippets + ADHOC). Same role
as the old testsuite-repo `./run.sh`.

Snippets use `target/release/cli` or `DEKA_NATIVE` (`deka run`). ADHOC
scenarios are not snippets: `deka init`, `deka serve` + HTTP, WASM compile of
`import { echo } from "io"`. Dump records them as category **ADHOC** (cached
commands + stdout on testsuite.deka.gg).

Browser/WASM playground stays live for snippet cases. ADHOC squares are
CACHED RESULTS only.

```
bun tests/testsuite/run.mjs --list
bun tests/testsuite/run.mjs --filter json
bun tests/testsuite/run.mjs --jobs 4
```

See deka#292.
