# Architecture

Fence separates presentation, durable orchestration, snapshot primitives, Git
awareness, and native filesystem operations:

```text
fence-cli / fence-tui
          |
     fence-session
       /       \
fence-core   fence-git
       \       /
 fence-unix / fence-windows
```

- `fence-cli` parses commands, renders the versioned JSON or human interface,
  and maps documented outcomes to exit statuses.
- `fence-tui` reviews an already constructed model and delegates mutation to
  the same restore service as the CLI.
- `fence-session` owns locks, before/after lifecycle, durable records,
  configuration, maintenance, restore journals, and recovery.
- `fence-core` owns native path records, manifests, object verification,
  capture, diffs, merge bounds, restore planning, and capability-rooted store
  access.
- `fence-git` discovers worktrees and reads repository, index, tracked-path,
  and ignore state without invoking `git` or modifying Git data.
- `fence-unix` and `fence-windows` quarantine the small native API surfaces
  needed to prove filesystem metadata and namespace behavior.

## Capture data flow

1. Discover a non-bare worktree and its private Fence store.
2. Freeze the inclusion policy and capture repository/index observations.
3. Capture the before manifest and immutable raw-byte objects.
4. Run the child with inherited terminal or console streams.
5. Capture the session-end state with the same frozen policy.
6. Persist the terminal session state and report the child exit status.

The window is an observation boundary, not process attribution. Descendants or
other users may write during the same interval.

## Restore data flow

1. Load retained before/session-end evidence.
2. Capture current state and construct a three-state plan.
3. Refuse drift or metadata that prevents a proven inverse.
4. Preview without mutation.
5. On a bound confirmation, stage outputs and persist a recovery journal.
6. Apply with no-follow/no-replace primitives and verify every target.
7. Mark the durable commit point, then remove backups.

The object store is content-addressed, but a hash is not authorization to write
a path. Path containment, current-state verification, metadata observations,
locks, and the recovery journal are separate safety requirements.
