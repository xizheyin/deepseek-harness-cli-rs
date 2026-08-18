#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
project_root="$(cd -- "$script_dir/.." && pwd)"
cd "$project_root"

cargo fmt --all -- --check
cargo check --all-targets --all-features --locked
cargo build --examples --all-features --locked
cargo test --all-targets --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
"$script_dir/test-check-whitespace.sh"
"$script_dir/check-whitespace.sh"
