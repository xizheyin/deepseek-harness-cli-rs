#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
project_root="$(cd -- "$script_dir/.." && pwd)"
install_root="$(mktemp -d "${TMPDIR:-/tmp}/dsh-phase9-install.XXXXXX")"

cleanup() {
  case "$install_root" in
    */dsh-phase9-install.*) rm -rf -- "$install_root" ;;
    *) printf 'refusing to clean unexpected install root: %s\n' "$install_root" >&2 ;;
  esac
}
trap cleanup EXIT HUP INT TERM

cd "$project_root"
cargo +1.85.0 install --locked --path . --root "$install_root"

installed_dsh="$install_root/bin/dsh"
"$installed_dsh" --version
"$installed_dsh" --help >/dev/null

python3 "$script_dir/render-terminal-snapshot.py" --self-test

DSH_TEST_BINARY="$installed_dsh" \
  cargo +1.85.0 test --locked --test release_acceptance -- --nocapture
