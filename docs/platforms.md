# Supported platforms

Support means the published artifact is built and executed natively and the
release smoke workflow covers capture, diff, restore, doctor, and GC.

| Platform | Alpha package support | Notes |
|---|---|---|
| Ubuntu 22.04/24.04 x86-64 | Supported | GNU/Linux artifact; glibc 2.35 baseline |
| Fedora x86-64 | Supported after release-artifact smoke | GNU/Linux filesystems only |
| Arch x86-64 | Supported after release-artifact smoke | Current pinned CI image |
| macOS 13.5+ Intel | Supported | Built and tested on native Intel runner |
| macOS 13.5+ Apple silicon | Supported | Built and tested on native ARM runner |
| Windows x64 | Source-level experimental | No npm or direct binary in `alpha.1`; mutation remains publicly refused |
| Windows ARM | Unsupported | No release artifact |
| Linux ARM | Unsupported | No release artifact |
| Alpine/musl | Unsupported | The Linux npm package requires glibc |

Network filesystems, unusual case-sensitive macOS directories, non-NTFS
Windows volumes, antivirus sharing interference, and privilege-boundary use
remain outside the first alpha's claims unless a release note explicitly adds
coverage.
