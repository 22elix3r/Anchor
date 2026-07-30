# Storage format and data location

Anchor stores data beneath the resolved Git common directory:

```text
$GIT_COMMON_DIR/anchor/users/<principal>/v1/
├── objects/b3/<prefix>/<suffix>.zst
├── manifests/b3/<prefix>/<suffix>.cbor
├── sessions/<worktree-key>/<session-id>.cbor
├── locks/<worktree-key>.active.lock
├── locks/store.activity.lock
└── transactions/<transaction-id>/
```

Objects and manifests are shared by linked worktrees. Sessions and the active
session lock are namespaced by worktree. The main worktree uses `main`; linked
worktrees use a BLAKE3-derived key from the native private Git-directory path.

Bare repositories are refused. Non-Git directories are not supported in the
current release.

## Objects

Object identity is BLAKE3 over the uncompressed raw bytes. The file envelope
contains:

- magic `ANCHOBJ1`;
- codec identifier (`1` means Zstandard);
- declared uncompressed length;
- the full BLAKE3 object ID.

Compression level and future codec choices do not affect identity. Publication
uses a private temporary file, `fsync`, and no-clobber persistence. An existing
object is fully verified instead of overwritten.

## Manifests

Manifest identity is a domain-separated BLAKE3 hash of deterministic CBOR.
Schema v1 is an explicit tuple containing:

- record tag and schema version;
- path-encoding family;
- sorted path entries;
- completeness and omissions.

Paths are componentized. Unix components contain raw bytes. Windows components
contain little-endian WTF-16 code units, preserving unpaired surrogates.
Components cannot be empty, `.`, `..`, rooted, prefixed, contain NUL, or contain
native separators.

Entries represent regular files, symlinks, and empty directories. Regular files
reference an object and store raw size plus Unix executable bits. Symlinks store
the opaque native target. Sockets, FIFOs, devices, and unsupported reparse points
are never encoded as regular files.

## Sessions

Session records are mutable, atomically replaced CBOR records. Schema v2
contains:

- a UUIDv7 session ID;
- the native command program and, only when opted in, its arguments;
- the count of arguments deliberately omitted from metadata;
- the frozen capture limits, completeness switches, and recording policy;
- the native invocation directory and worktree root;
- before and optional after endpoint records;
- raw-index object references and parsed summaries;
- repository state;
- timestamps, child result, lifecycle state, and a bounded failure message.

The environment is not recorded. Command arguments can contain secrets and are
therefore omitted by default. Schema-v1 sessions remain readable and are
reported as having recorded full arguments because that was the v1 behavior.

## Garbage collection

`anchor gc` acquires an exclusive lease on the common store, decodes sessions
from every linked-worktree namespace, loads every referenced manifest, and
verifies every reachable object before sweeping anything. Normal readers,
session capture, and restoration hold shared leases. Any corrupt retained
record or unresolved restore transaction aborts collection.

`anchor gc --dry-run` reports the same reachability result without deletion.
Objects published by a crashed capture but never referenced by a session are
eligible for collection once no active session holds the lock.

Schema migrations are append-only: readers dispatch on the record tag and
version, validate the old DTO, and convert it into current in-memory types.
Writers emit only the newest schema. Unsupported future schemas are refused.
