# Changelog

All notable changes to Fence are recorded here. Versions follow Semantic
Versioning; persisted-data compatibility has its own policy because this
project stores safety-critical snapshots and recovery journals.

## [0.1.0-alpha.2] - 2026-08-03

This is the first public Unix alpha. Filesystem mutation is not yet stable and
is not supported on Windows. Cargo and npm versions, the Git tag, and release
artifacts use the same immutable `0.1.0-alpha.2` version.

### Included

- Public project, binary, packages, environment variables, ignore file, and
  release artifacts renamed from Anchor to Fence.
- Capability-rooted private-store access, an explicit Fence store marker, and
  hard refusal when a legacy Anchor store creates migration ambiguity.
- Raw-byte, content-addressed before and session-end worktree snapshots.
- Complete frozen inclusion policy across session endpoints.
- Structured session-end, current, and drift diffs.
- Conservative single-path and journaled whole-session Unix restoration.
- Exact index restoration only when the raw current index has not drifted.
- Process-crash recovery, doctor, recoverable deletion, and verified GC.
- Linux x86-64 and macOS x86-64/arm64 release archives with checksums and
  GitHub build-provenance attestations.
- crates.io distribution as `fence-cli` with the six lockstep internal crates.
- npm distribution as `fence-cli`, installing the `fence` command from an
  exact-version platform package without lifecycle scripts or runtime download.
- A versioned JSON envelope with `schema`, `operation`, `status`, and `data`
  fields for every command that supports `--format json`.

### Safety boundaries

- Changes are attributed to the session window, not to a process or agent.
- Capture is best-effort consistent and aborts on unresolved instability.
- Hard-linked nodes and detected unmodeled extended metadata are refused for
  mutation.
- ACLs, ownership, timestamps, hard-link topology, arbitrary Unix mode bits,
  and Windows security metadata are not restored.
- The direct wrapped child exiting defines the session endpoint; descendants
  may continue to write afterward.
- Recovery is tested for abrupt process death. Machine-power-loss durability is
  not claimed.
- The rename is a hard pre-alpha break: no `anchor` command alias,
  `.anchorignore` behavior, `ANCHOR_*` environment fallback, or automatic old
  store import is provided.
- The first npm alpha and direct binaries support GNU/Linux x86-64 and macOS
  x86-64/arm64. Windows remains source-tested but is not distributed.
- Older ad-hoc JSON output is not compatible with the public schema-1 envelope.

See `docs/safety.md` for the complete guarantee and refusal model.

## [0.1.0-alpha.1] - 2026-08-03

This incomplete publication uploaded only `fence-unix` before release
verification was blocked by crates.io rejecting download probes without a
descriptive HTTP user agent. It was never announced as an installable Fence
release. `fence-unix 0.1.0-alpha.1` will be yanked after the complete alpha.2
crate set is published.
