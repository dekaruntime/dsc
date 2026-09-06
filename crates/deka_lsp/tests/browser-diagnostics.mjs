import { readFile } from "node:fs/promises";

const artifact = process.argv[2];
if (!artifact) throw new Error("usage: bun browser-diagnostics.mjs <diagnostics.wasm>");
const { instance } = await WebAssembly.instantiate(await readFile(artifact));
const e = instance.exports;

for (const name of ["deka_diagnostics_alloc", "deka_diagnostics_analyze", "deka_diagnostics_free", "deka_diagnostics_metadata"]) {
  if (!(name in e)) throw new Error(`required Deka diagnostics ABI export is missing: ${name}`);
}

function write(value) {
  const bytes = new TextEncoder().encode(value);
  if (bytes.length === 0) return [0, 0];
  const ptr = e.deka_diagnostics_alloc(bytes.length);
  new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

function analyze(source, uri) {
  const [sourcePtr, sourceLen] = write(source);
  const [uriPtr, uriLen] = write(uri);
  const resultPtr = e.deka_diagnostics_analyze(sourcePtr, sourceLen, uriPtr, uriLen);
  if (!resultPtr) throw new Error("diagnostics adapter allocation failed");
  const header = new DataView(e.memory.buffer, resultPtr, 8);
  const jsonPtr = header.getUint32(0, true);
  const jsonLen = header.getUint32(4, true);
  const response = JSON.parse(new TextDecoder().decode(new Uint8Array(e.memory.buffer, jsonPtr, jsonLen)));
  if (sourceLen) e.deka_diagnostics_free(sourcePtr, sourceLen);
  if (uriLen) e.deka_diagnostics_free(uriPtr, uriLen);
  e.deka_diagnostics_free(resultPtr, 8 + jsonLen);
  return response;
}

const response = analyze("const label = 'é'; const = ;\n", "file:///workspace/main.ds");
const diagnostic = response.diagnostics?.[0];
if (response.abi_version !== 1 || !response.accepted || diagnostic?.severity !== "error" || diagnostic?.range?.start?.character !== 25 || diagnostic?.range?.end?.character !== 26) {
  throw new Error(`UTF-16 diagnostics ABI mismatch: ${JSON.stringify(response)}`);
}

const rejected = analyze("const = ;", "file:///workspace/legacy.phpx");
if (rejected.accepted || rejected.diagnostics?.length !== 0) {
  throw new Error(`non-.ds source was not rejected: ${JSON.stringify(rejected)}`);
}

console.log("browser WASM diagnostics ABI passed");
