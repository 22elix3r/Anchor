# Quickstart

Use a disposable repository for the first run:

```console
mkdir fence-demo
cd fence-demo
git init
printf 'base\n' > example.txt
git add example.txt
git commit -m base

fence run -- sh -c 'printf "session change\n" > example.txt'
fence status
fence sessions
```

Fence initializes its private store lazily during the first `fence run`; there
is no separate `init` command. Capture is the before/after work performed by
`run`; there is no separate `capture` command.

Copy the session UUID printed by `run`, then inspect it:

```console
fence show <session-id>
fence diff <session-id>
fence diff <session-id> --current
fence doctor
```

`diff` exits with status 1 when differences exist. That status means
“differences found,” not an execution failure.

Preview a single-file restore:

```console
fence restore <session-id> --file example.txt
```

The preview makes no change. After reviewing the diff, apply it:

```console
fence restore <session-id> --file example.txt --yes
```

Fence applies only a byte-verified inverse and refuses post-session drift. Read
the [first recovery example](recovery.md) and [safety model](safety.md) before
using batch rollback on important work.
