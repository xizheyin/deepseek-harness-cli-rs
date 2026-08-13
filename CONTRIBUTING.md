# Contributing

Thank you for helping build `dsh`. The project is pre-release, and every implemented behavior must remain honest, testable, and grounded in the pinned DeepSeek Harness revision.

## Prerequisites

- Git
- Rust 1.85.0 through `rustup`

The checked-in `rust-toolchain.toml` selects the expected compiler, formatter, and Clippy version automatically.

## Local checks

From any directory, run:

```console
./path/to/deepseek-harness-cli-rs/scripts/verify.sh
```

From the repository root, the shorter form is:

```console
./scripts/verify.sh
```

This is the same entry point used by CI. Default tests must not access the public network, read a real API key, consume model credit, or modify a developer's real project.

## Change requirements

- Keep changes focused and preserve unrelated worktree changes.
- Add tests for behavior and regressions, including relevant error and safety paths.
- For agent-core behavior, record exact upstream source/tests in `docs/upstream.md` and update `docs/compatibility.md`.
- Do not label behavior `compatible` without deterministic comparison evidence.
- Update README only when the real CLI exposes a tested user capability.
- Never commit credentials, `.env` files, local sessions, logs, caches, personal agent instructions, or an upstream repository clone.

The project deliberately starts as one small crate. Add modules, traits, dependencies, or workspace members only when implemented behavior needs them.
