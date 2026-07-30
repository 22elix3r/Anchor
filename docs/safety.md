# Safety and threat model

## Mental model

For each path, Anchor reasons about three observable states:

- `base`: raw filesystem state immediately before the wrapped command;
- `session`: raw filesystem state captured when the command ended;
- `current`: raw filesystem state when restoration is requested.

The desired operation removes the `base → session` transformation from
`current`; it does not blindly write `base`.

The strongest guarantee Anchor can make is state-based:

> Anchor does not replace or remove an observed current path unless it proved
> that state is unchanged session-end residue, or a future merge engine has
> incorporated the current edit into a verified result.

Three snapshots cannot prove causal authorship or semantic intent. Anchor calls
changes “session-window changes,” never “changes made by the agent.”

## Enforced invariants

1. Capture never writes HEAD, refs, the Git index, Git object storage,
   configuration, or worktree content.
2. The wrapped command is not launched unless the before snapshot completes
   within configured limits.
3. A degraded manifest is rejected by restoration.
4. Object identity is BLAKE3 over uncompressed raw bytes; every read validates
   the envelope, declared length, and hash.
5. Stored paths are validated relative components. Absolute paths, `.` and `..`
   are rejected.
6. Symlinks are captured as opaque targets and never traversed during capture.
7. Single-path mutation evacuates the current node with a no-replace rename,
   verifies the evacuated bytes, and installs staged output with a second
   no-replace rename.
8. A concurrent creator is never overwritten. If rollback cannot safely reclaim
   the live name, the backup and journal remain for recovery.
9. Binary and otherwise opaque files are not speculatively merged.
10. Index restoration requires the current raw index to equal the recorded
    session-end index. It uses Git's `index.lock` convention plus evacuation and
    no-replace installation; split indexes are refused.
11. Whole restore requires a fresh current-manifest preview token and refuses
    the entire batch if any path has a structured conflict. It stages every
    output before mutation and retains every evacuated node until every target
    verifies.

## Restore decisions

| Base | Session end | Current | Result |
|---|---|---|---|
| equals session | any | any | Preserve current; no session delta |
| any | any | equals base | No action; already restored |
| any | any | equals session | Restore base exactly |
| absent | present | absent | Preserve absence |
| absent | present | different present | Conflict; do not delete |
| present | absent | absent | Recreate base |
| present | absent | different present | Conflict; do not overwrite |
| present | present | absent | Preserve post-session deletion |
| present | present | third opaque state | Conflict |

Executable bits are reasoned about independently. If the session changed only
the executable bits, Anchor can invert them while retaining later content. It
does not preserve ownership, timestamps, ACLs, extended attributes, or general
permission bits.

For a regular text file whose current bytes differ from both endpoints,
`--merge` may calculate an inverse three-way merge with `session end` as the
ancestor, `base` as the inverse side, and `current` as the post-session side.
Only valid UTF-8, NUL-free inputs up to 8 MiB each are considered. Different
line edits must not overlap, and output is capped at 16 MiB. A clean result is
shown as `current → merged` and is not installed until `--yes` is supplied.
Confirmation also supplies the previewed BLAKE3 object ID; Anchor recalculates
under the worktree lock and refuses if the result changed. Anchor returns a
structured conflict instead of writing conflict markers.

Schema-v3 restore journals retain the verified pre-restore node, intended node,
worktree identity, and sibling staging names. `anchor recover-transactions
--yes` validates those paths against the retained session and fresh repository
discovery, then rolls an interrupted file or index operation back to its
pre-restore state. Live, staged, or backup byte drift is a hard conflict.
Incomplete schema-v1/v2 journals are reported but not guessed at.

Batch journals record every path, expected and desired node, sibling temporary
name, and item state. `Verified` is the batch commit point: before it, recovery
restores every pre-operation node in reverse order; after it, recovery preserves
the verified inverse and only finishes cleanup. Multi-path mutation is therefore
recoverable but is not claimed to be globally atomic to concurrent observers.
Missing parent reconstruction is refused before target evacuation.

Deleting a session is recoverable until explicit purge. Tombstoned sessions
remain garbage-collection roots, so `anchor delete` cannot silently make their
objects collectible. `anchor purge --yes` removes the recovery record and is
the deliberate retention boundary.

## Capture consistency

Anchor does not claim a globally atomic filesystem snapshot. For each regular
file it:

1. reads no-follow metadata;
2. opens the file through a directory capability;
3. reads and hashes it;
4. checks open-handle and path metadata again;
5. retries up to three times if identity-relevant fields changed.

Directory names are enumerated before and after traversal, and the whole capture
is retried twice if the namespace changed. Persistent instability aborts.
Repository state and raw index bytes are sampled around the filesystem capture;
endpoint drift also triggers a bounded retry and then failure.

On Unix the active-session lock's open file description is inherited by the
wrapped child. If the wrapper crashes but the child keeps running, a second
session remains blocked until the child (and any descendant that retained the
descriptor) exits. A deliberately adversarial command can close inherited file
descriptors; Anchor does not claim containment of the wrapped command.

`SIGINT`, `SIGTERM`, `SIGHUP`, and `SIGQUIT` received by the Unix wrapper are
recorded and forwarded to the direct child. Anchor still waits for child exit
and attempts the after-snapshot. Terminal-generated signals may already reach
both processes through their shared foreground process group; signal delivery
is therefore not an authorship or containment mechanism.

When the wrapper disappears before a terminal session state is persisted, the
record remains nonterminal. A later `anchor recover` or new run first proves the
inherited worktree lock is free, then marks such records `Abandoned` without
inventing an after-snapshot. Abandoned records are not restorable.

## Repository drift

Anchor records HEAD attachment/target, recognized operation state, object hash
format, sparse mode, and raw index bytes at both endpoints. It reports drift but
does not move refs or rewrite history.

The current mutation backend refuses restoration if repository state changed
during the session or differs from the recorded session end. Working-tree
restoration never writes the index. The separate `restore-index` operation
returns a conflict without writing when post-session index drift is present.
CLI filesystem and index mutation requires explicit `--yes`; the default exact
restore invocation points back to the immutable session diff without writing.
The TUI is likewise not a mutation engine: `r` returns a selected-path intent to
the CLI, raw mode is restored, and the CLI asks for `y/yes` before calling the
same restore service. Rename rows are refused because a one-path action would
only invert half of the rename.

## Included and excluded data

Tracked paths are included even when an ignore rule matches. Nonignored
untracked paths are included. Git metadata, Anchor storage, ignored paths,
submodule contents, and explicit repository boundaries are excluded.

`.anchorignore` is a monotonic exclusion layer: its negation rules can cancel
earlier `.anchorignore` rules but cannot re-include a Git-ignored path.

Sensitive nonignored files are included. Anchor is local-only and performs no
telemetry or network I/O, but local users or malware able to read the store may
read captured contents. Unix store directories are forced to mode `0700`.
Windows store directories receive a protected DACL granting full access only to
the current user and Local System; `doctor` reads back and validates that exact
DACL.

Only the command program is recorded by default. Arguments are represented by a
count unless `--record-arguments` is explicitly enabled. The process environment
is never recorded. This metadata policy does not affect file inclusion: a
nonignored secret file is still captured.

Sparse checkout, sparse indexes, and split indexes are refused before launch.
The current implementation cannot yet freeze index-sourced ignore files from a
sparse worktree or retain every shared-index dependency, so claiming a complete
snapshot in those modes would violate the completeness invariant.

## Malicious or corrupt storage

Manifests and sessions are treated as untrusted:

- record tag, schema, size, path encoding, and tree invariants are validated;
- path traversal and leaf-prefix collisions are rejected;
- object decompression has a caller-supplied raw-size ceiling;
- object IDs and manifest IDs are recomputed on read;
- garbage collection aborts if retained metadata cannot be decoded and all
  reachable objects cannot be verified.

## Current non-guarantees

- no process-level or prompt-level attribution;
- no globally atomic snapshot;
- no globally atomic multi-file visibility or filesystem-wide transaction;
- no speculative binary merge or conflict-marker-only text merge;
- no worktree-plus-index combined transaction;
- no split-index restoration or Git history restoration;
- no recursive dirty-submodule capture or restoration;
- no preservation of hard-link topology, ACLs, xattrs, ownership, or timestamps;
- `SIGKILL`, power loss, or machine failure can leave an incomplete session
  that must be marked abandoned after its child lock is free;
- Windows support is experimental on non-NTFS filesystems, case-sensitive
  directories, and systems where security software denies delete sharing;
- no automatic reconstruction of missing uncaptured parent directories.

These cases must remain visible limitations, not silent fallbacks.
