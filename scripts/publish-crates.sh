#!/bin/sh
set -eu

if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
    echo "CARGO_REGISTRY_TOKEN is required" >&2
    exit 2
fi

version="$(
    cargo metadata --locked --no-deps --format-version 1 |
        jq -r '.packages[] | select(.name == "fence-cli") | .version'
)"
user_agent="Fence/${version} release publisher (+https://github.com/22elix3r/fence)"
temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/fence-crates.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM

for crate in \
    fence-unix \
    fence-windows \
    fence-core \
    fence-git \
    fence-session \
    fence-tui \
    fence-cli
do
    echo "preflighting ${crate} ${version}"
    cargo publish --dry-run --locked -p "$crate"
    cargo package --locked -p "$crate"
    local_crate="target/package/${crate}-${version}.crate"
    remote_crate="${temporary_directory}/${crate}-${version}.crate"
    download_url="https://crates.io/api/v1/crates/${crate}/${version}/download"

    if curl --fail --silent --show-error --location --user-agent "$user_agent" \
        "$download_url" --output "$remote_crate"
    then
        cmp "$local_crate" "$remote_crate" || {
            echo "published ${crate} ${version} differs from this tag" >&2
            exit 1
        }
        echo "${crate} ${version} already exists with identical bytes"
        continue
    fi

    cargo publish --locked -p "$crate"
    attempt=0
    while ! curl --fail --silent --show-error --location --user-agent "$user_agent" \
        "$download_url" --output "$remote_crate"
    do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 60 ]; then
            echo "timed out waiting for ${crate} ${version} on crates.io" >&2
            exit 1
        fi
        sleep 5
    done
    cmp "$local_crate" "$remote_crate" || {
        echo "downloaded ${crate} ${version} differs from the upload" >&2
        exit 1
    }
done
