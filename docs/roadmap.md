# Implementation roadmap

This roadmap records what remains after the safety-critical Unix prototype. It
is ordered by architectural dependency, not by UI visibility. A phase is done
only when its refusal behavior, crash behavior, and cross-platform boundary are
tested and documented.

## Current implementation baseline

| Area | Current state | Remaining boundary |
|---|---|---|
| Raw object store | BLAKE3 identity, Zstandard envelope, verified reads, atomic publication | Streaming large-object write/read paths and performance benchmarks |
| Manifests | Versioned CBOR, lossless Unix/WTF-16 paths, coverage and omission records | Migration fixture corpus and parser fuzzing |
| Git awareness | `gix` discovery, tracked paths, ignores, raw index and repository state | Sparse/split-index proof, broader linked-worktree fixtures |
| Capture | No-follow traversal, per-file and directory stability checks, limits | Incremental reuse and bounded parallel I/O |
| Sessions | Crash-visible lifecycle, inherited TTY/console, Unix signal forwarding, Windows Job containment, stale recovery | PTY opt-in spike only if inherited handles prove insufficient |
| Diff | Structured path changes, exact rename detection, bounded unified text | Lazy large-session rendering and optional fuzzy rename |
| Restore | Exact path inverse, bounded text inverse merge, recoverable worktree batch | Batch text merge, safe parent reconstruction, index composition |
| Index | Exact raw restore after no-drift proof | Split index and combined worktree/index transaction |
| TUI | Side-by-side/unified review and confirmed file restore | Conflicts, batch selection, hunk restore, very-large-file paging |
| Maintenance | Doctor, shared-store GC, tombstones, transaction recovery | Retention policy and terminal-journal pruning |
| Windows | Native no-follow capture, ACL hardening, Job containment, journaled worktree/index restore | Broader filesystem, antivirus, console, and crash-fault matrix |

## Release-critical phases

### R1. Restoration fault-injection matrix

Status: in progress. Deterministic crashes at all whole-batch and first-item
boundaries, post-commit roll-forward, concurrent-creator refusal, and duplicate
journal-path refusal are implemented.

Objective: prove that every persisted batch state either rolls back to the
recorded current state or rolls forward from the verified commit point.

Implementation:

- add a test-only fault hook at journal publication, stage creation, evacuation,
  installation, verification, and backup cleanup;
- generate one- and multi-path cases for present/absent regular files,
  symlinks, empty directories, mode changes, and exact renames;
- restart through `TransactionRecoveryService` after every injected failure;
- assert byte/type/mode equality, absence of unverified replacement, terminal
  journal state, and GC refusal while unresolved;
- add corruption cases for duplicate paths, unsafe temporary names, wrong
  worktree identity, changed live nodes, changed backups, and oversized records.

Acceptance:

- every pre-commit injected crash restores all expected nodes or returns a
  visible recovery conflict without overwriting the live tree;
- every post-commit crash preserves every verified desired node and completes
  cleanup;
- property tests cover arbitrary small path-state batches;
- no test invokes the `git` executable.

Likely files: `crates/anchor-session/src/restore.rs`,
`crates/anchor-session/tests/restore_faults.rs`, and a small test-only fault
module.

### R2. Complete restoration plan

Objective: make whole restore useful for the remaining safe text and tree cases
without weakening its all-or-nothing conflict gate.

Implementation:

- represent a batch plan as exact expected and desired nodes plus optional
  previewed merge-object IDs;
- calculate bounded inverse text merges for drifted regular files during
  preview and bind all results to the current-manifest ID;
- model implicit ancestor directories separately from captured empty-directory
  entries;
- create only ancestors proven present in `base`, absent at session end, absent
  now, and free of current prefix collisions;
- journal created ancestors and remove them during rollback only when empty;
- add `--paths-from`, repeated `--file`, and TUI file selection on top of the
  same core plan API; do not add shell-glob expansion inside the safety core.

Acceptance:

- a clean text merge can participate in a batch;
- one overlapping edit prevents every mutation;
- a session-deleted subtree can be reconstructed;
- a current file, symlink, reparse point, or case-folding collision at any
  ancestor refuses the entire plan;
- the plan preview is serializable as JSON without exposing file contents.

### R3. Optional combined index transaction

Objective: support `rollback --include-index` without allowing worktree success
and index failure to silently diverge.

Implementation:

- extend the tagged batch journal with an optional index item containing the
  discovered index path, expected session-end capture, desired before capture,
  lock name, stage, backup, and state;
- acquire `index.lock` before the first worktree evacuation and re-read the raw
  index after acquiring it;
- refuse split indexes, an existing lock, path drift, or current-index mismatch;
- keep the index backup through the same batch commit point;
- recovery validates fresh `gix` discovery and rolls the index with the
  worktree, never through an index-entry merge.

Acceptance:

- post-session staging always returns a conflict before worktree mutation;
- injected failures at every index/worktree ordering point recover coherently;
- the default rollback continues to leave the index untouched.

### R4. Integration, property, and fuzz suites

Status: in progress. Core absent/present/content restoration invariants use
`proptest`; manifest and native-path parser harnesses run in an isolated
`cargo-fuzz` workspace with a scheduled smoke workflow.

Objective: move safety claims out of unit-only fixtures.

Implementation:

- create library-driven temporary Git fixtures for staged+unstaged same-file
  state, intent-to-add, conflicts, unborn and detached HEAD, linked worktrees,
  submodules, nested repositories, ignore negation, non-UTF-8 names, symlinks,
  mode changes, and capture instability;
- add the formal `base × session-end × current` restoration matrix for every
  node kind;
- add `proptest` generators for valid native paths, manifests, and restore
  decisions;
- add `cargo-fuzz` targets for manifest/session/journal decode, object
  decompression, native path validation, diff rendering, and restore planning;
- add Miri jobs for pure core algorithms and scheduled fuzz smoke jobs.

Acceptance:

- invariant assertions explicitly prove preservation of pre-session and
  post-session state;
- CI exercises Linux, macOS, Windows, stable, and MSRV;
- fuzz parsers enforce input-size ceilings before large allocation.

### R5. Unix release hardening

Objective: ship a supportable first Unix release.

Implementation:

- add JSON output for restore previews, conflicts, doctor, recovery, and GC with
  a versioned envelope;
- add age/count retention and terminal-transaction pruning to GC, always with a
  dry run and active-reader exclusion;
- benchmark 10k, 100k, and 250k file fixtures plus repeated high-overlap
  sessions;
- stream compression/decompression and introduce bounded hashing concurrency
  only after benchmark evidence;
- publish a crate/binary release workflow with checksums, SBOM/provenance where
  practical, and signed GitHub release artifacts;
- add a compatibility fixture corpus for every persisted schema.

Acceptance:

- fresh install, capture, review, rollback, recovery, delete, and GC smoke tests
  pass on Linux and macOS;
- release artifacts run without a `git` executable;
- all limits, storage locations, non-guarantees, and exit codes are documented.

## Windows enablement

Windows capture and restoration are enabled as experimental. Implemented:

1. handle-relative enumeration/open with 128-bit identity checks;
2. standard symlink preservation and explicit refusal of unknown reparse tags,
   junctions, ADS, hard links, EFS, and cloud placeholders;
3. Local AppData storage with protected current-user/SYSTEM DACL verification;
4. kill-on-close Job containment for the wrapped process tree;
5. `NtCreateFile` staging, no-replace handle rename/delete, endpoint
   verification, and durable worktree/index recovery journals.

Remaining hardening is a real-runner matrix for ReFS/network volumes, long and
reserved paths, case-sensitive directories, antivirus sharing failures,
open-file replacement, console control behavior, and fault injection at every
Windows journal boundary. A PTY crate such as `portable-pty` should be added
only if real agent smoke tests demonstrate inherited console handles are
insufficient.

## Post-v1 feature scopes

These features fit the architecture but do not block the first trustworthy
release:

- **Capture scope preview:** `anchor scope` explains tracked, ignored,
  Anchor-excluded, boundary, sensitive-name, and limit decisions before a
  command runs.
- **Sensitive-file preflight:** warn on configurable credential-name patterns
  without reading or uploading content; explicit policy decides abort versus
  local capture.
- **Incremental capture:** reuse a prior manifest only when a platform-specific
  metadata fingerprint and directory-generation proof match; fall back to raw
  hashing on uncertainty.
- **Session labels and notes:** user-supplied local metadata with the same
  argument-secret warnings and bounded storage.
- **Conflict bundles:** materialize base/session/current/merged candidates into
  a private store directory and emit a structured conflict record; never put
  markers into the worktree automatically.
- **Hunk selection:** produce a new immutable desired object from selected
  inverse hunks, bind it to current bytes, and apply through the existing
  transaction engine.
- **Optional fuzzy rename hints:** review-only similarity ranking with strict
  CPU/size budgets; restoration continues to rely on path states and exact
  expected bytes.
- **Non-Git directory mode:** use an OS application-data root keyed by a stored
  directory identity. It must define ignore defaults and move/identity behavior
  before implementation; it must not imitate Git.

Cloud sync, accounts, telemetry, process attribution, Git history rollback,
custom terminal emulation, semantic diffs, and recursive dirty-submodule
restoration remain explicit non-goals.

## Immediate next task

Implement R1’s test-only fault hook for batch restore, beginning with
`Prepared → Staged → Evacuating → Installing → Verified → Complete`.

The task is independently reviewable when:

- production behavior is unchanged when the hook is absent;
- one integration test restarts recovery after each transition for a two-file
  batch;
- pre-commit transitions recover the exact session-end tree;
- post-commit transitions retain the exact restored tree;
- unresolved corruption is a visible refusal;
- workspace tests, strict Clippy, Rustdoc, and all three platform CI builds pass.
