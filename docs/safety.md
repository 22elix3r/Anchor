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
10. Anchor never changes the real Git index in the current release.

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

## Repository drift

Anchor records HEAD attachment/target, recognized operation state, object hash
format, sparse mode, and raw index bytes at both endpoints. It reports drift but
does not move refs or rewrite history.

The current mutation backend refuses restoration if repository state changed
during the session or differs from the recorded session end. Index drift does
not get overwritten because working-tree restoration never writes the index.

## Included and excluded data

Tracked paths are included even when an ignore rule matches. Nonignored
untracked paths are included. Git metadata, Anchor storage, ignored paths,
submodule contents, and explicit repository boundaries are excluded.

`.anchorignore` is a monotonic exclusion layer: its negation rules can cancel
earlier `.anchorignore` rules but cannot re-include a Git-ignored path.

Sensitive nonignored files are included. Anchor is local-only and performs no
telemetry or network I/O, but local users or malware able to read the store may
read captured contents. Unix store directories are forced to mode `0700`.
Windows ACL hardening is not yet implemented, and `doctor` reports the store as
not private there.

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
- no atomic multi-file restore;
- no automatic three-way text merge;
- no Git index or history restoration;
- no recursive dirty-submodule capture or restoration;
- no preservation of hard-link topology, ACLs, xattrs, ownership, or timestamps;
- no safe automatic Windows mutation yet;
- `SIGKILL`, power loss, or machine failure can leave an incomplete session;
- crash-journal automatic recovery is not yet exposed as a command.

These cases must remain visible limitations, not silent fallbacks.
