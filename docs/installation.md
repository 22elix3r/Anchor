# Installation

Fence is prerelease software that can restore and delete filesystem state.
Install an exact version, verify its origin, and try it first in a disposable
repository. Fence has no self-updater and never asks for elevated privileges.

## npm

The npm package contains a prebuilt executable selected by npm. It has no
installation lifecycle scripts and performs no installation-time download.

```console
npm install -g fence-cli@0.1.0-alpha.1
fence --version
```

The package requires Node.js 22.15 or newer and supports GNU/Linux x86-64,
Intel macOS, and Apple silicon macOS. The npm name `fence` belongs to an
unrelated project; the package name is `fence-cli`, while the installed command
is `fence`.

Use a user-owned npm prefix. Do not use `sudo npm install -g`: elevated
installation is unnecessary and can leave globally writable or root-owned
files. If optional dependencies are disabled, the launcher cannot find its
platform binary and exits without touching a Fence store.

## Cargo

Cargo does not select prereleases implicitly, so include the exact version:

```console
cargo install fence-cli --version 0.1.0-alpha.1 --locked
fence --version
```

This requires Rust 1.85 or newer and a working native build toolchain. Cargo
executes dependency build scripts while compiling; users who do not need a
source build should prefer the attested npm or GitHub artifacts.

## GitHub release archive

Download the archive and the central checksum manifest for the host target:

```console
sha256sum --check --ignore-missing fence-0.1.0-alpha.1-SHA256SUMS
gh attestation verify \
  fence-0.1.0-alpha.1-x86_64-unknown-linux-gnu.tar.gz \
  --repo 22elix3r/fence
tar -xzf fence-0.1.0-alpha.1-x86_64-unknown-linux-gnu.tar.gz
install -m 0755 \
  fence-0.1.0-alpha.1-x86_64-unknown-linux-gnu/fence \
  "$HOME/.local/bin/fence"
```

On macOS, use `shasum -a 256 -c` and the appropriate Apple target archive.
Checksums detect corruption; the GitHub attestation and signed release tag bind
the artifact to the repository build. Inspect both when installation trust
matters.

## Upgrade and downgrade

Before changing versions, finish or recover active sessions and run:

```console
fence doctor
fence recover
fence recover-transactions --yes
```

Never downgrade to work around an incomplete recovery journal. An older alpha
may not understand data written by a newer one and must be allowed to refuse it.
Fence does not update itself.

To uninstall, remove the installed executable/package. Uninstallation does not
delete repository stores. Preserve stores until every retained session and
recovery journal is no longer needed.

## Source build

```console
git clone https://github.com/22elix3r/fence.git
cd fence
cargo build --release --locked -p fence-cli
./target/release/fence --version
```

Building from a branch is not equivalent to installing a released version.
