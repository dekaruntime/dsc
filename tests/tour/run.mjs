#!/usr/bin/env bun
// Compile every tests/tour lesson with the local CLI. Match by id in
// manifest.json, never by display name. See deka#292.

import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync, cpSync, copyFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(__dirname, "..", "..");
const manifestPath = join(__dirname, "manifest.json");
const scratchDir = join(__dirname, ".run-tmp");

const DEFAULT_DEKA_LOCK = '{\n  "lockfileVersion": 1,\n  "packages": {}\n}\n';

const PACKAGE_DEKA_JSON = {
  name: "tour-lesson",
  security: {
    allow: {
      read: ["./"],
      write: [".cache", "php_modules", "ds_modules"],
    },
    prompt: false,
  },
};

const NL = String.fromCharCode(10);

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

function installIo(cliPath, tmpDir) {
  const cacheDir = join(repoRoot, ".cache", "deka-packages", "io");
  const cachedLock = join(cacheDir, "deka.lock");
  const hasCachedModules =
    existsSync(join(cacheDir, "ds_modules")) || existsSync(join(cacheDir, "php_modules"));

  writeFileSync(join(tmpDir, "deka.lock"), DEFAULT_DEKA_LOCK);
  writeFileSync(join(tmpDir, "deka.json"), JSON.stringify(PACKAGE_DEKA_JSON, null, 2) + "\n");

  if (existsSync(cachedLock) && hasCachedModules) {
    restoreCachedModules(cacheDir, tmpDir);
    copyFileSync(cachedLock, join(tmpDir, "deka.lock"));
    declareRestoredModules(tmpDir);
    return { ok: true, stderr: "" };
  }

  const spawned = spawnSync(cliPath, ["add", "io", "--yes"], {
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
        "deka add io failed",
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

function parseArgs(argv) {
  const args = { list: false, filter: null, help: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--list" || arg === "-l") args.list = true;
    else if (arg === "--filter" || arg === "-f") args.filter = argv[++i] || "";
    else if (arg === "--help" || arg === "-h") args.help = true;
  }
  return args;
}

function printUsage() {
  console.log(`usage: bun tests/tour/run.mjs [options]

options:
  -l, --list            List all lessons and exit
  -f, --filter <substr> Run only lessons whose id or title matches
  -h, --help            Show this help`);
}

function compileLesson(cliBinary, projectDir, sourcePath, outPath) {
  const ext = sourcePath.endsWith(".dsx") ? ".dsx" : ".ds";
  copyFileSync(sourcePath, join(projectDir, `lesson${ext}`));
  const result = spawnSync(cliBinary, ["transpile", `./lesson${ext}`, "--out", outPath], {
    cwd: projectDir,
    encoding: "utf-8",
    timeout: 30_000,
    env: { ...process.env, DEKA_SECURITY_NO_PROMPT: "1" },
  });
  const combined = `${result.stderr ?? ""}\n${result.stdout ?? ""}`;
  return {
    ok: result.status === 0,
    output: combined,
  };
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    printUsage();
    process.exit(0);
  }

  const manifest = JSON.parse(readFileSync(manifestPath, "utf-8"));
  const dsFiles = readdirSync(__dirname).filter((name) => name.endsWith(".ds") || name.endsWith(".dsx"));
  const manifestIds = new Set(manifest.map((lesson) => lesson.id));
  const fileIds = new Set(dsFiles.map((name) => name.replace(/\.dsx$/, "").replace(/\.ds$/, "")));

  const missingFiles = [...manifestIds].filter((id) => !fileIds.has(id));
  const orphanFiles = [...fileIds].filter((id) => !manifestIds.has(id));
  if (missingFiles.length > 0 || orphanFiles.length > 0) {
    if (missingFiles.length > 0) {
      console.error(`error: manifest ids with no .ds file: ${missingFiles.join(", ")}`);
    }
    if (orphanFiles.length > 0) {
      console.error(`error: .ds files not in manifest.json: ${orphanFiles.join(", ")}`);
    }
    process.exit(1);
  }

  if (args.list) {
    for (const lesson of manifest) {
      console.log(`  ${lesson.id}  compile=${lesson.expectCompile}  ${lesson.title}`);
    }
    console.log(`\nTotal: ${manifest.length}`);
    process.exit(0);
  }

  const filtered = args.filter
    ? manifest.filter((lesson) => {
        const needle = args.filter.toLowerCase();
        return lesson.id.toLowerCase().includes(needle) || lesson.title.toLowerCase().includes(needle);
      })
    : manifest;

  if (filtered.length === 0) {
    console.error(`error: no lessons match filter "${args.filter}"`);
    process.exit(1);
  }

  const cliBinary = findCliBinary();
  if (!cliBinary) {
    console.error("error: could not find deka CLI (build with: cargo build --release -p cli)");
    process.exit(1);
  }

  mkdirSync(scratchDir, { recursive: true });
  const projectDir = join(scratchDir, "project");
  mkdirSync(projectDir, { recursive: true });
  const installed = installIo(cliBinary, projectDir);
  if (!installed.ok) {
    console.error(`error: could not install io: ${installed.error ?? installed.stderr}`);
    process.exit(1);
  }

  let passed = 0;
  let failed = 0;

  console.log(`native CLI: ${cliBinary}`);
  console.log(`lessons: ${filtered.length}\n`);

  try {
    for (const lesson of filtered) {
      const sourcePath = existsSync(join(__dirname, `${lesson.id}.dsx`))
        ? join(__dirname, `${lesson.id}.dsx`)
        : join(__dirname, `${lesson.id}.ds`);
      const outPath = join(scratchDir, `${lesson.id}.js`);
      const compiled = compileLesson(cliBinary, projectDir, sourcePath, outPath);
      try {
        rmSync(outPath, { force: true });
      } catch {}

      const reasons = [];
      if (compiled.ok !== lesson.expectCompile) {
        reasons.push(
          lesson.expectCompile
            ? `expected compile success, got:\n${compiled.output}`
            : "expected compile failure, but transpile succeeded"
        );
      } else if (!lesson.expectCompile && lesson.expectError) {
        if (!compiled.output.includes(lesson.expectError)) {
          reasons.push(
            `expected diagnostic containing ${JSON.stringify(lesson.expectError)}, got:\n${compiled.output}`
          );
        }
      }

      if (reasons.length === 0) {
        passed++;
        console.log(`✓ ${lesson.id}`);
      } else {
        failed++;
        console.log(`✗ ${lesson.id}`);
        for (const reason of reasons) {
          console.log(`    ${reason}`);
        }
      }
    }
  } finally {
    try {
      rmSync(scratchDir, { recursive: true, force: true });
    } catch {}
  }

  console.log("\n============================================================");
  console.log(` Passed: ${passed} | Failed: ${failed} | Total: ${filtered.length}`);
  console.log("============================================================\n");
  process.exit(failed === 0 ? 0 : 1);
}

main();
