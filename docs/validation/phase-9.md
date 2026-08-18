# Phase 9 validation

## Status

The Phase 9 source-install release candidate is accepted. The immutable
candidate is `adcf2129617049cfeedbbe66ba2bed6bd4d30db3` on `main`, pushed
non-force to `origin/main`. Phase 9 completes the original v0.1 product scope;
Phase 10 is the only active development phase and is not a current CLI feature.

This is still version `0.1.0-alpha.0`. Acceptance does not publish to
crates.io, create a GitHub Release or tag, provide prebuilt binaries, or create
a supported stable release line.

## Candidate identity

- Project brand: `dsh-rs`
- Cargo package: `deepseek-harness-cli`
- Installed command: `dsh`
- Version: `0.1.0-alpha.0`
- Rust toolchain: `1.85.0`
- Branch and remote: `main` on
  `git@github.com:xizheyin/deepseek-harness-rs.git`
- Candidate commit: `adcf2129617049cfeedbbe66ba2bed6bd4d30db3`
- Pinned semantic baseline:
  `deepseek-ai/deepseek-harness@47f943859bef60e4160492346772ded9b24f765a`

The release work was committed in three reviewable checkpoints:

- `eb8d28d916b7637f7fbe0966dca7dede2f4e2bdd`: installed-binary acceptance,
  screenshots, documentation, and the two-platform workflow;
- `15200f4e9e031dd7c86ccd0f227e3872f08a5969`: normalize the signed transient
  PTY allocation error observed on macOS;
- `adcf2129617049cfeedbbe66ba2bed6bd4d30db3`: bound concurrent real-terminal
  fixtures before their loopback-server deadlines begin.

## Accepted product scope

- `cargo install --locked --path .` produces a working `dsh` whose `--help`
  and `--version` are exercised after installation.
- The terminal is scrollback-first rather than full-screen. It has a styled
  prompt, bounded streaming status, readable diffs and Shell previews, plus a
  complete plain `--no-color` fallback.
- Approval uses a visible Allow once / Reject / Cancel selector. Reject is the
  safe default; arrows, Vim keys, Tab, `y`/`n`/`c`, Enter, Escape, Ctrl+C, EOF,
  suspension, and terminating signals have tested behavior.
- Terminal modes are restored after approval, cancellation, suspension, EOF,
  output failure, and supported exit signals.
- The real Agent can list, glob, grep, and read workspace files; apply one
  approved patch; run one approved bounded foreground Bash command; and feed
  correlated results back to the model.
- A normally closed Session can be listed, resumed, cancelled, continued, and
  compacted once without replaying an earlier file or Shell side effect.
- README embeds two images generated from the installed binary's real PTY
  bytes. Configuration, security boundaries, known limits, compatibility, and
  the reproducible release checklist are published beside it.
- The declared matrix is GitHub-hosted `macos-14` on arm64 and
  `ubuntu-24.04` on x86_64. Both run the repository gate and installed-binary
  journey.

## Local validation

- Date: 2026-08-18 (Asia/Shanghai)
- Host: macOS 27.0 (26A5406e), arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Real DeepSeek API use: none
- Credentials: no real key; tests use a conspicuous loopback-only fake key
- User data: none; workspaces, Session roots, and install roots are temporary

The final local candidate passed:

```console
./scripts/verify.sh
./scripts/accept-phase9.sh
./scripts/capture-readme-screenshots.sh --check
git diff --check
```

`cargo test --all-targets -- --list` reports 765 default tests in the final
tree. The full gate completed with zero failures and zero ignored tests.
`release_acceptance` contains two product journeys plus two shared harness
checks; all four pass when the release script points them at the freshly
installed binary. Cargo may fetch locked dependencies during installation, but
the Agent journey itself uses only loopback HTTP and never reads a real key.

The final terminal-fixture shape also passed five consecutive local runs:

```console
for run in 1 2 3 4 5; do
  cargo +1.85.0 test --locked --test interactive_cli
done
```

Each run executed all 40 interactive tests with real PTYs and real child
processes. Only the number of simultaneously live fixtures is bounded so a
small CI host cannot exhaust its PTY allocator.

## Installed release journey

The resumable installed-binary journey performs one connected, offline
scenario:

1. `dsh` lists, greps, and reads a temporary Rust file.
2. The approval selector allows a patch that changes `ANSWER` from 41 to 42.
3. A second approval runs `check.sh`; its side-effect marker appears once.
4. A normal exit is followed by the real `--list-sessions` and `--resume`
   paths.
5. A stalled Provider response is cancelled; the same resumed CLI then
   completes a later turn, and cancelled partial text is not replayed.
6. Another resume crosses context pressure, performs exactly one tool-free
   summary, retains the recent balanced tail, and continues the same pending
   prompt.
7. The patch and Shell side effect remain single-shot across both resumes and
   compaction.

Assertions inspect the actual Provider requests, call IDs, latest tool result,
Session JSONL event adjacency, final workspace bytes, and side-effect marker.

## TUI and screenshot evidence

- `docs/assets/dsh-overview.png`: 1128×558,
  SHA-256 `a457fc09d8c212a1339b26f2d5b2fde58fa9b74b4c7b921d0f402a7c47243299`
- `docs/assets/dsh-approval.png`: 1128×558,
  SHA-256 `1a3807be6e916339ef532c163763cd7550664a314ac36f2d04bb89cbc2ee23cd`

The capture starts the installed candidate in a 120×24 PTY against an offline
loopback fixture and temporary workspace. The PNGs are rendered from those
terminal bytes, not from a mockup or generated prose. The fail-closed renderer
accepts only dsh's audited ANSI subset and a vendored JetBrains Mono font.
Chromium is used only to rasterize the local screenshot artifact; it is not a
product runtime dependency.

## Failure and safety coverage

Default tests cover normal completion, malformed Provider/tool input, Provider
and tool failure, approval rejection, active cancellation, turn/tool timeout,
Session recovery and corruption, terminal restoration, PTY output failure,
Shell process-group cleanup, path/link confinement, output and memory bounds,
and secret redaction. The two-platform candidate also runs the installed
journey rather than accepting library-only evidence.

The safety boundary remains explicit: approved Bash is native code, not a
sandbox; Session JSONL is plaintext convenience state, not a backup; automatic
summary is bounded but not guaranteed lossless; and only the declared platforms
are accepted.

## Compatibility and limits

At acceptance, `docs/compatibility.md` contains:

- 18 `compatible` rows backed by deterministic comparisons;
- 25 tested `intentional-difference` rows;
- 2 `partial` rows, both explicitly named `Post-v0.1` hardening.

The two broad persistence/compaction rows remain partial and are not advertised
as v0.1 behavior. The candidate does not claim database-grade durability,
complete upstream compaction parity, a sandbox, multiple Providers, MCP,
Hooks, Skills, subagents, background jobs, or plugins. Phase 10 may add only the
separately scoped subprocess tool-plugin system after this acceptance.

## Remote acceptance

GitHub Actions [CI run 32113801814](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32113801814)
completed successfully for the exact candidate commit:

- [macOS job 95638746330](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32113801814/job/95638746330),
  `2026-08-18T07:55:50Z`–`2026-08-18T08:00:52Z`;
- [Ubuntu job 95638746342](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32113801814/job/95638746342),
  `2026-08-18T07:55:50Z`–`2026-08-18T08:00:58Z`.

For both jobs, `Run repository checks` and
`Verify the installed release journey` concluded `success`. Earlier runs
32111380055 and 32112313525 exposed the macOS PTY fixture race and are retained
as superseded failure evidence, not counted as acceptance.

The later Phase 9 status commit exposed two more test-harness timing assumptions
without changing the accepted product behavior. Commit `d95a6f7` kept the fake
Provider's five-second first-connect guard but gave follow-up requests a looser,
still bounded deadline so a large durable tool round is judged by its owning
journey timeout. Commit `51996b2` removed wall-clock sleeps from the real-PTY
arrow test; deterministic selector unit tests continue to cover byte-fragmented
escape sequences, while the PTY test covers the complete key and requires Enter
before the file changes.

GitHub Actions [CI run 32117922771](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32117922771)
then completed successfully for `51996b2`:

- [macOS job 95651544017](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32117922771/job/95651544017),
  `2026-08-18T08:44:31Z`–`2026-08-18T08:49:29Z`;
- [Ubuntu job 95651543997](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32117922771/job/95651543997),
  `2026-08-18T08:44:31Z`–`2026-08-18T08:48:53Z`.

Both repository checks and both installed release journeys concluded `success`.
Runs 32115127657 and 32116731276 remain superseded failure evidence for the two
fixture assumptions rather than green acceptance evidence.

## Publication gate

The candidate and status/fix checkpoints were pushed without force. No tag,
GitHub Release, crate publication, Homebrew formula, or external binary artifact
was created. The separate Phase 9 status/fix chain now passes the same
macOS/Ubuntu workflow, so Phase 10 production implementation may proceed from
`51996b2`.
