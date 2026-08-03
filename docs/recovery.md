# First recovery example

Assume a completed session replaced `src/config.rs` and no one changed that
path after the session ended.

## Restore one path

Review the recorded change and current drift:

```console
fence diff <session-id>
fence diff <session-id> --drift
fence restore <session-id> --file src/config.rs
```

The last command is a nonmutating confirmation gate. Apply only after the path,
session, and diff are correct:

```console
fence restore <session-id> --file src/config.rs --yes
```

An exact restore returns one of three outcomes: applied, already safe/no-op, or
conflict. A conflict exits with status 4 and leaves the path unchanged. If
current text drift is intentional, preview the bounded inverse merge and bind
the apply to the returned object ID:

```console
fence restore <session-id> --file src/config.rs --merge
fence restore <session-id> --file src/config.rs --merge --yes \
  --expect-merged <previewed-object-id>
```

## Recover interrupted work

If Fence or the machine stopped during mutation, preserve the worktree and
store. Do not repeatedly retry against the only copy.

```console
fence doctor
fence recover
fence recover-transactions --yes
fence doctor
```

`recover` only marks stale command sessions abandoned after proving their child
lock is free. `recover-transactions` separately validates immutable restore
plans and byte-verifies rollback or post-commit cleanup. It does not guess when
the journal lacks sufficient evidence.

For a whole-session rollback, preview first and apply exactly the returned
current manifest:

```console
fence rollback <session-id>
fence rollback <session-id> --yes \
  --expect-current <previewed-manifest-id>
```

Any ambiguous path rejects the complete batch before target mutation begins.
