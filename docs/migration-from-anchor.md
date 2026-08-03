# Migration from Anchor

Fence is a hard pre-alpha rename. It intentionally provides no `anchor` binary
alias, `ANCHOR_*` environment fallback, `.anchorignore` behavior, or automatic
store import.

Do not rename or move a legacy Anchor store into Fence's store location. Keep
the old worktree and store intact until its retained sessions are no longer
needed. If Fence detects a legacy store for the same repository, `fence doctor`
reports it and session start or mutation refuses instead of guessing.

There is no automatic migration tool in `0.1.0-alpha.1`. To begin using Fence:

1. Finish recovery using the exact compatible Anchor version, against a copy if
   recovery is uncertain.
2. Preserve or archive the legacy store separately.
3. Remove legacy environment variables and aliases from the new shell.
4. Configure Fence independently and run `fence doctor`.
5. Start a new Fence session only after the ambiguity refusal is resolved.

Opaque pre-alpha wire-domain strings retained for forensics do not imply store
compatibility. Never downgrade Fence or substitute Anchor while a Fence journal
is unfinished.
