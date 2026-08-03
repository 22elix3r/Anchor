#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

if (process.argv.length !== 5) {
  throw new Error("usage: stage-npm-packages.mjs ASSET-DIR NOTICE-FILE OUTPUT-DIR");
}

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const assetDirectory = path.resolve(process.argv[2]);
const noticeFile = path.resolve(process.argv[3]);
const outputDirectory = path.resolve(process.argv[4]);
const version = JSON.parse(
  fs.readFileSync(path.join(repositoryRoot, "npm/fence-cli/package.json"), "utf8")
).version;

const targets = [
  ["linux-x64-gnu", "x86_64-unknown-linux-gnu"],
  ["darwin-x64", "x86_64-apple-darwin"],
  ["darwin-arm64", "aarch64-apple-darwin"]
];

fs.rmSync(outputDirectory, { recursive: true, force: true });
fs.mkdirSync(outputDirectory, { recursive: true });

function copyDistributionFiles(destination) {
  fs.copyFileSync(path.join(repositoryRoot, "README.md"), path.join(destination, "README.md"));
  fs.copyFileSync(path.join(repositoryRoot, "LICENSE-MIT"), path.join(destination, "LICENSE-MIT"));
  fs.copyFileSync(
    path.join(repositoryRoot, "LICENSE-APACHE"),
    path.join(destination, "LICENSE-APACHE")
  );
  fs.copyFileSync(noticeFile, path.join(destination, "THIRD_PARTY_LICENSES"));
}

for (const [npmTarget, rustTarget] of targets) {
  const destination = path.join(outputDirectory, npmTarget);
  fs.mkdirSync(path.join(destination, "bin"), { recursive: true });
  fs.copyFileSync(
    path.join(repositoryRoot, "npm/platform", npmTarget, "package.json"),
    path.join(destination, "package.json")
  );
  fs.copyFileSync(
    path.join(repositoryRoot, "npm/platform/README.md"),
    path.join(destination, "README.md")
  );
  copyDistributionFiles(destination);

  const sourceBinary = path.join(assetDirectory, `fence-${version}-${rustTarget}`);
  const binary = path.join(destination, "bin/fence");
  fs.copyFileSync(sourceBinary, binary);
  fs.chmodSync(binary, 0o755);
  const sha256 = crypto.createHash("sha256").update(fs.readFileSync(binary)).digest("hex");
  fs.writeFileSync(
    path.join(destination, "integrity.json"),
    `${JSON.stringify({ version, target: npmTarget, sha256 }, null, 2)}\n`
  );
}

const meta = path.join(outputDirectory, "fence-cli");
fs.cpSync(path.join(repositoryRoot, "npm/fence-cli"), meta, {
  recursive: true,
  filter: (source) => !source.includes(`${path.sep}test${path.sep}`) && !source.endsWith(`${path.sep}test`)
});
copyDistributionFiles(meta);

process.stdout.write(`staged npm ${version} packages in ${outputDirectory}\n`);
