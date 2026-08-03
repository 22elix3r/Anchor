# Troubleshooting

## `fence` is not on PATH

Check the package manager's prefix and use a user-owned bin directory:

```console
npm prefix -g
cargo install --list
command -v fence
```

For Cargo's default installation, add `$HOME/.cargo/bin` to PATH. For manual
archives, add `$HOME/.local/bin`.

## npm reports a missing platform package

Do not install with `--omit=optional`. Remove and reinstall `fence-cli` with
optional dependencies enabled. The launcher never falls back to another
`fence` on PATH and never downloads a replacement itself.

Alpine/musl, Linux ARM, and Windows are not supported by the alpha npm package.

## Cargo cannot compile Fence

Confirm Rust 1.85 or newer and the host's native compiler/linker are installed.
Use `--locked` and the exact alpha version. Prefer a verified binary package if
a source toolchain is not desired.

## Fence says the store is busy

Do not remove lock files. Confirm no `fence run`, restore, doctor, or GC process
is active. Then use `fence recover` to classify stale nonterminal sessions.

## Store permissions are weak or ownership is wrong

Stop mutation and preserve the store. Do not run Fence with `sudo` to work
around the refusal. Restore ownership and private directory permissions using
the same account that created the store, then rerun `fence doctor`. On shared
or managed systems, ask the administrator before changing ownership.

## A restore conflicts

Exit status 4 means Fence preserved current state. Inspect `fence diff
<session> --drift`. Use merge preview only for intended text drift. Never delete
the store or edit a recovery journal to force a restore.

## Doctor reports corrupt objects or unfinished transactions

Preserve a copy of the store and worktree. Run transaction recovery only from
the same or a newer Fence version. If recovery refuses, stop; do not retry on
the only copy. Follow `SECURITY.md` for a suspected integrity bypass or create
a synthetic issue for an ordinary operational failure.

## Safe diagnostics

Share the Fence version, OS/filesystem, command exit status, session UUID, and
sanitized human output. Session objects can contain exact credentials or source
bytes. Never upload `.git/fence`, an external Fence store, or raw journals from
a real repository.
