# Unix alpha release checklist

Fence mutates source trees, so a tag is not a release gate by itself. The
release owner must record each item below in the release pull request or tag
preparation notes.

## Source and compatibility

- [ ] The version is a `0.1.0-alpha.N` prerelease in `Cargo.toml`,
  internal workspace dependency requirements, `Cargo.lock`, and
  `CHANGELOG.md`.
- [ ] The changelog describes safety boundaries, refusals, schema changes, and
  operator actions; it does not claim process authorship or Windows mutation.
- [ ] The repository contains only SHA-pinned third-party Actions and
  Dependabot has no ignored action-pin update.
- [ ] New writers emit only documented schema versions. Fixtures prove that
  supported old records load into conservative in-memory states.
- [ ] Unsupported future schemas, sessions without complete frozen policy, and
  recovery journals without immutable plan binding refuse mutation.
- [ ] A downgrade is not required or recommended. If a new schema was written,
  the release notes state that older binaries may refuse it.

## Required verification

Run on a clean checkout of the release commit:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo +1.85 test --workspace --locked
cargo deny check advisories bans licenses sources
```

- [ ] Stable tests pass on Linux, macOS, and Windows CI.
- [ ] MSRV Rust 1.85 passes.
- [ ] Scheduled fuzz smoke is green for every target in `fuzz/Cargo.toml`, or
  every target was run manually for at least 30 seconds.
- [ ] The real-process crash matrix passes on Linux and macOS.
- [ ] The formal restoration matrix has no unclassified row.
- [ ] No unresolved recovery journal remains after the end-to-end smoke tests.
- [ ] `scripts/release-smoke.sh` passes against each packaged binary with a
  `PATH` that cannot contain `git`.
- [ ] `cargo deny` is clean. Any advisory exception is narrow, documented, and
  reviewed before tagging.

## Artifact review

- [ ] The release workflow builds Linux x86-64, macOS x86-64, and macOS arm64
  from the annotated release tag.
- [ ] Every archive contains only the `fence` binary, licenses, changelog, and
  user-facing safety/configuration/storage documentation.
- [ ] SHA-256 checksum files verify after downloading the artifacts.
- [ ] `gh attestation verify <archive> -R 22elix3r/fence` succeeds for each
  archive.
- [ ] The GitHub release is marked prerelease and says that Unix mutation is
  experimental and Windows mutation is unsupported.
- [ ] No crate is published to crates.io until package metadata and the
  multi-crate publication order have received a separate review.
- [ ] The annotated tag points to a commit on protected `main`, the same commit
  has a successful `CI` push run, and version/tag/changelog checks agree.

## Bad-release response

1. Mark the affected GitHub prerelease as withdrawn and remove it from the
   recommended installation path. Do not reuse or move the released tag.
2. Publish a security advisory if integrity, path containment, or data loss may
   be involved.
3. Tell users not to delete the Fence store. A newer recovery binary may need
   its journals and immutable objects.
4. Reproduce against a copy of the worktree and store. Never ask a reporter to
   retry mutation on their only copy.
5. Fix forward in `0.1.0-alpha.(N+1)`. Document whether the bad version wrote a
   schema that older binaries cannot read.

## Persisted-data compatibility during alpha

Alpha readers may stop accepting an older record for mutation when accepting it
would weaken a safety proof. They must still report the record and a precise
refusal reason when bounded parsing is possible. Writers never silently
reinterpret an old field as stronger evidence:

- legacy unknown metadata is not proven absence;
- a missing frozen policy is not reconstructed from live ignore files;
- an incomplete journal is not guessed into a recovery state;
- future schema versions are not parsed as current versions.

Immutable objects remain identified by BLAKE3 over raw uncompressed bytes.
Schema migrations are explicit DTO-to-current conversions; there is no
in-place bulk rewrite in the alpha line.
