# ADHOC

Scenarios that are not snippet fixtures. `./run.sh` runs them after the
snippet grid. Dump publishes them as category **ADHOC** on testsuite.deka.gg
(cached commands + stdout).

```sh
bun tests/testsuite/adhoc/run.mjs
bun tests/testsuite/adhoc/run.mjs --filter serve
DEKA_WASM=path/to/deka_compiler.wasm bun tests/testsuite/adhoc/run.mjs --filter wasm
```
