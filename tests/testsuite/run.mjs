#!/usr/bin/env bun
// Native Hats runner. Loads tests/testsuite/<category>/<name>/ and executes
// each fixture with `deka run` (target/release/cli or DEKA_NATIVE).
// Browser/WASM stays the live playground on testsuite.deka.gg; this command
// is the language gate. See deka#292.

import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync, chmodSync, cpSync, copyFileSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import os from "node:os";

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(__dirname, "..", "..");
const testsRoot = __dirname;
const scratchRoot = join(__dirname, ".run-tmp");

const DEFAULT_DEKA_LOCK = '{\n  "lockfileVersion": 1,\n  "packages": {}\n}\n';

const NL = String.fromCharCode(10);

const DEFAULT_DEKA_JSON = {
  name: "conformance-fixture",
  security: {
    allow: {
      read: ["./"],
      write: [".cache"],
    },
    prompt: false,
  },
};

const PACKAGE_DEKA_JSON = {
  name: "conformance-fixture",
  security: {
    allow: {
      read: ["./"],
      write: [".cache", "php_modules", "ds_modules"],
    },
    prompt: false,
  },
};

function fixtureImportsIo(test) {
  const blobs = [test.source, ...Object.values(test.files ?? {})];
  return blobs.some((s) => /\bfrom\s+["']io["']/.test(s));
}

function packagesFor(test) {
  const packages = [...(test.packages ?? [])];
  if (fixtureImportsIo(test) && !packages.some((p) => p === "io" || p === "@deka/io")) {
    packages.push("io");
  }
  return packages;
}

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

function parseStatusFromFilename(filename) {
  if (filename.endsWith(".pass.ds") || filename.endsWith(".pass.dsx")) return "pass";
  if (filename.endsWith(".fail.ds") || filename.endsWith(".fail.dsx")) return "fail";
  return null;
}

function baseNameFromFilename(filename) {
  return filename.replace(/\.(pass|fail)\.dsx?$/, "");
}

function slugFromParts(category, name) {
  return `${category}-${name}`.toLowerCase().replace(/[^a-z0-9]+/g, "-");
}

function collectDsFiles(dir, relativeTo) {
  const results = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    const rel = relative(relativeTo, full).replace(/\\/g, "/");
    if (entry.isDirectory()) {
      results.push(...collectDsFiles(full, relativeTo));
    } else if (entry.isFile() && (entry.name.endsWith(".ds") || entry.name.endsWith(".dsx"))) {
      results.push(rel);
    }
  }
  return results;
}

function parseHosts(raw) {
  if (!Array.isArray(raw)) return ["native", "browser"];
  const hosts = raw.filter((item) => item === "native" || item === "browser");
  return hosts.length > 0 ? hosts : ["native", "browser"];
}

function parsePackages(raw) {
  if (!Array.isArray(raw)) return undefined;
  const names = raw.filter((item) => typeof item === "string" && item.trim().length > 0);
  return names.length > 0 ? names : undefined;
}

function currentCompiler() {
  const value = process.env.DEKA_COMPILER;
  if (typeof value === "string" && value.trim().toLowerCase() === "v1") {
    return "v1";
  }
  return "v2";
}

function parseCompiler(raw) {
  if (raw === "v1" || raw === "v2") return raw;
  return undefined;
}

function readMaybe(dir, filename) {
  const filePath = join(dir, filename);
  if (!existsSync(filePath)) return undefined;
  return readFileSync(filePath, "utf-8");
}

function readMetadata(dir, name) {
  const jsonPath = join(dir, `${name}.json`);
  if (!existsSync(jsonPath)) return {};
  try {
    const raw = JSON.parse(readFileSync(jsonPath, "utf-8"));
    return {
      title: typeof raw.title === "string" ? raw.title : undefined,
      stage: ["parse", "typecheck", "run"].includes(raw.stage) ? raw.stage : undefined,
      hosts: parseHosts(raw.hosts),
      expectedStdoutNative:
        typeof raw.expectedStdoutNative === "string" ? raw.expectedStdoutNative : undefined,
      expectedDiagnosticContains:
        typeof raw.expectedDiagnosticContains === "string" ? raw.expectedDiagnosticContains : undefined,
      dekaJson:
        raw.dekaJson && typeof raw.dekaJson === "object" && !Array.isArray(raw.dekaJson)
          ? raw.dekaJson
          : undefined,
      packages: parsePackages(raw.packages),
      compiler: parseCompiler(raw.compiler),
      notes: typeof raw.notes === "string" ? raw.notes : undefined,
    };
  } catch {
    return {};
  }
}

function loadAllTests() {
  const categories = [];
  for (const categoryEntry of readdirSync(testsRoot, { withFileTypes: true })) {
    if (!categoryEntry.isDirectory()) continue;
    if (categoryEntry.name.startsWith(".")) continue;
    const categoryName = categoryEntry.name;
    const categoryDir = join(testsRoot, categoryName);
    const tests = [];
    for (const testEntry of readdirSync(categoryDir, { withFileTypes: true })) {
      if (!testEntry.isDirectory()) continue;
      const testName = testEntry.name;
      const testDir = join(categoryDir, testName);
      const dsFiles = collectDsFiles(testDir, testDir);
      const entryFile = dsFiles.find((f) => parseStatusFromFilename(f) && !f.includes("/"));
      if (!entryFile) continue;

      const status = parseStatusFromFilename(entryFile);
      const name = baseNameFromFilename(entryFile);
      const source = readMaybe(testDir, entryFile);
      if (source === undefined) continue;

      const metadata = readMetadata(testDir, name);
      const expectedStdout = readMaybe(testDir, `${name}.stdout`);
      const extraDsFiles = dsFiles.filter((f) => f !== entryFile);
      const files =
        extraDsFiles.length > 0
          ? Object.fromEntries(
              extraDsFiles
                .map((f) => [f, readMaybe(testDir, f)])
                .filter(([, content]) => content !== undefined)
            )
          : undefined;

      tests.push({
        slug: slugFromParts(categoryName, testName),
        category: categoryName,
        status,
        name: testName,
        source,
        files,
        entryPath: entryFile,
        title: metadata.title ?? testName.replace(/_/g, " "),
        stage: metadata.stage ?? "run",
        hosts: metadata.hosts ?? ["native", "browser"],
        expectedStdout,
        expectedStdoutNative: metadata.expectedStdoutNative,
        expectedDiagnosticContains: metadata.expectedDiagnosticContains,
        dekaJson: metadata.dekaJson,
        packages: metadata.packages,
        compiler: metadata.compiler,
        notes: metadata.notes,
      });
    }
    if (tests.length > 0) {
      tests.sort((a, b) => a.name.localeCompare(b.name));
      categories.push({ name: categoryName, tests });
    }
  }
  categories.sort((a, b) => a.name.localeCompare(b.name));
  return categories;
}

function parseNativeDiagnostics(stderr) {
  const diagnostics = [];
  const lines = stderr.split("\n");
  let line;
  let column;

  for (const current of lines) {
    const headerMatch = current.match(/^┌─\s+\S+:(\d+):(\d+)\s*$/);
    if (headerMatch) {
      line = Number(headerMatch[1]);
      column = Number(headerMatch[2]);
      continue;
    }
    const messageMatch = current.match(/\^\s+(.+)$/);
    if (messageMatch) {
      const message = messageMatch[1].trim();
      if (message) {
        diagnostics.push({ severity: "error", message, line, column });
      }
      line = undefined;
      column = undefined;
    }
  }

  if (diagnostics.length === 0) {
    const firstLine = lines.find((l) => {
      const trimmed = l.trim();
      return (
        trimmed.length > 0 &&
        !trimmed.startsWith("[") &&
        !trimmed.startsWith("Validation") &&
        !trimmed.startsWith("❌")
      );
    });
    if (firstLine) {
      diagnostics.push({ severity: "error", message: firstLine.trim() });
    }
  }

  return diagnostics;
}

function writeProjectFiles(tmpDir, entryPath, source, files) {
  const isProject = files && Object.keys(files).length > 0;
  if (!isProject) {
    const ext = String(entryPath || "test.ds").endsWith(".dsx") ? ".dsx" : ".ds";
    writeFileSync(join(tmpDir, `test${ext}`), source);
    return { isProject: false, ext };
  }
  mkdirSync(dirname(join(tmpDir, entryPath)), { recursive: true });
  writeFileSync(join(tmpDir, entryPath), source);
  for (const [filePath, content] of Object.entries(files)) {
    const fullPath = join(tmpDir, filePath);
    mkdirSync(dirname(fullPath), { recursive: true });
    writeFileSync(fullPath, content);
  }
  return { isProject: true, ext: ".ds" };
}

// A cache hit restores ds_modules/ and deka.lock but not the manifest, so the
// fixture ended up with packages installed and never declared -- exactly the
// shape the project gate rejects (deka#403, deka#430). Derive the dependency
// block from what was actually restored, so it does not matter which runner
// filled the shared cache.
function declareRestoredModules(tmpDir) {
  const manifestPath = join(tmpDir, "deka.json");
  const manifest = existsSync(manifestPath)
    ? JSON.parse(readFileSync(manifestPath, "utf-8"))
    : {};
  const deps = { ...(manifest.dependencies ?? {}) };
  for (const modulesDir of ["ds_modules", "php_modules"]) {
    const scope = join(tmpDir, modulesDir, "@deka");
    if (!existsSync(scope)) continue;
    for (const name of readdirSync(scope)) {
      const pkg = "@deka/" + name;
      if (deps[pkg]) continue;
      const pkgManifest = join(scope, name, "deka.json");
      let version = "*";
      if (existsSync(pkgManifest)) {
        try {
          version = JSON.parse(readFileSync(pkgManifest, "utf-8")).version ?? "*";
        } catch {
          version = "*";
        }
      }
      deps[pkg] = version;
    }
  }
  manifest.dependencies = deps;
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + NL);
}

function restoreCachedModules(cacheDir, tmpDir) {
  for (const name of ["ds_modules", "php_modules"]) {
    const cached = join(cacheDir, name);
    if (existsSync(cached)) {
      cpSync(cached, join(tmpDir, name), { recursive: true });
    }
  }
}

function installPackages(cliPath, tmpDir, packages) {
  const cacheKey = packages.slice().sort().join("+");
  const cacheDir = join(repoRoot, ".cache", "deka-packages", cacheKey);
  const cachedLock = join(cacheDir, "deka.lock");
  const hasCachedModules =
    existsSync(join(cacheDir, "ds_modules")) || existsSync(join(cacheDir, "php_modules"));

  if (existsSync(cachedLock) && hasCachedModules) {
    restoreCachedModules(cacheDir, tmpDir);
    copyFileSync(cachedLock, join(tmpDir, "deka.lock"));
    declareRestoredModules(tmpDir);
    return { ok: true, stderr: "" };
  }

  const spawned = spawnSync(cliPath, ["add", ...packages, "--yes"], {
    cwd: tmpDir,
    encoding: "utf-8",
    timeout: 120000,
    env: { ...process.env, DEKA_SECURITY_NO_PROMPT: "1" },
  });
  const stderr = spawned.stderr ?? "";
  if (spawned.status !== 0 || spawned.error) {
    return {
      ok: false,
      error:
        spawned.error?.message ??
        stderr
          .split("\n")
          .map((line) => line.trim())
          .find((line) => line.length > 0) ??
        `deka add ${packages.join(" ")} failed`,
      stderr,
    };
  }

  mkdirSync(cacheDir, { recursive: true });
  for (const name of ["ds_modules", "php_modules"]) {
    const dir = join(tmpDir, name);
    if (existsSync(dir)) {
      cpSync(dir, join(cacheDir, name), { recursive: true });
    }
  }
  const lockPath = join(tmpDir, "deka.lock");
  if (existsSync(lockPath)) {
    copyFileSync(lockPath, cachedLock);
  }
  return { ok: true, stderr };
}


function runNative(cliPath, test) {
  mkdirSync(scratchRoot, { recursive: true });
  const tmpDir = mkdtempSync(join(scratchRoot, "case-"));
  chmodSync(tmpDir, 0o700);
  const packages = packagesFor(test);

  try {
    const { isProject, ext } = writeProjectFiles(tmpDir, test.entryPath ?? "test.ds", test.source, test.files);
    writeFileSync(join(tmpDir, "deka.lock"), DEFAULT_DEKA_LOCK);
    const dekaJson = test.dekaJson ?? (packages.length > 0 ? PACKAGE_DEKA_JSON : DEFAULT_DEKA_JSON);
    writeFileSync(join(tmpDir, "deka.json"), JSON.stringify(dekaJson, null, 2) + "\n");

    if (packages.length > 0) {
      const installed = installPackages(cliPath, tmpDir, packages);
      if (!installed.ok) {
        return {
          ok: false,
          stdout: "",
          stderr: installed.stderr,
          error: installed.error,
          transpileFailed: true,
          diagnostics: installed.error ? [{ severity: "error", message: installed.error }] : [],
        };
      }
    }

    const entryRel = isProject ? `./${test.entryPath ?? "main.ds"}` : `./test${ext ?? ".ds"}`;
    const spawned = spawnSync(cliPath, ["run", entryRel], {
      cwd: tmpDir,
      encoding: "utf-8",
      timeout: 30000,
      env: { ...process.env, DEKA_SECURITY_NO_PROMPT: "1" },
    });

    const stdout = spawned.stdout ?? "";
    const rawStderr = spawned.stderr ?? "";
    const stderr = rawStderr
      .split("\n")
      .filter((line) => !line.startsWith("[security]"))
      .join("\n");
    const failed = spawned.status !== 0 || spawned.error !== undefined;
    const ranInIsolate = rawStderr.includes("Run failed:") || stdout.length > 0;
    const diagnostics = failed ? parseNativeDiagnostics(stderr || rawStderr) : [];
    const firstError =
      diagnostics[0]?.message ??
      stderr
        .split("\n")
        .map((line) => line.trim())
        .find((line) => line.length > 0) ??
      spawned.error?.message ??
      (failed ? "deka run failed" : undefined);

    return {
      ok: !failed,
      stdout,
      stderr,
      error: failed ? firstError : undefined,
      transpileFailed: failed && !ranInIsolate,
      diagnostics,
    };
  } finally {
    try {
      rmSync(tmpDir, { recursive: true, force: true });
    } catch {}
  }
}

function nativeStage(result) {
  if (result.ok) return "run";
  return result.transpileFailed ? "parse" : "run";
}

function stageMatches(expected, actual) {
  if (expected === actual) return true;
  // Native isolate does not distinguish typecheck from parse on compile failure.
  if (
    (expected === "parse" || expected === "typecheck") &&
    (actual === "parse" || actual === "typecheck")
  ) {
    return true;
  }
  return false;
}

function expectedStdoutForNative(test) {
  if (test.expectedStdoutNative !== undefined) return test.expectedStdoutNative;
  return test.expectedStdout;
}

function evaluate(test, result) {
  const stage = nativeStage(result);
  const reasons = [];
  const actualStatus = result.ok ? "pass" : "fail";
  if (actualStatus !== test.status) {
    reasons.push(`status: want ${test.status}, got ${actualStatus}${result.error ? ` (${result.error})` : ""}`);
  }
  if (!stageMatches(test.stage, stage)) {
    reasons.push(`stage: want ${test.stage}, got ${stage}`);
  }
  const expectedStdout = expectedStdoutForNative(test);
  if (expectedStdout !== undefined && result.stdout !== expectedStdout) {
    reasons.push(
      `stdout:\n  expected: ${JSON.stringify(expectedStdout)}\n  actual:   ${JSON.stringify(result.stdout)}`
    );
  }
  if (test.expectedDiagnosticContains) {
    const needle = test.expectedDiagnosticContains.toLowerCase();
    const hay = [
      ...result.diagnostics.map((d) => d.message),
      result.stderr,
      result.error ?? "",
    ]
      .join("\n")
      .toLowerCase();
    if (!hay.includes(needle)) {
      reasons.push(
        `diagnostic: expected to contain ${JSON.stringify(test.expectedDiagnosticContains)}, got ${JSON.stringify(result.error ?? result.stderr)}`
      );
    }
  }
  return { matched: reasons.length === 0, stage, reasons };
}

function parseArgs(argv) {
  const args = {
    list: false,
    json: false,
    filter: null,
    help: false,
    jobs: Math.min(8, os.availableParallelism?.() || 4),
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--list" || arg === "-l") args.list = true;
    else if (arg === "--json") args.json = true;
    else if (arg === "--filter" || arg === "-f") args.filter = argv[++i] || "";
    else if (arg === "--jobs" || arg === "-j") args.jobs = Number(argv[++i] || args.jobs);
    else if (arg === "--help" || arg === "-h") args.help = true;
  }
  return args;
}

function printUsage() {
  console.log(`usage: bun tests/testsuite/run.mjs [options]

options:
  -l, --list                 List all fixtures and exit
  --json                     Output a JSON array of per-fixture results
  -f, --filter <substr>      Run only fixtures whose slug or title matches
  -j, --jobs <n>             Parallel native runs (default: min(8, CPUs))
  -h, --help                 Show this help

Native isolate only (\`deka run\`). Uses target/release/cli or DEKA_NATIVE.
Browser/WASM is the live playground on testsuite.deka.gg.

examples:
  cargo build --release -p cli
  bun tests/testsuite/run.mjs
  bun tests/testsuite/run.mjs --filter json
  DEKA_COMPILER=v2 bun tests/testsuite/run.mjs --json`);
}

async function mapPool(items, limit, fn) {
  const ret = new Array(items.length);
  let next = 0;
  async function worker() {
    while (true) {
      const idx = next++;
      if (idx >= items.length) return;
      ret[idx] = await fn(items[idx], idx);
    }
  }
  const n = Math.max(1, Math.min(limit, items.length));
  await Promise.all(Array.from({ length: n }, () => worker()));
  return ret;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    printUsage();
    process.exit(0);
  }

  const categories = loadAllTests();
  const all = categories.flatMap((c) => c.tests);

  if (args.list) {
    for (const test of all) {
      const hosts = test.hosts.join(",");
      console.log(`  ${test.slug}  ${test.status}@${test.stage}  [${hosts}]  ${test.title}`);
    }
    console.log(`\nTotal: ${all.length}`);
    process.exit(0);
  }

  const filtered = args.filter
    ? all.filter((t) => {
        const needle = args.filter.toLowerCase();
        return (
          t.slug.toLowerCase().includes(needle) ||
          t.title.toLowerCase().includes(needle) ||
          t.name.toLowerCase().includes(needle) ||
          t.category.toLowerCase().includes(needle)
        );
      })
    : all;

  if (filtered.length === 0) {
    console.error(`error: no fixtures match filter "${args.filter}"`);
    process.exit(1);
  }

  const cliBinary = findCliBinary();
  if (!cliBinary) {
    console.error("error: could not find deka CLI (build with: cargo build --release -p cli)");
    process.exit(1);
  }

  if (!args.json) {
    console.log(`native CLI: ${cliBinary}`);
    console.log(`fixtures: ${filtered.length}  jobs: ${args.jobs}`);
  }

  const activeCompiler = currentCompiler();

  const results = await mapPool(filtered, args.jobs, async (test) => {
    if (!test.hosts.includes("native")) {
      return { test, skipped: true, reason: "hosts does not include native" };
    }
    if (test.packages && test.packages.length > 0) {
      return { test, skipped: true, reason: "index packages are exercised by the dump, not the language gate" };
    }
    if (test.compiler && test.compiler !== activeCompiler) {
      return { test, skipped: true, reason: `compiler mismatch: fixture requires ${test.compiler}, running ${activeCompiler}` };
    }
    const native = runNative(cliBinary, test);
    const evaled = evaluate(test, native);
    return { test, skipped: false, native, ...evaled };
  });

  let passed = 0;
  let failed = 0;
  let skipped = 0;

  if (args.json) {
    const output = results.map((r) => ({
      slug: r.test.slug,
      category: r.test.category,
      name: r.test.name,
      expectedStatus: r.test.status,
      expectedStage: r.test.stage,
      skipped: r.skipped ?? false,
      skipReason: r.skipped ? r.reason : undefined,
      matched: r.matched ?? false,
      actualStatus: r.native ? (r.native.ok ? "pass" : "fail") : undefined,
      actualStage: r.stage,
      stdout: r.native?.stdout,
      stderr: r.native?.stderr,
      error: r.native?.error,
      reasons: r.reasons,
    }));
    console.log(JSON.stringify(output, null, 2));
    process.exit(0);
  }

  console.log("");
  for (const result of results) {
    if (result.skipped) {
      skipped++;
      continue;
    }
    if (result.matched) {
      passed++;
      continue;
    }
    failed++;
    console.log(`✗ ${result.test.slug}`);
    for (const reason of result.reasons) {
      console.log(`    ${reason}`);
    }
  }

  console.log("\n============================================================");
  console.log(
    ` Passed: ${passed} | Failed: ${failed} | Skipped: ${skipped} | Total: ${filtered.length}`
  );
  console.log("============================================================\n");

  process.exit(failed === 0 ? 0 : 1);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
