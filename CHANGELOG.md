# Changelog

All notable changes to Anchor are recorded here. Versions follow Semantic
Versioning; persisted-data compatibility has its own policy because this
project stores safety-critical snapshots and recovery journals.

## [0.1.0-alpha.1] - Unreleased

This is an experimental Unix alpha. Filesystem mutation is not yet stable and
is not supported on Windows.

### Included

- Raw-byte, content-addressed before and session-end worktree snapshots.
- Complete frozen inclusion policy across session endpoints.
- Structured session-end, current, and drift diffs.
- Conservative single-path and journaled whole-session Unix restoration.
- Exact index restoration only when the raw current index has not drifted.
- Process-crash recovery, doctor, recoverable deletion, and verified GC.
- Linux x86-64 and macOS x86-64/arm64 release archives with checksums and
  GitHub build-provenance attestations.

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

See `docs/safety.md` for the complete guarantee and refusal model.
