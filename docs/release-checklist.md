# Public alpha release checklist

Fence mutates source trees, and registry versions cannot be replaced. The
release owner records every checked item in the release pull request. A tag is
authorization to build, not permission to skip a failed gate.

## One-time registry and repository setup

- [ ] The maintainer controls the seven crates.io names, `fence-cli` on npm,
  and the public `@22elix3r` npm scope with phishing-resistant 2FA enabled.
- [ ] First versions use short-lived least-privilege bootstrap tokens. Tokens
  are revoked after crates.io and npm trusted publishers are restricted to this
  repository, release workflow, and `release` environment.
- [ ] The `release` GitHub environment requires an explicit reviewer and
  permits only protected `v*` tags. Enable self-review prevention as soon as a
  second trusted maintainer can approve releases; a single-maintainer
  repository must record that temporary exception in the release PR.
- [ ] `v*` tags are immutable and signed. Release administrators cannot bypass
  the approval or tag policy in routine use.
- [ ] The release maintainer has a protected GPG or SSH signing key registered
  with GitHub and has verified a disposable signed tag outside the `v*`
  namespace before creating the immutable release tag.
- [ ] Registry recovery contacts and account recovery codes are stored outside
  the repository.

## Source and compatibility

- [ ] `node scripts/check-release-version.mjs` confirms the exact
  `0.1.0-alpha.N` across Cargo, npm, internal dependencies, tag, and changelog.
- [ ] The release commit is exactly protected `main` and has a successful CI
  push run.
- [ ] The changelog describes safety boundaries, refusals, schema changes,
  supported targets, and operator actions. It does not claim process
  authorship or Windows mutation.
- [ ] JSON golden tests cover the schema-1 `schema/operation/status/data`
  envelope and documented exit statuses.
- [ ] New persisted-data writers emit only documented schemas. Old fixtures
  load into conservative states; future and insufficient records refuse
  mutation.
- [ ] A downgrade is neither required nor recommended. If the release writes a
  new schema, notes say which older binaries will refuse it.
- [ ] All third-party Actions are pinned by full commit SHA.

## Required verification

Run on a clean checkout of the exact tag:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo +1.85 test --workspace --locked
cargo deny check advisories bans licenses sources
node scripts/check-release-version.mjs
node --test npm/fence-cli/test/*.test.js
```

- [ ] Stable tests pass on Linux, macOS, and Windows; MSRV 1.85 passes.
- [ ] The scheduled fuzz smoke is green for every target, or each target ran
  manually for at least 30 seconds.
- [ ] Real-process crash and formal restoration matrices pass.
- [ ] `scripts/release-smoke.sh` passes against every supported packaged binary
  with a PATH that cannot contain `git`.
- [ ] Released Linux bytes pass Ubuntu 22.04/24.04, pinned Fedora, and pinned
  Arch smoke tests. Intel and ARM macOS bytes pass on native runners.
- [ ] PTY review, SIGINT/SIGTERM, child exit propagation, interrupted install,
  missing optional package, corrupt binary, weak permission, and unwritable
  prefix/store tests pass.
- [ ] No recovery journal remains after end-to-end smoke tests.

## Cargo packages

For each crate, inspect `cargo package --list --locked`. Then dry-run and publish
one at a time, waiting for registry/index visibility before its consumer:

1. `fence-unix`
2. `fence-windows`
3. `fence-core`
4. `fence-git`
5. `fence-session`
6. `fence-tui`
7. `fence-cli`

- [ ] Every archive contains only declared sources/tests, README, generated
  Cargo files, and required license metadata.
- [ ] All internal requirements are exact `=0.1.0-alpha.N` requirements.
- [ ] Native docs and the macOS/Windows docs.rs target builds pass.
- [ ] After publication, a clean registry-only
  `cargo install fence-cli --version <version> --locked` works on MSRV and the
  pinned release toolchain.

## GitHub and npm artifacts

- [ ] Native jobs produce byte-identical raw binaries for the GitHub archive
  and matching npm leaf.
- [ ] Archives have normalized relative entries and contain only the binary,
  licenses/notices, changelog, user documentation, and target SBOM.
- [ ] The central SHA-256 manifest verifies every archive and raw binary.
- [ ] `gh attestation verify` succeeds for raw binaries, archives, checksums,
  SBOMs, and npm tarballs.
- [ ] `npm pack --dry-run --json` and packed-tar inspection show no secrets,
  development fixtures, or lifecycle scripts.
- [ ] Platform leaves publish first under `alpha`; clean registry installs
  validate their exact bytes.
- [ ] `fence-cli` publishes under `alpha`; global installs pass with
  `--ignore-scripts`, Node 22.15, and current Node 24.
- [ ] The npm launcher rejects omitted/corrupt/unsupported leaves, ignores a
  poisoned PATH, and preserves TTY, signals, and child exit status.
- [ ] Only after all registry tests pass is the exact npm version promoted to
  `latest` and the draft GitHub release published as a prerelease.

## Documentation and announcement

- [ ] README install commands use `fence-cli`, while examples invoke `fence`.
- [ ] Installation, quickstart, first recovery, architecture, security,
  troubleshooting, platforms, migration, configuration, storage, and safety
  links work from the packaged documentation.
- [ ] Release notes clearly exclude Windows binaries/npm, Windows ARM, Linux
  ARM, and musl for `alpha.2`.
- [ ] `SECURITY.md` identifies the supported alpha and private reporting path.

## Bad-release response

1. Stop the workflow before publishing downstream packages whenever possible.
2. Keep unpublished npm versions off `latest`; move or remove the tag on a bad
   npm version and deprecate it. Do not replace its tarball.
3. Yank affected crates; crates.io versions are never deleted or reused.
4. Mark the GitHub prerelease withdrawn. Do not move or reuse the signed tag or
   silently replace an asset at the same name.
5. Publish a security advisory if integrity, containment, or data loss may be
   involved. Tell users to preserve the Fence store.
6. Reproduce against copies, fix forward in `alpha.(N+1)`, and document written
   schemas and downgrade refusals.

## Persisted-data compatibility during alpha

Readers may stop accepting an older record for mutation when accepting it
would weaken a safety proof. They still report bounded records and a precise
refusal. Missing frozen policy is never reconstructed from live files,
incomplete journals are never guessed, and future schemas are never parsed as
current ones. Immutable objects remain BLAKE3 identities over raw uncompressed
bytes; migration is explicit DTO conversion, not an in-place bulk rewrite.
