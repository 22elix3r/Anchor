# Contributing

Anchor's restoration code is safety-critical. Changes should prefer a visible
refusal over an inferred overwrite.

## Development checks

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

The MSRV is Rust 1.85. Do not raise it or enable broad `gix` feature sets without
an architecture discussion.

## Boundaries

- `anchor-core` must remain independent of the CLI, terminal UI, and Git.
- `anchor-unix` and `anchor-windows` quarantine reviewed native FFI. Their
  public APIs must accept already-open handles where a path lookup would create
  a race, and every unsafe block requires a local safety argument.
- `anchor-git` is read-only. Do not add calls that mutate refs, the index,
  configuration, worktree, or Git object storage.
- `anchor-session` owns persistence, process execution, locking, restoration,
  and maintenance orchestration.
- `anchor-cli` presents stable human and machine output.
- `anchor-tui` must consume core/session APIs rather than embedding restore
  decisions.

Every restoration change needs state-matrix tests proving pre-session and
post-session bytes survive. Filesystem mutation changes also need injected-race
or evacuation/rollback tests on each supported platform.

Start safety-critical review with the [independent audit
guide](docs/audit-guide.md). Changes to capture authorization, restore
planning, path verification, journals, recovery, index replacement, object
publication, or garbage collection should be submitted as small semantic
commits and receive review from someone other than the author before release.
Pure code movement must be separate from behavior changes so reviewers can
confirm that mutation semantics did not move at the same time.

Runtime code must not invoke the `git` executable or require network access.
Tests may eventually use Git as a parity oracle but must also retain executable-
free fixtures.

Contributions are dual licensed under MIT OR Apache-2.0 unless explicitly stated
otherwise.
