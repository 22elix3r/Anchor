# Security model

Fence is a local recovery tool, not a sandbox. It observes a worktree during a
command window and can later apply a conservative inverse when retained and
current evidence proves that operation safe. It cannot attribute a write to a
specific child, user, agent, or prompt.

## Assets and trust boundaries

- Worktree and Git metadata are user assets. Capture is read-only with respect
  to both; mutation is opt-in and never writes Git history or refs.
- The Fence store contains exact file bytes and is confidential local data. Its
  private ownership and permissions are checked independently from object
  hashes.
- Manifests, sessions, policies, restore plans, and journals are untrusted
  inputs even when they live in the private store.
- Current filesystem state is authoritative evidence. A retained object does
  not authorize overwriting a current path.
- Package registries, release workflows, maintainer accounts, and local package
  prefixes form the installation trust boundary.

## Controls

- Native relative paths reject absolute, empty, `.` and `..` components and are
  resolved beneath capability-rooted handles.
- Capture does not follow symlinks, records repository boundaries, and excludes
  Git/Fence storage from restore targets.
- Stored objects are length-bounded, decompression-bounded, and content-hash
  verified on read.
- Restore compares base, session-end, and current state; ambiguous drift is a
  conflict, not an overwrite fallback.
- Staging, no-replace operations, byte verification, and durable journals make
  interrupted mutations recoverable without guessing.
- Unix stores require private ownership/mode; Windows stores use a protected
  current-user/SYSTEM DACL.
- npm installation executes no lifecycle script and downloads no binary outside
  npm's normal package fetch. The launcher uses an exact scoped leaf, verifies
  its binary digest, and never searches PATH.
- Releases use signed tags, protected approval, OIDC publishing, checksums,
  SBOMs, third-party notices, and build provenance attestations.

## Privileges and updates

Fence never escalates privileges. Running it as root or administrator expands
the files it can affect and can create privileged stores or restored files, so
permission refusals must not be bypassed with `sudo`. Fence has no self-updater;
operators explicitly install new versions after completing recovery and
running `doctor`.

## Non-goals

Fence does not defend against a fully compromised account that can modify both
the worktree and private store, provide a malicious executable, or take over a
trusted registry/release identity. It does not provide globally atomic
snapshots, causal attribution, malware containment, or recovery proof across
machine power loss. Complete runtime guarantees and refusal boundaries are in
[Safety and threat model](safety.md).
