# Fence

Fence is an open-source, local-first Rust CLI for reviewing filesystem changes
observed during an interactive command session.

```console
fence run -- codex
fence run -- claude
fence run -- aider
fence run -- opencode
fence run -- bash
```

Fence snapshots the worktree before launching the command and again after it
exits. Existing staged, unstaged, and untracked work is part of the before
snapshot, so a later restore does not mean “reset to Git.” Fence does not invoke
the `git` executable, create commits, modify refs, or use Git object storage.

> Fence reports **session-window changes**. It cannot prove which process or
> person wrote them.

## Project status

Fence is pre-release software. The implemented safety-first subset is:

- lossless Unix-byte and Windows-WTF-16 path records;
- immutable BLAKE3-addressed, Zstandard-compressed raw-byte objects;
- versioned CBOR manifests and session records;
- content-addressed per-session inclusion policy freezing global excludes,
  common `info/exclude`, in-tree ignore bytes, tracked overlays, and repository
  boundaries;
- read-only Git discovery, index capture, tracked-path and ignore awareness via
  `gix`;
- stable before/after capture around inherited interactive terminal or console streams;
- Unix signal forwarding with an after-capture attempt after interruption;
- stale-session abandonment after the inherited child lock is free;
- path-level diffs with unique exact-content rename detection;
- bounded, control-character-sanitized unified text diffs;
- a side-by-side terminal reviewer with narrow-layout fallback and confirmed
  file-level restore actions;
- structured three-state restoration planning;
- bounded inverse three-way text merge with structured overlap conflicts;
- single-path regular-file, symlink, and empty-directory restore when safety is proven;
- preview-token-bound whole-session restore of all unambiguous included paths,
  with staged outputs and a recoverable multi-path journal;
- exact raw-index restore when the current index still equals the
  session-end index (split indexes are refused);
- byte-verified rollback of interrupted file, index, and pre-commit batch
  transactions, plus roll-forward cleanup after a verified batch commit point;
- refusal when current bytes, type, symlink target, mode, HEAD, or repository
  operation state is ambiguous;
- storage integrity checking and reachability-based garbage collection.
- experimental native Windows capture/review with handle-relative no-follow
  traversal, protected per-user storage, and kill-on-close child containment;
  the internal mutation backend remains publicly refused.

Not yet implemented: combining index restoration into the worktree batch,
and hunk-level restore. Those cases are refused rather than approximated.

## Build

Fence uses Rust 2024 and has an MSRV of Rust 1.85.

```console
cargo build --release -p fence-cli
./target/release/fence --help
```

The runtime has no network dependency and does not require the `git` executable.

The Fence rename is a deliberate pre-alpha compatibility break. The `fence`
binary does not provide an `anchor` alias, read `ANCHOR_*` environment
variables, interpret `.anchorignore`, auto-discover old Anchor stores, or
migrate them. If a legacy Anchor store is present for the same repository,
`fence doctor` reports it and mutation/session start refuses; the old data is
left untouched. Opaque pre-alpha wire-domain strings and object magic remain
unchanged where changing them would add no safety and would complicate
forensics.

## Usage

Run any interactive command from inside a non-bare Git worktree:

```console
fence run -- codex
fence sessions
fence show <session-id>
fence diff <session-id>
fence diff <session-id> --current
fence diff <session-id> --drift
fence diff <session-id> --format json
fence review <session-id>
```

In the reviewer, `r` exits raw terminal mode and asks for confirmation before
calling the same exact single-path restore service. Exact renames are disabled
as a one-file TUI action because they span two paths.

Restore one worktree-root-relative path:

```console
fence restore <session-id> --file src/main.rs
fence restore <session-id> --file src/main.rs --yes
fence restore <session-id> --file src/main.rs --merge
fence restore <session-id> --file src/main.rs --merge --yes \
  --expect-merged <previewed-object-id>
fence restore <session-id> --all
fence restore <session-id> --all --yes \
  --expect-current <previewed-manifest-id>
fence rollback <session-id>
fence rollback <session-id> --yes \
  --expect-current <previewed-manifest-id>
fence rollback <session-id> --format json
fence restore <session-id> --file src/main.rs --yes --format json
fence restore-index <session-id> --yes
```

The restore either applies a byte-verified inverse, reports that no action is
needed, or exits with a visible conflict. It does not overwrite post-session
drift. Without `--yes`, exact restore prints the diff-review command and makes
no change. `--merge` previews a clean, bounded inverse text merge without changing
the worktree and prints its object ID. `--merge --yes --expect-merged <id>`
recalculates and applies only that exact result. Overlapping edits,
binary/opaque input, and oversized text remain conflicts.

`rollback` is the clearer alias for `restore --all`; both first perform a
nonmutating preview. They apply only when every changed
path is unambiguous and `--expect-current` matches a freshly recaptured whole
worktree manifest. All outputs are staged before any target is evacuated. A
persistent batch journal retains every backup until all targets verify. Fence
refuses a batch that would require reconstructing a missing parent directory;
it does not infer uncaptured structural directories. The index remains a
separate opt-in operation.

Verify retained data and preview garbage collection:

```console
fence doctor
fence gc --dry-run
fence gc
fence recover
fence recover-transactions --yes
fence delete <session-id> --yes
fence deleted-sessions
fence undelete <session-id>
```

`fence diff` returns `1` when differences exist. Restore conflicts return `4`.
The `run` command returns the wrapped child’s exit code (or `128 + signal` on
Unix) after attempting the after-snapshot.

The default diff is `before → session end`. `--current` is `before → current`;
`--drift` is `session end → current` and separately reports repository and raw
index drift. `fence recover` never guesses a missing session end: after it can
acquire the worktree lock, it marks stale nonterminal records `Abandoned`.
`fence recover-transactions --yes` is separate: it validates the immutable
restore plan, byte-verifies, and rolls back interrupted single-path/index or
pre-commit batch transactions.
Once every batch target verified, recovery instead finishes backup cleanup
because that state is the durable commit point. Legacy incomplete journals
remain visible but require manual recovery because they lack sufficient state.

Session deletion is recoverable by default. Tombstoned sessions continue to
protect their manifests and objects from garbage collection. `fence purge
<id> --yes` permanently removes only the tombstoned record; a later `fence gc`
can then reclaim newly unreachable immutable data.

## Inclusion and limits

By default Fence includes tracked files and nonignored untracked files. It
excludes Git metadata, its own store, ignored files, and submodule contents. A
root `.fenceignore` adds Git-style exclusions but cannot re-include a
Git-ignored path. `.fenceignore` itself is captured.

Fence freezes the selected global Git excludes, common `info/exclude`, nested
`.gitignore`, root `.fenceignore`, case mode, and repository boundaries before
the initial capture. Session-end and current captures use those retained bytes
even if live ignore files later change; drift is shown separately. Older
session schemas remain readable but are review-only because they cannot prove a
complete current-state scope.

Default capture limits are 250,000 included manifest entries, 2 GiB of raw
regular-file content, and 256 MiB per regular file. Regular files, symlinks, and
empty directories all consume the entry ceiling. Exceeding a limit aborts
before the child starts. Ignored files are not a security boundary: a
nonignored `.env`, credential, or key file is stored exactly like source code.

Command arguments are not retained by default because they commonly contain
tokens and other secrets. Use `fence run --record-arguments -- <command>` only
when the complete invocation is safe to store. Capture limits and the two
degraded-behavior switches can be configured or overridden explicitly; see
[Configuration](docs/configuration.md).

See [Safety and threat model](docs/safety.md) and
[Storage format](docs/storage.md) before using Fence on sensitive worktrees.
Independent reviewers can start with the
[audit guide](docs/audit-guide.md). Implemented and remaining work is tracked
in the [implementation roadmap](docs/roadmap.md).

Tagged Unix alpha releases provide `.tar.gz` archives for Linux x86-64 and
macOS x86-64/arm64. Verify the adjacent SHA-256 file before installing:

```console
sha256sum --check fence-0.1.0-alpha.1-<target>.tar.gz.sha256
tar -xzf fence-0.1.0-alpha.1-<target>.tar.gz
install fence-0.1.0-alpha.1-<target>/fence ~/.local/bin/fence
```

On macOS, use `shasum -a 256 -c` in place of `sha256sum --check`. GitHub build
provenance can additionally be checked with:

```console
gh attestation verify fence-0.1.0-alpha.1-<target>.tar.gz \
  -R 22elix3r/fence
```

Release owners follow the [Unix alpha release
checklist](docs/release-checklist.md).

## Platform support

| Platform | Capture/review | Filesystem restore |
|---|---|---|
| Linux | Supported | Experimental single-path and batch |
| macOS | Supported | Experimental single-path and batch |
| Windows | Experimental native support | Not claimed; metadata-safe mutation is pending |

Windows paths and command arguments retain exact WTF-16. Capture uses pinned
directory handles, 128-bit file identities, reparse-point inspection, and
alternate-stream detection. A native no-replace transaction backend exists,
but the current manifest records extended-metadata observation as unavailable,
so public worktree mutation remains refused until that proof gap is closed.
Stores live under the current user's Local AppData and receive a protected
current-user/SYSTEM DACL. The wrapped process tree is assigned to a
kill-on-close Job object. Windows capture/review remains experimental while its
real-runner compatibility matrix grows, especially for non-NTFS volumes,
antivirus sharing interference, unusual case-sensitive directories, and
third-party console applications.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security issues should follow
[SECURITY.md](SECURITY.md).

## License

Licensed under either Apache-2.0 or MIT, at your option.
