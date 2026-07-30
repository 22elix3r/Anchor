#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 PATH-TO-ANCHOR" >&2
    exit 2
fi

case "$1" in
    /*) anchor_binary=$1 ;;
    *) anchor_binary="$(pwd)/$1" ;;
esac

test_root="$(mktemp -d "${TMPDIR:-/tmp}/anchor-release-smoke.XXXXXX")"
trap 'rm -rf "$test_root"' EXIT HUP INT TERM

mkdir -p "$test_root/.git/objects" "$test_root/.git/refs/heads"
printf 'ref: refs/heads/main\n' > "$test_root/.git/HEAD"
printf '[core]\n\trepositoryformatversion = 0\n\tbare = false\n' \
    > "$test_root/.git/config"
printf 'base' > "$test_root/file"

cd "$test_root"
run_output="$(
    PATH=/anchor-release-smoke-path-without-git \
        "$anchor_binary" run -- /bin/sh -c 'printf session > file' 2>&1
)"
session_id="$(
    printf '%s\n' "$run_output" |
        sed -n 's/^Anchor session \([^:]*\):.*/\1/p'
)"
test -n "$session_id"

set +e
PATH=/anchor-release-smoke-path-without-git \
    "$anchor_binary" diff "$session_id" --format json \
    > diff.json
diff_status=$?
set -e
test "$diff_status" -eq 1
grep '"status": "modified"' diff.json >/dev/null

PATH=/anchor-release-smoke-path-without-git \
    "$anchor_binary" restore "$session_id" --file file --yes --format json \
    > restore.json
test "$(cat file)" = base
PATH=/anchor-release-smoke-path-without-git \
    "$anchor_binary" doctor --format json > doctor.json
PATH=/anchor-release-smoke-path-without-git \
    "$anchor_binary" gc --dry-run --format json > gc.json
