#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
test_root="$(mktemp -d)"
external_target="$(mktemp)"

# Make this test independent of a contributor's signing, hooks, aliases, or
# other machine-level Git configuration.
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
unset GIT_CONFIG_COUNT

cleanup() {
    rm -rf -- "$test_root"
    rm -f -- "$external_target"
}
trap cleanup EXIT

mkdir -p "$test_root/scripts"
cp "$script_dir/check-whitespace.sh" "$test_root/scripts/check-whitespace.sh"
chmod +x "$test_root/scripts/check-whitespace.sh"
git -C "$test_root" init --quiet

printf 'clean\n' >"$test_root/file.txt"
git -C "$test_root" add file.txt scripts/check-whitespace.sh
"$test_root/scripts/check-whitespace.sh"

printf 'untracked trailing whitespace \n' >"$test_root/untracked.txt"
if "$test_root/scripts/check-whitespace.sh" >/dev/null 2>&1; then
    printf 'error: untracked trailing whitespace was not rejected\n' >&2
    exit 1
fi
printf 'clean\n' >"$test_root/untracked.txt"

printf 'modified trailing whitespace \n' >"$test_root/file.txt"
if "$test_root/scripts/check-whitespace.sh" >/dev/null 2>&1; then
    printf 'error: modified trailing whitespace was not rejected\n' >&2
    exit 1
fi
printf 'clean\n' >"$test_root/file.txt"

printf 'staged trailing whitespace \n' >"$test_root/file.txt"
git -C "$test_root" add file.txt
printf 'clean working tree\n' >"$test_root/file.txt"
if "$test_root/scripts/check-whitespace.sh" >/dev/null 2>&1; then
    printf 'error: staged trailing whitespace was not rejected\n' >&2
    exit 1
fi

printf 'clean\n' >"$test_root/file.txt"
printf 'clean\n' >"$test_root/-leading-dash.txt"
git -C "$test_root" add file.txt
printf 'external target without a newline' >"$external_target"
ln -s "$external_target" "$test_root/link.txt"
"$test_root/scripts/check-whitespace.sh"
