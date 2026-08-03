"use strict";

const assert = require("node:assert/strict");
const childProcess = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const {
  PLATFORM_PACKAGES,
  platformTarget,
  resolveInstallation,
  sha256,
  verifyBinary
} = require("../bin/fence.js");

test("maps only supported release targets", () => {
  assert.equal(platformTarget("darwin", "x64"), "darwin-x64");
  assert.equal(platformTarget("darwin", "arm64"), "darwin-arm64");
  assert.equal(
    platformTarget("linux", "x64", { getReport: () => ({ header: { glibcVersionRuntime: "2.35" } }) }),
    "linux-x64-gnu"
  );
  assert.throws(() => platformTarget("win32", "x64"), /no npm binary/);
  assert.throws(
    () => platformTarget("linux", "x64", { getReport: () => ({ header: {} }) }),
    /requires glibc/
  );
  assert.deepEqual(Object.keys(PLATFORM_PACKAGES).sort(), [
    "darwin-arm64",
    "darwin-x64",
    "linux-x64-gnu"
  ]);
});

test("verifies version, target, and binary digest", (context) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "fence-launcher-test-"));
  context.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const binary = path.join(root, "fence");
  const integrity = path.join(root, "integrity.json");
  fs.writeFileSync(binary, "test binary");
  fs.writeFileSync(integrity, JSON.stringify({
    version: "0.1.0-alpha.1",
    target: "linux-x64-gnu",
    sha256: sha256(binary)
  }));

  assert.doesNotThrow(() => verifyBinary(
    binary,
    integrity,
    "0.1.0-alpha.1",
    "linux-x64-gnu"
  ));
  fs.appendFileSync(binary, "corrupt");
  assert.throws(
    () => verifyBinary(binary, integrity, "0.1.0-alpha.1", "linux-x64-gnu"),
    /checksum verification failed/
  );
});

test("resolves a leaf by absolute package path and explains an omitted leaf", () => {
  const result = resolveInstallation(
    "darwin-arm64",
    "@22elix3r/fence-darwin-arm64",
    () => "/prefix/node_modules/@22elix3r/fence-darwin-arm64/package.json"
  );
  assert.equal(
    result.binary,
    "/prefix/node_modules/@22elix3r/fence-darwin-arm64/bin/fence"
  );
  const missing = Object.assign(new Error("missing"), { code: "MODULE_NOT_FOUND" });
  assert.throws(
    () => resolveInstallation("darwin-arm64", "missing", () => { throw missing; }),
    /--omit=optional/
  );
});

test("replaces itself and preserves child exit and signal behavior", async (context) => {
  if (
    process.platform !== "linux" ||
    process.arch !== "x64" ||
    typeof process.execve !== "function"
  ) {
    context.skip("the integration fixture exercises the GNU/Linux x64 execve launcher");
    return;
  }

  const root = fs.mkdtempSync(path.join(os.tmpdir(), "fence-launcher-exec-test-"));
  context.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const meta = path.join(root, "node_modules/fence-cli");
  const leaf = path.join(root, "node_modules/@22elix3r/fence-linux-x64-gnu");
  fs.mkdirSync(path.join(meta, "bin"), { recursive: true });
  fs.mkdirSync(path.join(leaf, "bin"), { recursive: true });
  fs.copyFileSync(path.join(__dirname, "../bin/fence.js"), path.join(meta, "bin/fence.js"));
  fs.writeFileSync(
    path.join(meta, "package.json"),
    JSON.stringify({ name: "fence-cli", version: "0.1.0-alpha.1" })
  );
  fs.writeFileSync(
    path.join(leaf, "package.json"),
    JSON.stringify({ name: "@22elix3r/fence-linux-x64-gnu", version: "0.1.0-alpha.1" })
  );
  const binary = path.join(leaf, "bin/fence");
  fs.writeFileSync(binary, "#!/bin/sh\nexit \"$2\"\n", { mode: 0o755 });
  fs.writeFileSync(
    path.join(leaf, "integrity.json"),
    JSON.stringify({
      version: "0.1.0-alpha.1",
      target: "linux-x64-gnu",
      sha256: sha256(binary)
    })
  );

  const result = childProcess.spawnSync(
    process.execPath,
    [path.join(meta, "bin/fence.js"), "exit", "7"],
    {
      env: { ...process.env, PATH: "/path-containing-no-fence" },
      encoding: "utf8"
    }
  );
  assert.equal(result.status, 7, result.stderr);
  assert.equal(result.signal, null);

  fs.writeFileSync(binary, "#!/bin/sh\necho ready\nwhile :; do :; done\n", { mode: 0o755 });
  fs.writeFileSync(
    path.join(leaf, "integrity.json"),
    JSON.stringify({
      version: "0.1.0-alpha.1",
      target: "linux-x64-gnu",
      sha256: sha256(binary)
    })
  );
  const waiting = childProcess.spawn(
    process.execPath,
    [path.join(meta, "bin/fence.js"), "wait"],
    { env: { ...process.env, PATH: "/path-containing-no-fence" } }
  );
  const signal = await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      waiting.kill("SIGKILL");
      reject(new Error("timed out waiting for the execve fixture"));
    }, 5000);
    waiting.once("error", reject);
    waiting.stdout.once("data", () => waiting.kill("SIGTERM"));
    waiting.once("close", (_code, closedSignal) => {
      clearTimeout(timeout);
      resolve(closedSignal);
    });
  });
  assert.equal(signal, "SIGTERM");
});
