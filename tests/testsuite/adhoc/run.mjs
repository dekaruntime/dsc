#!/usr/bin/env bun
// ADHOC scenarios for the one-command gate. See tests/testsuite/README.md.

import { runAdhocScenarios } from "./cases.mjs";

function parseArgs(argv) {
  const args = { list: false, json: false, filter: "", help: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--list" || arg === "-l") args.list = true;
    else if (arg === "--json") args.json = true;
    else if (arg === "--filter" || arg === "-f") args.filter = argv[++i] || "";
    else if (arg === "--help" || arg === "-h") args.help = true;
  }
  return args;
}

const NAMES = [
  { name: "deka-init", title: "deka init writes a project v2 can compile" },
  { name: "deka-serve", title: "deka init && deka serve answers HTTP 200" },
  { name: "wasm-io", title: "WASM compiles import { echo } from io" },
];

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    console.log(`usage: bun tests/testsuite/adhoc/run.mjs [--list] [--json] [--filter substr]`);
    process.exit(0);
  }
  if (args.list) {
    for (const c of NAMES) console.log(`  adhoc-${c.name}  ${c.title}`);
    console.log(`\nTotal: ${NAMES.length}`);
    process.exit(0);
  }

  const { cli, wasmPath, results } = await runAdhocScenarios({ filter: args.filter });
  if (args.json) {
    console.log(JSON.stringify({ cli, wasmPath, results }, null, 2));
  } else {
    console.log(`native CLI: ${cli ?? "(missing)"}`);
    console.log(`wasm: ${wasmPath ?? "(missing)"}`);
    console.log(`scenarios: ${results.length}\n`);
    let passed = 0;
    let failed = 0;
    let skipped = 0;
    for (const r of results) {
      if (r.skipped) {
        skipped++;
        console.log(`⊘ ${r.name}  ${r.skipReason ?? "skipped"}`);
        continue;
      }
      if (r.ok) {
        passed++;
        console.log(`✓ ${r.name}`);
      } else {
        failed++;
        console.log(`✗ ${r.name}`);
        if (r.error) console.log(`    ${r.error}`);
      }
    }
    console.log("\n============================================================");
    console.log(` ADHOC  Passed: ${passed} | Failed: ${failed} | Skipped: ${skipped} | Total: ${results.length}`);
    console.log("============================================================\n");
  }

  if (results.some((r) => !r.skipped && !r.ok)) process.exit(1);
}

await main();
