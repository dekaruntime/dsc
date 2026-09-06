// ADHOC scenarios: product paths that are not snippet fixtures.
// Native `deka run` of a .ds file cannot see these. ./run.sh runs them too.

import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { spawn, spawnSync } from "node:child_process";
import { createServer } from "node:net";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import os from "node:os";

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(__dirname, "..", "..", "..");

function findCliBinary() {
  if (process.env.DEKA_NATIVE) {
    const resolved = isAbsolute(process.env.DEKA_NATIVE)
      ? process.env.DEKA_NATIVE
      : resolve(process.cwd(), process.env.DEKA_NATIVE);
    if (!existsSync(resolved)) {
      throw new Error(`DEKA_NATIVE is set to ${process.env.DEKA_NATIVE} but that file does not exist`);
    }
    return resolved;
  }
  const candidate = join(repoRoot, "target", "release", "cli");
  try {
    const stat = statSync(candidate);
    if (stat.isFile() && (stat.mode & 0o111)) return candidate;
  } catch {}
  return null;
}

function findWasmArtifact() {
  if (process.env.DEKA_WASM) {
    const resolved = isAbsolute(process.env.DEKA_WASM)
      ? process.env.DEKA_WASM
      : resolve(process.cwd(), process.env.DEKA_WASM);
    if (existsSync(resolved)) return resolved;
  }
  const candidates = [
    join(repoRoot, "dist", "deka-compiler-wasm", "deka_compiler.wasm"),
    join(repoRoot, "target", "wasm32-unknown-unknown", "release", "deka_compiler_wasm.wasm"),
  ];
  return candidates.find((p) => existsSync(p)) ?? null;
}

function runCli(cli, args, cwd, extra = {}) {
  const spawned = spawnSync(cli, args, {
    cwd,
    encoding: "utf-8",
    timeout: extra.timeout ?? 60000,
    env: { ...process.env, DEKA_SECURITY_NO_PROMPT: "1", ...(extra.env ?? {}) },
  });
  return {
    status: spawned.status,
    stdout: spawned.stdout ?? "",
    stderr: spawned.stderr ?? "",
    error: spawned.error?.message,
  };
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      const port = typeof addr === "object" && addr ? addr.port : 0;
      server.close((err) => (err ? reject(err) : resolvePort(port)));
    });
    server.on("error", reject);
  });
}

async function waitForHttp(url, timeoutMs) {
  const start = Date.now();
  let lastErr = "";
  while (Date.now() - start < timeoutMs) {
    try {
      const res = await fetch(url);
      const body = await res.text();
      return { status: res.status, body };
    } catch (err) {
      lastErr = err instanceof Error ? err.message : String(err);
      await new Promise((r) => setTimeout(r, 150));
    }
  }
  throw new Error(`server did not answer ${url} within ${timeoutMs}ms (${lastErr})`);
}

async function scenarioInit(cli) {
  const dir = mkdtempSync(join(os.tmpdir(), "deka-adhoc-init-"));
  const commands = [
    `cd ${dir}`,
    `${cli} init`,
    `${cli} check ./app/page.dsx`,
  ];
  try {
    const init = runCli(cli, ["init"], dir);
    const check = runCli(cli, ["check", "./app/page.dsx"], dir);
    const stdout = ["# deka init", init.stdout, init.stderr, "# deka check ./app/page.dsx", check.stdout, check.stderr]
      .filter((s) => s && s.length)
      .join("\n");
    const ok = init.status === 0 && check.status === 0;
    return {
      name: "deka-init",
      title: "deka init writes a project v2 can compile",
      commands,
      ok,
      skipped: false,
      stdout,
      stderr: check.stderr || init.stderr,
      error: ok
        ? undefined
        : check.error ||
          init.error ||
          (check.status !== 0 ? `deka check exited ${check.status}` : `deka init exited ${init.status}`),
    };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

async function scenarioServe(cli) {
  const dir = mkdtempSync(join(os.tmpdir(), "deka-adhoc-serve-"));
  const port = await freePort();
  const commands = [
    `cd ${dir}`,
    `${cli} init`,
    `${cli} serve --port ${port}`,
    `curl -fsS http://127.0.0.1:${port}/`,
  ];
  let child = null;
  try {
    const init = runCli(cli, ["init"], dir);
    if (init.status !== 0) {
      return {
        name: "deka-serve",
        title: "deka init && deka serve answers HTTP 200",
        commands,
        ok: false,
        skipped: false,
        stdout: init.stdout,
        stderr: init.stderr,
        error: init.error || `deka init exited ${init.status}; serve not started`,
      };
    }
    child = spawn(cli, ["serve", "--port", String(port)], {
      cwd: dir,
      env: { ...process.env, DEKA_SECURITY_NO_PROMPT: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let serveOut = "";
    child.stdout?.on("data", (chunk) => {
      serveOut += chunk.toString();
    });
    child.stderr?.on("data", (chunk) => {
      serveOut += chunk.toString();
    });
    const http = await waitForHttp(`http://127.0.0.1:${port}/`, 15000);
    const ok = http.status === 200;
    const stdout = ["# deka init", init.stdout, `# GET http://127.0.0.1:${port}/`, `HTTP ${http.status}`, http.body, "# serve log", serveOut].join(
      "\n"
    );
    return {
      name: "deka-serve",
      title: "deka init && deka serve answers HTTP 200",
      commands,
      ok,
      skipped: false,
      stdout,
      stderr: serveOut,
      error: ok ? undefined : `expected HTTP 200, got ${http.status}`,
    };
  } catch (err) {
    return {
      name: "deka-serve",
      title: "deka init && deka serve answers HTTP 200",
      commands,
      ok: false,
      skipped: false,
      stdout: "",
      stderr: "",
      error: err instanceof Error ? err.message : String(err),
    };
  } finally {
    if (child && child.pid) {
      try {
        child.kill("SIGTERM");
      } catch {}
    }
    rmSync(dir, { recursive: true, force: true });
  }
}

async function scenarioWasmIo(wasmPath) {
  const source = readFileSync(join(__dirname, "io-echo.ds"), "utf-8");
  const commands = [
    `DEKA_WASM=${wasmPath ?? "<missing>"}`,
    `wasm compile io-echo.ds  # import { echo } from "io"`,
  ];
  if (!wasmPath) {
    return {
      name: "wasm-io",
      title: "WASM compiles import { echo } from io",
      commands,
      ok: false,
      skipped: true,
      skipReason: "no wasm artifact (set DEKA_WASM or build scripts/test-deka-compiler-wasm.sh)",
      stdout: "",
      stderr: "",
      error: undefined,
    };
  }
  const wasmBytes = readFileSync(wasmPath);
  const { instance } = await WebAssembly.instantiate(wasmBytes);
  const e = instance.exports;
  const write = (value) => {
    const bytes = new TextEncoder().encode(value);
    const ptr = e.deka_compiler_alloc(bytes.length);
    new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
    return [ptr, bytes.length];
  };
  const [sourcePtr, sourceLen] = write(source);
  const [filenamePtr, filenameLen] = write("io-echo.ds");
  const optionsJson = JSON.stringify({ mode: "deka" });
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

  const diag = (response.diagnostics ?? []).map((d) => d.message || d.rendered || "").join("\n");
  const ok = Boolean(response.ok);
  return {
    name: "wasm-io",
    title: "WASM compiles import { echo } from io",
    commands,
    ok,
    skipped: false,
    stdout: JSON.stringify({ ok: response.ok, diagnostics: response.diagnostics ?? [] }, null, 2),
    stderr: diag,
    error: ok ? undefined : diag || "wasm compile failed",
  };
}

function skippedResult(name, title, reason) {
  return {
    name,
    title,
    commands: [],
    ok: false,
    skipped: true,
    skipReason: reason,
    stdout: "",
    stderr: "",
    error: undefined,
  };
}

export async function runAdhocScenarios(options = {}) {
  const filter = (options.filter ?? "").toLowerCase();
  const cli = options.cli ?? findCliBinary();
  const wasmPath = options.wasmPath ?? findWasmArtifact();

  const wanted = (name) => {
    if (!filter) return true;
    return name.toLowerCase().includes(filter) || filter === "adhoc";
  };

  const results = [];
  if (wanted("deka-init")) {
    results.push(cli ? await scenarioInit(cli) : skippedResult("deka-init", "deka init writes a project v2 can compile", "no CLI"));
  }
  if (wanted("deka-serve")) {
    results.push(cli ? await scenarioServe(cli) : skippedResult("deka-serve", "deka init && deka serve answers HTTP 200", "no CLI"));
  }
  if (wanted("wasm-io")) {
    results.push(await scenarioWasmIo(wasmPath));
  }
  return { cli, wasmPath, results };
}

export function toHatsCategory(results) {
  return {
    name: "ADHOC",
    tests: results.map((r) => {
      const matched = r.skipped ? false : r.ok;
      const nativeSkipped = Boolean(r.skipped);
      return {
        slug: `adhoc-${r.name}`,
        category: "ADHOC",
        name: r.name,
        title: r.title,
        status: "pass",
        stage: "run",
        hosts: ["native"],
        source: (r.commands ?? []).join("\n") + "\n",
        notes: "ADHOC scenario. Cached commands + stdout. Not a live playground.",
        overallStatus: r.skipped ? "skip" : r.ok ? "pass" : "fail",
        nativeMatches: matched,
        wasmMatches: false,
        nativeResult: {
          ok: r.ok,
          stage: "run",
          stdout: r.stdout ?? "",
          stderr: r.stderr ?? "",
          error: r.error,
          skipped: nativeSkipped,
          skipReason: r.skipReason,
          diagnostics: r.error ? [{ severity: "error", message: r.error }] : [],
        },
        wasmResult: {
          ok: false,
          stage: "run",
          stdout: "",
          stderr: "",
          skipped: true,
          skipReason: "ADHOC is a cached scenario, not the live WASM playground",
          diagnostics: [],
        },
      };
    }),
  };
}
