# Storage format and data location

On Unix, Fence stores data beneath the resolved Git common directory:

```text
$GIT_COMMON_DIR/fence/users/<principal>/v1/
├── fence-store
├── objects/b3/<prefix>/<suffix>.zst
├── manifests/b3/<prefix>/<suffix>.cbor
├── policies/b3/<prefix>/<suffix>.cbor
├── plans/b3/<prefix>/<suffix>.cbor
├── sessions/<worktree-key>/<session-id>.cbor
├── deleted-sessions/<worktree-key>/<session-id>.cbor
├── locks/<worktree-key>.active.lock
├── locks/store.activity.lock
└── transactions/<transaction-id>/
```

Objects and manifests are shared by linked worktrees. Sessions and the active
session lock are namespaced by worktree. The main worktree uses `main`; linked
worktrees use a BLAKE3-derived key from the native private Git-directory path.

On Windows the equivalent layout is under
`FOLDERID_LocalAppData/Fence/stores/v1/repo-<identity>/`. The repository and
linked-worktree keys derive from volume serial plus the 128-bit filesystem file
ID, so path spelling and repository renames do not change identity. Every
created store directory has a protected current-user/SYSTEM DACL.

Production store initialization opens the resolved Git common directory once
as a trusted boundary. Every component below it is created and opened
separately. Unix directory opens use `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC`;
existing components must be directories owned by the effective uid with no
group/other mode bits. Weak or foreign pre-existing components are refused,
not repaired. The retained `cap-std` directory capability roots object,
manifest, policy, restore-plan, session, lock, tombstone, and garbage-
collection operations. Record reads compare the opened identity with the
capability-relative directory entry. Transaction directories use an exclusive
capability-relative create; journal semantics still provide the independent
content/path validation described below.

The `fence-store` file contains the exact product/layout marker
`FENCE_STORE\n1\n`. A nonempty root without that marker, or with different
bytes, is refused. This prevents an arbitrary development store from being
silently claimed as Fence data.

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
uses a private temporary file, synchronizes its content, and publishes through a
no-clobber hard link. Unix also synchronizes the destination directory. Windows
cannot flush directory handles; Fence therefore claims process-crash recovery,
not machine-power-loss durability, on that platform. An existing object is fully
verified instead of overwritten.

## Manifests

Manifest identity is a domain-separated BLAKE3 hash of deterministic CBOR.
Writers emit schema v3; schemas v1 and v2 remain byte-identical and readable. The
explicit tuple contains:

- record tag and schema version;
- path-encoding family;
- sorted path entries;
- completeness and omissions.

Paths are componentized. Unix components contain raw bytes. Windows components
contain little-endian WTF-16 code units, preserving unpaired surrogates.
Components cannot be empty, `.`, `..`, rooted, prefixed, contain NUL, or contain
native separators.

Entries represent regular files, symlinks, and empty directories. Regular files
reference an object and store raw size plus either Unix executable bits or the
Windows read-only attribute. Windows symlinks additionally retain link kind,
substitute name, print name, and reparse flags. Sockets, FIFOs, devices,
junctions, cloud placeholders, EFS files, alternate streams, and unsupported
reparse points are never silently encoded as ordinary files.

Schema v3 safety observations distinguish unqueried legacy metadata, proven
absence, detected unmodeled metadata, query failure, and platform-managed
metadata. Regular entries also retain observed link count and a capture-local
hard-link group. These fields authorize or refuse a mutation; they do not claim
to reproduce topology or metadata. The schema-v1/v2 Boolean `false` is migrated
to `Unknown`, never to proven absence.

## Sessions

Session records are mutable, atomically replaced CBOR records. Schema v3
contains:

- a UUIDv7 session ID;
- the native command program and, only when opted in, its arguments;
- the count of arguments deliberately omitted from metadata;
- the frozen capture limits, completeness switches, and recording policy;
- the content-derived ID of the complete frozen inclusion policy;
- the native invocation directory and worktree root;
- before and optional after endpoint records;
- the policy observed at each endpoint and structured drift from the frozen
  policy;
- raw-index object references and parsed summaries;
- repository state;
- timestamps, child result, lifecycle state, and a bounded failure message.

New sessions persist capture-policy version 2, where `max_entries` is the
ceiling across regular files, symlinks, and empty directories. Policy version 1
and the legacy serialized `max_files` field remain readable and are migrated to
the same, conservatively stricter entry-ceiling meaning.

The environment is not recorded. Command arguments can contain secrets and are
therefore omitted by default. Schema-v1 and schema-v2 sessions remain readable.
They predate complete policy freezing, so current-state capture and worktree
restoration refuse them instead of reconstructing policy from live files.
Schema-v1 records are reported as having recorded full arguments because that
was the v1 behavior.

## Frozen inclusion policies

Before the initial worktree capture, Fence reads and content-addresses the
selected `core.excludesFile` (including explicit absence), the common Git
directory's `info/exclude`, every reachable in-tree `.gitignore`, and the root
`.fenceignore`. The policy record also stores source ordering,
`core.ignoreCase`, the base tracked set, submodule boundaries,
nested-repository boundaries, and the worktree-relative Fence-store exclusion
when applicable.

The before capture is made with that compiled immutable record. Fence repeats
policy discovery after the capture and does not launch the child unless the two
records are equal. Session-end and current captures compile the retained policy
and add only endpoint tracked paths. They separately observe live policy
sources and record drift; changed ignore bytes do not alter the frozen scope. A
changed submodule or nested-repository boundary makes the endpoint incomplete
instead of allowing a different tree meaning.

Policy source contents are ordinary immutable Fence objects. Garbage
collection marks objects referenced by both the frozen policy and endpoint
observations before sweeping.

## Garbage collection

`fence gc` acquires an exclusive lease on the common store, decodes sessions
from every linked-worktree namespace, loads every referenced manifest, and
verifies every reachable object before sweeping anything. Normal readers,
session capture, and restoration hold shared leases. Any corrupt retained
record or unresolved restore transaction aborts collection.

`fence gc --dry-run` reports the same reachability result without deletion.
Objects published by a crashed capture but never referenced by a session are
eligible for collection once no active session holds the lock.

Schema migrations are append-only: readers dispatch on the record tag and
version, validate the old DTO, and convert it into current in-memory types.
Writers emit only the newest schema. Unsupported future schemas are refused.

During the `0.1.0-alpha.N` line, compatibility is subordinate to restoration
safety. A newer reader may classify an old record as review-only when the old
schema lacks evidence required for mutation. It must still report a bounded,
valid old record and its refusal reason; it must not strengthen missing
evidence. An older binary may refuse a newer schema. Operators should retain
the store and use a newer recovery binary rather than downgrade or manually
delete transaction data. Immutable raw-byte object identity is not versioned by
the enclosing manifest or session schema.

### Anchor-to-Fence boundary

The rename is a hard pre-alpha boundary. Fence uses a new store root and does
not scan, import, rewrite, or garbage-collect the former
`$GIT_COMMON_DIR/anchor/...` (or Windows Local AppData `Anchor`) root. Its
presence is reported by `doctor` and blocks session start and restoration to
avoid presenting two stores as one history. There is no migration command in
the alpha.

Opaque internal identifiers retain their already-reviewed bytes:
`ANCHOBJ1`, the `anchor:*` manifest/path/restore-plan hash domains, and existing
numeric record tags. They are wire identifiers, not user-visible product
names. Changing them would break object/schema identity without improving
containment. New public names and any new schema domains use Fence.

## Restore journals

Restore transaction directories contain an atomically replaced `journal.cbor`.
Unix single-path schema v5 and index schema v4 record the owning
session/worktree, immutable restore-plan ID, transaction ID, validated target
path, sibling stage and backup names, exact expected node, desired node, and
progress state. Windows remains on schema v2 and is not a supported mutation
backend for the Unix alpha.
Index journals additionally record the freshly validated index path and raw
before/after captures.

Unix batch-journal schema v4 stores the owning session/worktree, immutable
restore-plan ID, transaction ID, and an ordered list of validated relative
paths, exact expected/desired nodes and safety observations, collision-resistant
sibling stage/backup names, per-item progress, and a batch state. Journal input
is capped at 256 MiB and duplicate paths or temporary names are rejected during
recovery. `Verified` is the durable commit point: recovery rolls earlier states
back and rolls `Verified`, `Cleaning`, and `CleanupComplete` states forward by
verifying targets and removing backups.

Completed and safely rolled-back journals are terminal. Any other state blocks
new sessions, restoration, and garbage collection. Recovery never trusts a
stored absolute path alone: it loads the owning session, rediscovers the Git
worktree with `gix`, verifies that the common-store identity and index path
match, then validates live/staged/backup bytes before a no-replace rollback or
post-commit batch cleanup.
Legacy incomplete journals do not contain enough information for this and are
refused.

## Session retention

`fence delete` moves a terminal session record into `deleted-sessions` using a
no-clobber same-filesystem hard link followed by source removal. A crash can
leave duplicate links but cannot leave no record. Tombstoned sessions remain GC
roots and can be restored with `fence undelete`.

`fence purge --yes` permanently removes a tombstoned record. It does not
directly delete immutable data; the next GC verifies all remaining active and
tombstoned sessions before reclaiming anything newly unreachable.
