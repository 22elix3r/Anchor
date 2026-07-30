## Scope

Describe the user-visible behavior and the safety boundary changed.

## Safety checklist

- [ ] I identified which modeled `base → session → current` states change.
- [ ] I added or updated explicit restoration-matrix rows.
- [ ] I proved that pre-session and post-session modeled state is preserved.
- [ ] Ambiguous input returns a structured conflict or refusal.
- [ ] Stored paths and journal fields are treated as untrusted.
- [ ] Any new mutation transition has rollback/roll-forward and process-crash tests.
- [ ] Capture changes do not write Git state or worktree content.
- [ ] Persistent schema changes preserve identity or define an explicit refusal boundary.
- [ ] Diagnostics and non-guarantees were updated.
- [ ] No Windows mutation claim was introduced without Windows-specific evidence.

## Verification

Paste the exact commands run. Safety-critical changes should include:

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

## Recovery evidence

For mutation changes, list each durable journal boundary exercised and the
expected recovery direction. For data-format changes, identify old-schema
fixtures and migration/refusal tests.

