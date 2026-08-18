# Release checklist

This checklist verifies the Phase 9 source-install release candidate and the
post-v0.1 Phase 10 subprocess-tool extension. It does not publish to crates.io,
create a GitHub Release, or contact the real DeepSeek API.

## Candidate checks

From a clean candidate commit on Rust 1.85.0:

```console
git status --short
git diff --check
./scripts/verify.sh
./scripts/accept-phase9.sh
./scripts/accept-phase10.sh
```

The first script runs repository-wide formatting, compilation, tests, Clippy,
and whitespace checks. The second installs `dsh` to a temporary root, checks
`--help`/`--version`, then drives the installed release binary through the real
PTY and loopback-Provider journey. The third installs the same candidate, builds
the two real example plugins and fault fixture, then exercises approval,
protocol faults, cancellation, cleanup, and no-replay recovery. Cargo may fetch
locked dependencies if the local registry cache is empty; the Agent scenarios
themselves are offline and keyless.

Regenerate the README images only when the real terminal presentation changes:

```console
./scripts/capture-readme-screenshots.sh
./scripts/capture-readme-screenshots.sh --check
```

The renderer accepts only dsh's audited ANSI subset. It uses the vendored
JetBrains Mono files and a local Chromium rasterizer; do not hand-edit the PNGs.

## Review and publish

1. Check README commands against `tests/release_acceptance.rs` and CLI smoke tests.
2. Confirm the screenshot caption says the model is an offline loopback fixture.
3. Confirm `SECURITY.md`, `docs/configuration.md`, known limits, the target
   platform matrix, and compatibility rows match the candidate.
4. Push the candidate without force.
5. For a Phase 10 candidate, confirm both real examples are reachable through
   the installed CLI and `--plugin-config`, and that configured program paths,
   configured program argv, stderr, and protocol IDs do not enter Session
   JSONL. Model-requested tool arguments must still be recorded before dispatch.
6. Wait for both `ubuntu-24.04` and `macos-14` jobs to pass `verify.sh`, the
   Phase 9 installed-binary journey, and the Phase 10 installed-plugin journey.
7. Create or update the matching `docs/validation/phase-N.md` with the immutable
   candidate commit and both Actions job URLs.
8. Only in a separate reviewed status commit, mark the active phase complete;
   push it without force and verify that status commit's CI as well.

Do not tag or publish artifacts unless the maintainer explicitly authorizes that
separate external release action.
