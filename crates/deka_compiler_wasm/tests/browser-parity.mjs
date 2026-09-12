import { existsSync } from "node:fs";
import { readFile, readdir } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const artifact = process.argv[2];
if (!artifact) throw new Error("usage: bun browser-parity.mjs <compiler.wasm>");
const { instance } = await WebAssembly.instantiate(await readFile(artifact));
const e = instance.exports;

for (const name of ["phpx_compile", "phpx_compile_alloc", "phpx_compile_free"]) {
  if (name in e) throw new Error(`removed PHPX ABI export is still public: ${name}`);
}
for (const name of ["deka_compiler_alloc", "deka_compiler_compile", "deka_compiler_free", "deka_compiler_metadata"]) {
  if (!(name in e)) throw new Error(`required Deka ABI export is missing: ${name}`);
}

function write(value) {
  const bytes = new TextEncoder().encode(value);
  const ptr = e.deka_compiler_alloc(bytes.length);
  new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

function compile(source, filename, options) {
  const optionsJson = typeof options === "string" ? JSON.stringify({ mode: options }) : JSON.stringify(options);
  const [sourcePtr, sourceLen] = write(source);
  const [filenamePtr, filenameLen] = write(filename);
  const [optionsPtr, optionsLen] = write(optionsJson);
  const resultPtr = e.deka_compiler_compile(sourcePtr, sourceLen, filenamePtr, filenameLen, optionsPtr, optionsLen);
  const header = new DataView(e.memory.buffer, resultPtr, 8);
  const jsonPtr = header.getUint32(0, true);
  const jsonLen = header.getUint32(4, true);
  const response = JSON.parse(new TextDecoder().decode(new Uint8Array(e.memory.buffer, jsonPtr, jsonLen)));
  e.deka_compiler_free(sourcePtr, sourceLen);
  e.deka_compiler_free(filenamePtr, filenameLen);
  e.deka_compiler_free(optionsPtr, optionsLen);
  e.deka_compiler_free(resultPtr, 8 + jsonLen);
  return response;
}

const success = compile("const answer = 42;", "lesson.ds", "auto");
if (!success.ok || success.metadata.language !== "deka" || !success.output?.code?.includes("const answer") || !success.output?.code?.includes("42")) {
  throw new Error(`successful .ds compile did not match the ABI contract: ${JSON.stringify(success)}`);
}

const rejectedFilename = compile("function greeting($name: string): string { return $name; }", "legacy.phpx", "phpx");
if (rejectedFilename.ok || !rejectedFilename.diagnostics?.[0]?.message.includes("only accepts .ds")) {
  throw new Error(`PHPX filename fallback was not rejected: ${JSON.stringify(rejectedFilename)}`);
}

const rejectedMode = compile("const answer = 42;", "lesson.ds", "phpx");
if (rejectedMode.ok || !rejectedMode.diagnostics?.[0]?.message.includes("supported modes are `auto` and `deka`")) {
  throw new Error(`PHPX mode fallback was not rejected: ${JSON.stringify(rejectedMode)}`);
}

const failure = compile("function broken(", "broken.ds", "deka");
const diagnostic = failure.diagnostics?.[0];
if (failure.ok || diagnostic?.severity !== "error" || diagnostic.filename !== "broken.ds" || !Number.isInteger(diagnostic.start_line)) {
  throw new Error(`diagnostic compile did not match the ABI contract: ${JSON.stringify(failure)}`);
}

// The tour is owned by dekaruntime/tour and fetched by
// scripts/ci-fetch-tour.sh; it is never vendored under tests/.
const tourDir = join(dirname(fileURLToPath(import.meta.url)), "../../../.cache/tour");
const tourManifest = JSON.parse(await readFile(join(tourDir, "manifest.json"), "utf-8"));
const tourFiles = (await readdir(tourDir)).filter((name) => name.endsWith(".ds") || name.endsWith(".dsx"));
const manifestIds = new Set(tourManifest.map((lesson) => lesson.id));
const fileIds = new Set(tourFiles.map((name) => name.replace(/\.dsx?$/, "")));
for (const id of manifestIds) {
  if (!fileIds.has(id)) throw new Error(`.cache/tour/manifest.json lists ${id} but ${id}.ds/.dsx is missing`);
}
for (const id of fileIds) {
  if (!manifestIds.has(id)) throw new Error(`.cache/tour/${id}.ds/.dsx is not listed in manifest.json`);
}
if (tourManifest.length === 0) {
  throw new Error(".cache/tour must contain at least one lesson");
}
for (const lesson of tourManifest) {
  const extension = existsSync(join(tourDir, `${lesson.id}.dsx`)) ? "dsx" : "ds";
  const source = await readFile(join(tourDir, `${lesson.id}.${extension}`), "utf-8");
  const response = compile(source, `${lesson.id}.${extension}`, "deka");
  if (response.ok !== lesson.expectCompile) {
    throw new Error(`${lesson.id} browser WASM compile result drifted: ${JSON.stringify(response)}`);
  }
  if (lesson.expectCompile && typeof response.output?.code !== "string") {
    throw new Error(`${lesson.id} did not return browser WASM output: ${JSON.stringify(response)}`);
  }
  if (!lesson.expectCompile && !response.diagnostics?.some((diagnostic) => diagnostic.severity === "error")) {
    throw new Error(`${lesson.id} browser WASM expected an error diagnostic: ${JSON.stringify(response)}`);
  }
}

const structSource = `struct Point {
  x: number
  y: number
}

const origin = Point { x: 3, y: 4 };
origin.x + origin.y`;
const structResponse = compile(structSource, "structs.ds", "deka");
if (!structResponse.ok || !structResponse.output?.code) {
  throw new Error(`struct compile failed: ${JSON.stringify(structResponse)}`);
}
// The emitted JS is executed by the tour in a strict-mode function. Ensure the
// deka.Struct factory does not assign to f.name (which is non-writable in strict
// mode and throws "Attempted to assign to readonly property.").
//
// The emitted code is run verbatim. It is deliberately NOT reshaped with
// regexes: a pattern cannot see string or template-literal boundaries, so
// `[^;]+;` stops at a semicolon inside a string and `[\s\S]*$` silently
// deletes the rest of the file — leaving a program that still parses and a
// test that passes while executing nothing. If emit ever grows module syntax,
// this must become a module runner, not a stripper. The assertion below is
// what makes that a loud failure instead of a silent one.
const structCode = structResponse.output.code;
if (/^\s*export[\s{]/m.test(structCode)) {
  throw new Error(
    "emitted JS now contains `export`, which `new Function` cannot evaluate.\n" +
      "Run the module properly (e.g. import a data: URL) instead of stripping\n" +
      "exports with a regex — see dekaruntime/deka#359.",
  );
}
try {
  const run = new Function(
    `"use strict";\n${structCode}\nreturn origin.x + origin.y;`,
  );
  const result = run();
  if (result !== 7) {
    throw new Error(`expected struct output to be 7, got: ${JSON.stringify(result)}`);
  }
} catch (error) {
  throw new Error(`strict-mode struct execution failed: ${error.message}\n${structCode}`);
}

// Hostile fixture (deka#359): every character class that breaks a codegen path
// which assembles or reshapes source as text — a template literal, `${`, an
// escaped backslash and an escape sequence. The struct prelude above already
// carries backticks, so a regression here breaks both fixtures at once.
const hostileSource = `const name = "world"
const greeting = \`hi \${name}\`
const path = "c:\\\\tmp"
const multi = "a\\nb"
greeting + path + multi`;
const hostileResponse = compile(hostileSource, "hostile.ds", "deka");
if (!hostileResponse.ok || !hostileResponse.output?.code) {
  throw new Error(`hostile fixture failed to compile: ${JSON.stringify(hostileResponse)}`);
}
const hostileResult = new Function(
  `"use strict";\n${hostileResponse.output.code}\nreturn greeting + path + multi;`,
)();
if (hostileResult !== "hi worldc:\\tmpa\nb") {
  throw new Error(
    `hostile fixture round-trip corrupted the source: ${JSON.stringify(hostileResult)}`,
  );
}

// Stdlib imports are typed as `Infer` in the single-file WASM compiler, so a
// function that calls `echo` (imported from "io") should compile.
const ioInsideFunction = `import { echo } from "io"
fn greet(name: string) {
  echo("hello " + name)
}
greet("deka")`;
const ioResponse = compile(ioInsideFunction, "io-function.ds", "deka");
if (!ioResponse.ok || !ioResponse.output?.code?.includes('import { echo }')) {
  throw new Error(`stdlib io import inside function failed: ${JSON.stringify(ioResponse)}`);
}

// Match the native ABI contract: a moduleBase rewrites resolved imports;
// it cannot supply missing package export metadata.
const packageResponse = compile(
  `import { Widget } from "@acme/widgets"\nconst answer = 42`,
  "package-import.ds",
  { mode: "deka", moduleBase: "/tour/modules" },
);
if (packageResponse.ok || !packageResponse.diagnostics?.some(
  diagnostic => diagnostic.message.includes("cannot resolve imported name `Widget`"),
)) {
  throw new Error(`unresolved package import was not rejected: ${JSON.stringify(packageResponse)}`);
}

// .dsx files are DS + JSX and must be accepted by the browser compiler ABI.
const dsxSource = `const el = <div class="box"><span>hi</span></div>`;
const dsxResponse = compile(dsxSource, "component.dsx", "deka");
if (!dsxResponse.ok || !dsxResponse.output?.code) {
  throw new Error(".dsx filename was rejected by browser compiler");
}

// rfd#62 stage 2a must use the same data and control-flow lowering in WASM.
const erasureSource = `
fn data(fail: boolean) Result<number, string> { return fail ? Err("bad") : Ok(7); }
fn nested(fail: boolean) Result<number, string> { return Ok(1 + (match data(fail) { Ok(v) => v, Err(e) })); }
fn melted() number { const r = Ok(7); return match r { Ok(v) => v, Err(e) => 0 }; }
fn source(fail: boolean) Exception<number, string> { if (fail) { return Throw("bad"); } return Ok(7); }
fn nested_exception(fail: boolean) Exception<number, string> { return Ok(1 + (match source(fail) { Ok(v) => v, Throw(e) })); }
enum User { Present(number), Absent }
const user = User.Present(3);
const option = Some(4);
`;
const erasure = compile(erasureSource, "erasure.ds", "deka");
if (!erasure.ok) throw new Error(`Result erasure WASM compilation failed: ${JSON.stringify(erasure)}`);
const observed = new Function(`${erasure.output.code}\nreturn [nested(false), nested(true), melted(), nested_exception(false), user.__enum, option.__case];`)();
const wanted = [{ok: true, value: 8}, {ok: false, error: "bad"}, 7, 8, "User", "Some"];
if (JSON.stringify(observed) !== JSON.stringify(wanted)) {
  throw new Error(`Result erasure WASM runtime mismatch: ${JSON.stringify(observed)}`);
}
try {
  new Function(`${erasure.output.code}\nnested_exception(true);`)();
  throw new Error("WASM emission lost native exception propagation");
} catch (error) {
  if (error !== "bad") throw error;
}
const wrongChannel = compile(`fn data() Result<number, string> { return Err("bad"); } fn f() Exception<number, string> { return Ok(match data() { Ok(v) => v, Err(e) }); }`, "wrong-channel.ds", "deka");
if (wrongChannel.ok || !wrongChannel.diagnostics.some(d => d.message.includes("raise it into the exception channel explicitly"))) {
  throw new Error(`WASM lost nested cross-channel diagnostic: ${JSON.stringify(wrongChannel)}`);
}
const pausedOption = compile(`fn noop() void {} const present = Some(noop());`, "option-paused.ds", "deka");
if (!pausedOption.ok) throw new Error("stage 2a must not reject Option<void>");

console.log("browser WASM parity fixtures passed");
