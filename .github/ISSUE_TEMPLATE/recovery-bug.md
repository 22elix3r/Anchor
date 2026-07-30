---
name: Recovery bug
about: Report a synthetic restore journal or crash-recovery failure
title: "recovery: "
labels: "recovery"
assignees: ""
---

Do not attach a real Anchor store or proprietary snapshot contents. For a
suspected confidential vulnerability or data-loss path, use the private
security-advisory process described in `SECURITY.md`.

### Revision and environment

- Anchor revision:
- OS and version:
- Filesystem:
- Mount options, containers or user namespace:

### State model

- Base node:
- Session-end node:
- Current node before restore:
- Expected outcome:
- Observed outcome:

### Crash boundary

- Journal kind/schema/state:
- Process crash, machine-power loss, or neither:
- Was `SIGKILL` used:
- Did `anchor recover-transactions --yes` roll back, roll forward, or refuse:

### Integrity report

Paste redacted `anchor doctor --format json` output. Include filenames and
hashes only when they contain no sensitive information.

### Minimal reproduction

Provide a synthetic repository fixture and exact commands that do not require
the `git` executable.

