# Phase 0 foundation decisions

Phase 0 establishes a reproducible project without pretending that an agent already exists.

## Public identity

- Cargo package: `deepseek-harness-cli`
- Executable: `dsh`
- Pre-release version: `0.1.0-alpha.0`
- License: MIT
- Repository: <https://github.com/xizheyin/deepseek-harness-cli-rs>

The `dsh` executable name follows the upstream product name. The project remains an independent community implementation and does not imply affiliation with DeepSeek or Anthropic. The upstream MIT source license is not a trademark grant; this naming choice must be reviewed again before a public v0.1 release and changed if the owner objects or likely user confusion remains.

## Rust baseline

The crate uses Rust edition 2024 and declares Rust 1.85 as its minimum supported Rust version (MSRV). `rust-toolchain.toml` pins 1.85.0 for repeatable local and CI checks. The binary has no third-party dependency in Phase 0; dependencies will be added only when an implemented feature needs them.

## Current CLI behavior

Only `--help`/`-h` and `--version`/`-V` succeed. Starting without arguments, passing unknown arguments, or passing non-Unicode operating-system arguments fails with exit code 2 and a clear message. This prevents an empty scaffold from looking like a working agent and prevents malformed external input from causing a panic.

The official CLI at the pinned revision launches Cordis profiles and includes Web and plugin-management modes. Its Commander-based usage errors return exit code 1. This Rust project intentionally rejects those launcher commands because its product is one standalone terminal coding agent, and it uses exit code 2 to distinguish command-line usage errors from other failures. Users therefore cannot reuse upstream launcher commands or rely on the same error code. Dedicated smoke tests fix these differences, and `docs/compatibility.md` records them.

## Verification

`scripts/verify.sh` is the single entry point for local and CI checks. It performs formatting, compilation, tests, Clippy with warnings denied, and whitespace validation over committed, modified, and untracked project files. Tests are offline and do not read a DeepSeek API key.

The initial CI is configured for Ubuntu 24.04. Phase 0 is not complete until the pushed workflow proves that the skeleton builds there; even after that check, it is not a general operating-system support claim.
