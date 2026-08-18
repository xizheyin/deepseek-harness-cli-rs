#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
project_root="$(cd -- "$script_dir/.." && pwd)"
install_root="$(mktemp -d "${TMPDIR:-/tmp}/dsh-phase10-install.XXXXXX")"

cleanup() {
  case "$install_root" in
    */dsh-phase10-install.*) rm -rf -- "$install_root" ;;
    *) printf 'refusing to clean unexpected install root: %s\n' "$install_root" >&2 ;;
  esac
}
trap cleanup EXIT HUP INT TERM

cd "$project_root"
cargo +1.85.0 install --locked --path . --root "$install_root"
cargo +1.85.0 build --locked --examples

installed_dsh="$install_root/bin/dsh"
text_stats_plugin="$project_root/target/debug/examples/text_stats_plugin"
json_format_plugin="$project_root/target/debug/examples/json_format_plugin"
fault_plugin="$project_root/target/debug/examples/fault_plugin"

"$installed_dsh" --version
"$installed_dsh" --help >/dev/null

DSH_TEST_BINARY="$installed_dsh" \
DSH_TEXT_STATS_PLUGIN="$text_stats_plugin" \
DSH_JSON_FORMAT_PLUGIN="$json_format_plugin" \
DSH_FAULT_PLUGIN="$fault_plugin" \
  cargo +1.85.0 test --locked --test plugin_examples -- --nocapture
