#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
project_root="$(cd -- "$script_dir/.." && pwd)"
capture_root="$(mktemp -d "${TMPDIR:-/tmp}/dsh-readme-capture.XXXXXX")"
mode="${1:-write}"

cleanup() {
  case "$capture_root" in
    */dsh-readme-capture.*) rm -rf -- "$capture_root" ;;
    *) printf 'refusing to clean unexpected capture root: %s\n' "$capture_root" >&2 ;;
  esac
}
trap cleanup EXIT HUP INT TERM

if [[ "$mode" != "write" && "$mode" != "--check" ]]; then
  printf 'usage: %s [--check]\n' "$0" >&2
  exit 2
fi

cd "$project_root"
python3 "$script_dir/render-terminal-snapshot.py" --self-test

DSH_SCREENSHOT_DIR="$capture_root/raw" "$script_dir/accept-phase9.sh"

mkdir -p -- "$capture_root/rendered"
for name in approval overview; do
  python3 "$script_dir/render-terminal-snapshot.py" \
    --input "$capture_root/raw/$name.ansi" \
    --output "$capture_root/rendered/dsh-$name.png" \
    --columns 120 \
    --rows 24
done

asset_root="$project_root/docs/assets"
if [[ "$mode" == "--check" ]]; then
  for name in approval overview; do
    cmp --silent \
      "$capture_root/rendered/dsh-$name.png" \
      "$asset_root/dsh-$name.png" || {
        printf 'README screenshot is stale: docs/assets/dsh-%s.png\n' "$name" >&2
        exit 1
      }
  done
  printf 'README screenshots match the installed-binary PTY capture.\n'
else
  mkdir -p -- "$asset_root"
  for name in approval overview; do
    cp -- "$capture_root/rendered/dsh-$name.png" "$asset_root/dsh-$name.png"
  done
  printf 'Wrote docs/assets/dsh-approval.png and docs/assets/dsh-overview.png.\n'
fi
