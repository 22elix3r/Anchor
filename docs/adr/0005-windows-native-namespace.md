# ADR 0005: Native Windows Namespace Boundary

## Status

Implemented; Windows support remains experimental while the real-filesystem
compatibility matrix grows.

## Decision

Anchor uses a small internal `anchor-windows` crate for Windows namespace inspection. It opens
descendants with `FILE_FLAG_OPEN_REPARSE_POINT`, enumerates directories through
`FileIdExtdDirectoryInfo`, and validates the 128-bit `FileIdInfo` after every name-based open.

The selected root may be resolved once. Its final path is reopened and pinned. Descendant reparse
points are classified but never followed.

`cap-std` remains the Unix capability layer but is not Anchor's Windows containment boundary.

## Evidence

The cached `cap-primitives` 4.0.2 Windows backend asserts that no-follow `read_dir` is not
implemented. Its fallback uses `std::fs::read_dir` over a concatenated path. That cannot provide the
identity and reparse checks required by Anchor's restore threat model.

The Win32 APIs expose the required primitives:

- `CreateFileW(FILE_FLAG_OPEN_REPARSE_POINT)` for no-follow opens;
- `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` for raw names, reparse tags, and 128-bit
  IDs;
- `GetFileInformationByHandleEx(FileIdInfo)` for post-open identity validation;
- `FSCTL_GET_REPARSE_POINT` for bounded reparse payload inspection;
- `GetFileInformationByHandleEx(FileStreamInfo)` for alternate-stream detection.
- `NtCreateFile` with a pinned `RootDirectory` for handle-relative staging;
- `SetFileInformationByHandle(FileRenameInfo)` for no-replace evacuation and install;
- a kill-on-close Job object for wrapper-crash child containment.

## Consequences

- Unsafe FFI is confined to `anchor-windows`; every other workspace crate keeps
  `unsafe_code = "forbid"`.
- Unknown or malformed reparse data fails closed.
- An entry replaced between enumeration and open is reported as unstable.
- Unknown reparse tags, hard-link topology, EFS content, cloud placeholders,
  and named streams fail complete capture before child launch.
- Restore uses a separate durable Windows journal and retains backups until all
  desired endpoints verify.
