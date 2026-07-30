# Anchor

Anchor is an open-source, local-first Rust CLI for reviewing filesystem changes
observed during an interactive command session.

```console
anchor run -- codex
anchor run -- claude
anchor run -- aider
anchor run -- opencode
anchor run -- bash
```

Anchor snapshots the worktree before launching the command and again after it
exits. Existing staged, unstaged, and untracked work is part of the before
snapshot, so a later restore does not mean “reset to Git.” Anchor does not invoke
the `git` executable, create commits, modify refs, or use Git object storage.

> Anchor reports **session-window changes**. It cannot prove which process or
> person wrote them.

## Project status

Anchor is pre-release software. The implemented safety-first subset is:

- lossless Unix-byte and Windows-WTF-16 path records;
- immutable BLAKE3-addressed, Zstandard-compressed raw-byte objects;
- versioned CBOR manifests and session records;
- frozen per-session capture policy with schema-v1 migration support;
- read-only Git discovery, index capture, tracked-path and ignore awareness via
  `gix`;
- stable before/after capture around inherited interactive terminal streams;
- Unix signal forwarding with an after-capture attempt after interruption;
- stale-session abandonment after the inherited child lock is free;
- path-level diffs with unique exact-content rename detection;
- bounded, control-character-sanitized unified text diffs;
- a side-by-side terminal reviewer with narrow-layout fallback and confirmed
  file-level restore actions;
- structured three-state restoration planning;
- bounded inverse three-way text merge with structured overlap conflicts;
- single-path regular-file and symlink restore on Unix when safety is proven;
- byte-verified empty-directory restore on Unix;
- preview-token-bound whole-session restore of all unambiguous included paths,
  with staged outputs and a recoverable multi-path journal on Unix;
- exact raw-index restore on Unix when the current index still equals the
  session-end index (split indexes are refused);
- byte-verified rollback of interrupted file, index, and pre-commit batch
  transactions, plus roll-forward cleanup after a verified batch commit point;
- refusal when current bytes, type, symlink target, mode, HEAD, or repository
  operation state is ambiguous;
- storage integrity checking and reachability-based garbage collection.

Not yet implemented: combining index restoration into the worktree batch,
hunk-level restore, and Windows filesystem mutation. Those cases are refused
rather than approximated.

## Build

Anchor uses Rust 2024 and has an MSRV of Rust 1.85.

```console
cargo build --release -p anchor-cli
./target/release/anchor --help
```

The runtime has no network dependency and does not require the `git` executable.

## Usage

Run any interactive command from inside a non-bare Git worktree:

```console
anchor run -- codex
anchor sessions
anchor show <session-id>
anchor diff <session-id>
anchor diff <session-id> --current
anchor diff <session-id> --drift
anchor diff <session-id> --format json
anchor review <session-id>
```

In the reviewer, `r` exits raw terminal mode and asks for confirmation before
calling the same exact single-path restore service. Exact renames are disabled
as a one-file TUI action because they span two paths.

Restore one worktree-root-relative path:

```console
anchor restore <session-id> --file src/main.rs
anchor restore <session-id> --file src/main.rs --yes
anchor restore <session-id> --file src/main.rs --merge
anchor restore <session-id> --file src/main.rs --merge --yes \
  --expect-merged <previewed-object-id>
anchor restore <session-id> --all
anchor restore <session-id> --all --yes \
  --expect-current <previewed-manifest-id>
anchor rollback <session-id>
anchor rollback <session-id> --yes \
  --expect-current <previewed-manifest-id>
anchor rollback <session-id> --format json
anchor restore <session-id> --file src/main.rs --yes --format json
anchor restore-index <session-id> --yes
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
persistent batch journal retains every backup until all targets verify. Anchor
refuses a batch that would require reconstructing a missing parent directory;
it does not infer uncaptured structural directories. The index remains a
separate opt-in operation.

Verify retained data and preview garbage collection:

```console
anchor doctor
anchor gc --dry-run
anchor gc
anchor recover
anchor recover-transactions --yes
anchor delete <session-id> --yes
anchor deleted-sessions
anchor undelete <session-id>
```

`anchor diff` returns `1` when differences exist. Restore conflicts return `4`.
The `run` command returns the wrapped child’s exit code (or `128 + signal` on
Unix) after attempting the after-snapshot.

The default diff is `before → session end`. `--current` is `before → current`;
`--drift` is `session end → current` and separately reports repository and raw
index drift. `anchor recover` never guesses a missing session end: after it can
acquire the worktree lock, it marks stale nonterminal records `Abandoned`.
`anchor recover-transactions --yes` is separate: it byte-verifies and rolls
back interrupted schema-v3 single-path/index or pre-commit batch transactions.
Once every batch target verified, recovery instead finishes backup cleanup
because that state is the durable commit point. Legacy incomplete journals
remain visible but require manual recovery because they lack sufficient state.

Session deletion is recoverable by default. Tombstoned sessions continue to
protect their manifests and objects from garbage collection. `anchor purge
<id> --yes` permanently removes only the tombstoned record; a later `anchor gc`
can then reclaim newly unreachable immutable data.

## Inclusion and limits

By default Anchor includes tracked files and nonignored untracked files. It
excludes Git metadata, its own store, ignored files, and submodule contents. A
root `.anchorignore` adds Git-style exclusions but cannot re-include a
Git-ignored path. `.anchorignore` itself is captured.

Default capture limits are 250,000 regular files, 2 GiB of raw content, and
256 MiB per file. Exceeding a limit aborts before the child starts. Ignored
files are not a security boundary: a nonignored `.env`, credential, or key file
is stored exactly like source code.

Command arguments are not retained by default because they commonly contain
tokens and other secrets. Use `anchor run --record-arguments -- <command>` only
when the complete invocation is safe to store. Capture limits and the two
degraded-behavior switches can be configured or overridden explicitly; see
[Configuration](docs/configuration.md).

See [Safety and threat model](docs/safety.md) and
[Storage format](docs/storage.md) before using Anchor on sensitive worktrees.
Implemented and remaining work is tracked in the
[implementation roadmap](docs/roadmap.md).

## Platform support

| Platform | Capture/review | Filesystem restore |
|---|---|---|
| Linux | Supported | Experimental single-path and batch |
| macOS | Supported | Experimental single-path and batch |
| Windows | Wire-format and core tests only | Refused |

Windows paths are represented losslessly from the start. Session capture and
mutation are refused until reparse-point containment, private ACL creation,
no-replace replacement, and console behavior have dedicated tests.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security issues should follow
[SECURITY.md](SECURITY.md).

## License

Licensed under either Apache-2.0 or MIT, at your option.
