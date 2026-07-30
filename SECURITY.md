# Security policy

Fence is pre-release software and has no supported stable version yet.

Please report suspected path-containment escapes, destructive restoration,
object-verification bypasses, decompression issues, or storage-permission flaws
privately through GitHub's security-advisory interface for this repository.
Do not include real secrets or proprietary snapshot objects in a report.

Fence stores exact local file contents. Before sharing a reproduction, build a
minimal synthetic repository and remove the local store under `.git/fence` from
archives.
