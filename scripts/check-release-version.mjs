#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const cargo = fs.readFileSync(path.join(repositoryRoot, "Cargo.toml"), "utf8");
const changelog = fs.readFileSync(path.join(repositoryRoot, "CHANGELOG.md"), "utf8");

const versionMatch = cargo.match(/^version = "([^"]+)"$/m);
if (!versionMatch) {
  throw new Error("workspace package version is missing from Cargo.toml");
}
const version = versionMatch[1];
if (!/^0\.1\.0-alpha\.[1-9][0-9]*$/.test(version)) {
  throw new Error(`release version must be 0.1.0-alpha.N, found ${version}`);
}

const internalCrates = [
  "fence-unix",
  "fence-windows",
  "fence-core",
  "fence-git",
  "fence-session",
  "fence-tui"
];
for (const crate of internalCrates) {
  const escaped = crate.replaceAll("-", "\\-");
  const requirement = new RegExp(`^${escaped} = \\{ version = "=([^\"]+)"`, "m").exec(cargo);
  if (!requirement || requirement[1] !== version) {
    throw new Error(`${crate} must use the exact workspace version =${version}`);
  }
}

const packagePaths = [
  "npm/package.json",
  "npm/fence-cli/package.json",
  "npm/platform/linux-x64-gnu/package.json",
  "npm/platform/darwin-x64/package.json",
  "npm/platform/darwin-arm64/package.json"
];
const packages = packagePaths.map((relative) => {
  const manifest = JSON.parse(fs.readFileSync(path.join(repositoryRoot, relative), "utf8"));
  if (manifest.version !== version) {
    throw new Error(`${relative} has version ${manifest.version}, expected ${version}`);
  }
  return [relative, manifest];
});

const npmLock = JSON.parse(
  fs.readFileSync(path.join(repositoryRoot, "npm/package-lock.json"), "utf8")
);
if (npmLock.version !== version || npmLock.packages?.[""]?.version !== version) {
  throw new Error(`npm/package-lock.json must use version ${version}`);
}

const meta = packages.find(([relative]) => relative === "npm/fence-cli/package.json")[1];
const expectedLeaves = new Map(packages.slice(2).map(([, manifest]) => [manifest.name, manifest]));
for (const [name, requirement] of Object.entries(meta.optionalDependencies)) {
  if (!expectedLeaves.has(name)) {
    throw new Error(`unexpected npm platform dependency ${name}`);
  }
  if (requirement !== version) {
    throw new Error(`${name} must be pinned to exact version ${version}`);
  }
  expectedLeaves.delete(name);
}
if (expectedLeaves.size !== 0) {
  throw new Error(`npm meta package is missing platform leaves: ${[...expectedLeaves.keys()]}`);
}

for (const [relative, manifest] of packages) {
  for (const script of ["preinstall", "install", "postinstall", "prepare"]) {
    if (manifest.scripts && Object.hasOwn(manifest.scripts, script)) {
      throw new Error(`${relative} must not define the ${script} lifecycle script`);
    }
  }
}

const heading = `## [${version}]`;
if (changelog.split(heading).length !== 2) {
  throw new Error(`CHANGELOG.md must contain exactly one ${heading} heading`);
}

const ref = process.env.GITHUB_REF_NAME;
if (ref && ref.startsWith("v") && ref !== `v${version}`) {
  throw new Error(`tag ${ref} does not match v${version}`);
}

process.stdout.write(`release metadata agrees on ${version}\n`);
