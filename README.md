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
- read-only Git discovery, index capture, tracked-path and ignore awareness via
  `gix`;
- stable before/after capture around inherited interactive terminal streams;
- path-level diffs with unique exact-content rename detection;
- structured three-state restoration planning;
- single-path regular-file and symlink restore on Unix when safety is proven;
- exact raw-index restore on Unix when the current index still equals the
  session-end index (split indexes are refused);
- refusal when current bytes, type, symlink target, mode, HEAD, or repository
  operation state is ambiguous;
- storage integrity checking and reachability-based garbage collection.

Not yet implemented: automatic text three-way merge, whole-session transactional
rollback, crash-journal recovery commands, empty-directory
mutation, the terminal reviewer, and Windows filesystem mutation. Those cases
are refused rather than approximated.

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
anchor diff <session-id> --format json
```

Restore one worktree-root-relative path:

```console
anchor restore <session-id> --file src/main.rs
anchor restore-index <session-id>
```

The restore either applies a byte-verified inverse, reports that no action is
needed, or exits with a visible conflict. It does not overwrite post-session
drift.

Verify retained data and preview garbage collection:

```console
anchor doctor
anchor gc --dry-run
anchor gc
```

`anchor diff` returns `1` when differences exist. Restore conflicts return `4`.
The `run` command returns the wrapped child’s exit code (or `128 + signal` on
Unix) after attempting the after-snapshot.

## Inclusion and limits

By default Anchor includes tracked files and nonignored untracked files. It
excludes Git metadata, its own store, ignored files, and submodule contents. A
root `.anchorignore` adds Git-style exclusions but cannot re-include a
Git-ignored path. `.anchorignore` itself is captured.

Default capture limits are 250,000 regular files, 2 GiB of raw content, and
256 MiB per file. Exceeding a limit aborts before the child starts. Ignored
files are not a security boundary: a nonignored `.env`, credential, or key file
is stored exactly like source code.

See [Safety and threat model](docs/safety.md) and
[Storage format](docs/storage.md) before using Anchor on sensitive worktrees.

## Platform support

| Platform | Capture/review | Filesystem restore |
|---|---|---|
| Linux | Supported | Experimental single-path |
| macOS | Supported | Experimental single-path |
| Windows | Experimental | Refused |

Windows paths are represented losslessly from the start. Mutation remains
disabled until no-replace replacement, reparse-point containment, ACLs, and
console behavior have dedicated tests.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security issues should follow
[SECURITY.md](SECURITY.md).

## License

Licensed under either Apache-2.0 or MIT, at your option.
