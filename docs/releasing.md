# Release checklist

This checklist produces the Phase 9 source-install release candidate. It does
not publish to crates.io, create a GitHub Release, or contact the real DeepSeek
API.

## Candidate checks

From a clean candidate commit on Rust 1.85.0:

```console
git status --short
git diff --check
./scripts/verify.sh
./scripts/accept-phase9.sh
```

The first script runs repository-wide formatting, compilation, tests, Clippy,
and whitespace checks. The second installs `dsh` to a temporary root, checks
`--help`/`--version`, then drives the installed release binary through the real
PTY and loopback-Provider journey. Cargo may fetch locked dependencies if the
local registry cache is empty; the Agent scenario itself is offline and keyless.

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
5. Wait for both `ubuntu-24.04` and `macos-14` jobs to pass `verify.sh` and the
   installed-binary journey.
6. Create or update `docs/validation/phase-9.md` with the immutable candidate
   commit and both Actions job URLs.
7. In that separate reviewed status commit, mark Phase 9 complete and Phase 10
   in progress; push it without force and verify its CI as well.

Do not tag or publish artifacts unless the maintainer explicitly authorizes that
separate external release action.
