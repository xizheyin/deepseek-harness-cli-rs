# Phase 0 validation

## Scope

Phase 0 validates the reproducible Rust CLI foundation. It does not claim that a model provider, Agent Loop, tools, sessions, or an interactive terminal exists.

## Local validation

- Date: 2026-08-14 (Asia/Shanghai)
- Git base before the Phase 0 checkpoint: `4d63dd24d2366432e94827eede2099cc519e679a`
- Tested state: Phase 0 working tree that will become the checkpoint commit
- Host: macOS 27.0.0, arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Network used by default verification: no
- Credentials or API keys used: no
- User project data modified: no

Commands run successfully:

```console
./scripts/verify.sh
cargo build --locked
cargo run --locked -- --help
cargo run --locked -- --version
```

`./scripts/verify.sh` ran:

- `cargo fmt --all -- --check`;
- `cargo check --all-targets --all-features --locked`;
- `cargo test --all-targets --all-features --locked`;
- `cargo clippy --all-targets --all-features --locked -- -D warnings`;
- deterministic negative self-tests for staged, modified, and untracked whitespace checks;
- the final repository check, which scans the full tracked tree even in a clean checkout.

Six CLI smoke tests passed. They cover long and short help/version flags, missing and unknown arguments, non-Unicode operating-system arguments on Unix, and explicit rejection of valid upstream profile/Web/plugin/config launcher invocations.

## Failure and safety categories

| Category | Evidence |
| --- | --- |
| Normal | Help and version exit successfully with tested output. |
| Failure | Missing, unknown, and non-Unicode arguments exit 2 with readable errors; no panic. |
| Rejection | Upstream-only launcher modes are explicitly rejected and tested. |
| Cancellation | N/A: Phase 0 starts no long-running work. |
| Timeout | N/A: Phase 0 performs no network or subprocess operation in the product. |
| Recovery | N/A: Phase 0 has no persisted session state. |
| Safety | Default tests are offline/keyless; CI uses read-only repository permission and does not persist checkout credentials. |

## Independent review

Three independent read-only reviews checked code/CI safety, public-document truthfulness, and upstream/attribution accuracy. Findings were corrected before this record:

- non-Unicode arguments previously panicked and now return a usage error;
- whitespace validation now covers clean CI content, staged blobs, modifications, and untracked files without following symlinks outside the repository;
- the whitespace self-test is isolated from user Git signing and Hook configuration and proves the staged-content case independently;
- CI is pinned to Ubuntu 24.04 and `actions/checkout` is pinned to a full commit SHA with read-only permission;
- the CLI intentional differences now name exit-code and launcher-mode behavior and have direct smoke tests;
- the security-report fallback is actionable;
- upstream source/fixture attribution and naming/trademark risks are explicitly documented.

No unresolved implementation, test, documentation, compatibility, or attribution issue was found in the final reviewed tree. The project name remains an explicit maintainer choice; it carries an unofficial-project disclaimer, uses no upstream logo, and must be reconsidered before v0.1 if it could cause confusion or the owner objects.

## Remote acceptance

Pending the Phase 0 checkpoint push and the Ubuntu 24.04 GitHub Actions result. Phase 0 remains `in-progress` until this section records that evidence.
