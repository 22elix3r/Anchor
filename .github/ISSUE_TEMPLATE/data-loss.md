---
name: Potential data loss
about: Route a suspected modeled-state overwrite to the security process
title: "potential data loss: "
labels: "data-loss"
assignees: ""
---

If this report may reveal a new destructive path, stop here and open a private
GitHub security advisory as required by `SECURITY.md`. Do not publish real
secrets, object-store files, proprietary paths, or recovery journals.

For a non-confidential synthetic reproduction, provide:

### Violated invariant

- [ ] Pre-session modeled state changed or disappeared.
- [ ] Post-session modeled state changed or disappeared.
- [ ] Fence wrote a protected Git/store path.
- [ ] Fence presented degraded state as safely restorable.
- [ ] Recovery chose a direction despite ambiguity.

### Exact three-state fixture

- Base:
- Session end:
- Current before operation:
- Operation and confirmation flags:
- Actual final state:

### Environment

- Fence revision:
- OS/filesystem:
- `fence doctor --format json` (redacted):
- Relevant journal schema/state:

### Reproduction

Use only synthetic content. State whether the failure reproduces after a normal
error, an external process kill, or machine-power loss.

