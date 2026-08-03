#!/usr/bin/env node

"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

const PLATFORM_PACKAGES = Object.freeze({
  "linux-x64-gnu": "@22elix3r/fence-linux-x64-gnu",
  "darwin-x64": "@22elix3r/fence-darwin-x64",
  "darwin-arm64": "@22elix3r/fence-darwin-arm64"
});

function platformTarget(platform = process.platform, arch = process.arch, report = process.report) {
  if (platform === "darwin" && (arch === "x64" || arch === "arm64")) {
    return `darwin-${arch}`;
  }
  if (platform === "linux" && arch === "x64") {
    const header = report && typeof report.getReport === "function"
      ? report.getReport().header
      : undefined;
    if (header && header.glibcVersionRuntime) {
      return "linux-x64-gnu";
    }
    throw new Error("Fence's Linux npm package requires glibc; Alpine/musl is not supported");
  }
  throw new Error(`Fence has no npm binary for ${platform}/${arch}`);
}

function sha256(file) {
  const hash = crypto.createHash("sha256");
  hash.update(fs.readFileSync(file));
  return hash.digest("hex");
}

function verifyBinary(binary, integrityFile, expectedVersion, expectedTarget) {
  const integrity = JSON.parse(fs.readFileSync(integrityFile, "utf8"));
  if (integrity.version !== expectedVersion) {
    throw new Error(
      `Fence package version mismatch: expected ${expectedVersion}, found ${integrity.version}`
    );
  }
  if (integrity.target !== expectedTarget) {
    throw new Error(
      `Fence target mismatch: expected ${expectedTarget}, found ${integrity.target}`
    );
  }
  const actual = sha256(binary);
  if (!/^[0-9a-f]{64}$/.test(integrity.sha256) || actual !== integrity.sha256) {
    throw new Error("Fence binary checksum verification failed; reinstall the package");
  }
}

function resolveInstallation(target, packageName, resolve = require.resolve) {
  let packageJson;
  try {
    packageJson = resolve(`${packageName}/package.json`);
  } catch (error) {
    const detail = error && error.code === "MODULE_NOT_FOUND"
      ? "the platform package is missing (npm may have been run with --omit=optional)"
      : String(error);
    throw new Error(`${detail}; reinstall fence-cli with optional dependencies enabled`);
  }
  const root = path.dirname(packageJson);
  return {
    binary: path.join(root, "bin", "fence"),
    integrity: path.join(root, "integrity.json"),
    target
  };
}

function main() {
  const ownPackage = require("../package.json");
  const target = platformTarget();
  const packageName = PLATFORM_PACKAGES[target];
  const installation = resolveInstallation(target, packageName);
  verifyBinary(installation.binary, installation.integrity, ownPackage.version, target);

  if (typeof process.execve !== "function") {
    throw new Error("Fence requires Node.js 22.15.0 or newer to preserve terminal and signal behavior");
  }
  process.execve(
    installation.binary,
    [installation.binary, ...process.argv.slice(2)],
    process.env
  );
}

if (require.main === module) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`fence: ${error.message}\n`);
    process.stderr.write(
      "Install with Cargo or a verified GitHub archive if this platform is unsupported: " +
      "https://github.com/22elix3r/fence/blob/main/docs/installation.md\n"
    );
    process.exitCode = 1;
  }
}

module.exports = { PLATFORM_PACKAGES, platformTarget, resolveInstallation, sha256, verifyBinary };
