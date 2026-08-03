# Security policy

Fence is pre-release software and has no supported stable version yet. During
the public alpha, only the newest published `0.1.0-alpha.N` receives security
fixes. A report affecting an older alpha must first be reproduced against the
newest release without mutating the reporter's only copy.

Please report suspected path-containment escapes, destructive restoration,
object-verification bypasses, decompression issues, or storage-permission flaws
privately through GitHub's security-advisory interface for this repository.
Do not include real secrets or proprietary snapshot objects in a report.

The project aims to acknowledge a private report within five business days.
Do not open a public issue before the maintainers have evaluated whether an
integrity, containment, or data-loss issue needs coordinated disclosure.

Fence stores exact local file contents. Before sharing a reproduction, build a
minimal synthetic repository and remove the local store under `.git/fence` from
archives.

For data-loss or recovery reports, preserve the affected store and worktree,
stop further Fence mutation, and report the Fence version, operating system,
filesystem, session ID, terminal journal state, and a synthetic
`base/session/current` reproduction. Do not retry recovery on the only copy.

Release binaries are published without a self-updater. Installation packages
must not invoke `sudo`, execute npm lifecycle scripts, or download a binary at
install time. Verify GitHub attestations and checksums as described in
`docs/installation.md`; a checksum obtained from the same compromised channel
is not by itself proof of publisher identity.
