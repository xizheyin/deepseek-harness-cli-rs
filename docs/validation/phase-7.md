# Phase 7 validation

## Status

Phase 7 is **complete**. The corrected checkpoint passed the complete local
macOS and Ubuntu 24.04 verification suites, independent review, and the exact
GitHub Actions Ubuntu gate.

The implementation checkpoint
`118e07f8ad87ba656a5acd9f8c977cbe779a219d` was committed and pushed without
force after the earlier `.git/index.lock` permission problem was removed. Its
Ubuntu run failed because one test compared two PTY master descriptors. Linux
opens all PTY masters through the same `/dev/ptmx` clone device, so that did not
construct the intended mismatched-device case. Production validation was not
relaxed: checkpoint `7dd7362af49e6dcea4b03f51fe3825c379a4fd43`
changes only the test to compare concrete PTY slave devices and has also been
pushed without force. Its exact Ubuntu workflow is green.

## Scope

Phase 7 connects the real `dsh` executable to the existing DeepSeek Provider,
Agent Loop, append-only Session, approval pipeline, and six local tools. One
process can now hold multiple turns in memory, show model text only after its
source event commits, request a fresh one-shot terminal challenge before a file
change or Shell call, and continue after cancelling a turn.

The same executable has two deliberately small product modes:

- an interactive, line-oriented macOS/Linux terminal with `/help`, `/exit`,
  `/quit`, Ctrl+C cancellation, Ctrl+D exit, and cleanup-before-Ctrl+Z job
  control; and
- a one-turn `--prompt` or piped-input mode that waits for Agent settlement,
  writes only the authoritative final assistant text to stdout, and rejects
  operations that would need human approval.

The terminal is plain text rather than a full-screen TUI. Phase 7 does not add
durable storage, process restart recovery, session listing/resume, or context
compaction; those are Phase 8 work.

## Local validation

- Date: 2026-08-15 (Asia/Shanghai)
- Git base before Phase 7:
  `0f7938c093f1f98a300d915c8b6c98a6b9a50445`
- Tested Phase 7 checkpoint:
  `7dd7362af49e6dcea4b03f51fe3825c379a4fd43`
- Total default/all-feature Rust tests: 493
- Ignored tests: 0
- Library tests: 238
- Integration and CLI tests: 255
- Host: macOS 27.0 (build 26A5406e), arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Public-network use: none; HTTP tests bind only loopback addresses
- Credentials: no real key; tests use conspicuous fake values and scan every
  PTY byte across read boundaries for disclosure
- User project data: none; all file, Shell, terminal, and Git-facing tests use
  fresh temporary workspaces

The repository-wide local gate passed on the final source tree:

```console
./scripts/verify.sh
git diff --check
```

`verify.sh` ran formatting, the locked all-target/all-feature build, all default
tests, Clippy with warnings denied, deterministic whitespace tests, and the
complete-tree whitespace check. The final count is 493/493 with zero ignored.

Phase 7 contributes 143 named scenarios. One replaces an older CLI smoke test,
so the repository grows by 142 tests from Phase 6's accepted 351:

- 93 library tests for CLI parsing, rendering, terminal facts, approval joining,
  signal and script state, Session observation, fallible entropy, and Workspace
  staging;
- 7 Agent Loop fairness and ACP behavior comparisons;
- 12 real script/startup scenarios, with one replacing an older smoke test;
- 1 real Workspace approval oracle comparison; and
- 30 real-binary/support PTY scenarios.

The particularly relevant final suites contain 51 `agent_loop`, 19
`file_changes`, 17 `cli_smoke`, and 30 `interactive_cli` tests.

Two final whole-tree reruns exposed test-harness races rather than product
failures. First, a Bash job-control fixture compared the foreground job against
Readline's temporary prompt mode; it now lets the resumed foreground job run
`stty sane`, publish a ready file, and wait for the parent to confirm the
foreground group before `exec dsh`. The terminating-signal case passed five
focused reruns and the Ctrl+Z/foreground-resume case passed three. Second,
parallel macOS PTY allocation could transiently return `ENXIO`; the test-only
allocator now retries for one bounded three-second window and every positive PTY
starts from explicit canonical terminal facts before a negative test removes one
fact. The complete 30-test PTY target then passed three consecutive parallel
reruns before the final repository gate.

The focused repetition commands were:

```console
for run_index in 1 2 3 4 5; do
  cargo +1.85.0 test --test interactive_cli \
    terminating_signals_override_a_pending_ctrl_z_after_shell_cleanup \
    --locked -- --exact --test-threads=1
done

for run_index in 1 2 3; do
  cargo +1.85.0 test --test interactive_cli \
    ctrl_z_cleans_an_approved_shell_group_before_bash_fg_resumes_dsh \
    --locked -- --exact --test-threads=1
done

for run_index in 1 2 3; do
  cargo +1.85.0 test --test interactive_cli --locked
done
```

The additional documentation and optimized-production gates also passed:

```console
RUSTDOCFLAGS='-D warnings' \
  cargo +1.85.0 doc --locked --no-deps --document-private-items

cargo +1.85.0 test --release --locked \
  --test cli_smoke -- --test-threads=1

cargo +1.85.0 test --release --locked \
  --test interactive_cli -- --test-threads=1

cargo +1.85.0 tree --locked --edges normal -e features \
  -i uuid@1.18.1

cargo +1.85.0 tree --locked --edges normal -e features \
  -i rustix@1.1.4
```

The two optimized suites passed 17/17 and 30/30. The normal production feature
tree does not enable UUID v4 generation or rustix's test-only `event` feature.
A fresh release LLVM-IR build of the library and binary contained no PTY
dependency, injected entropy source, or named Session/Agent/Workspace/process
test seam. It retained the intended
`entropy::system_fill` to `getrandom::fill` production path.

The release IR was generated and inspected from a fresh target directory:

```console
release_target="$(mktemp -d /tmp/dsh-phase7-release.XXXXXX)"

CARGO_TARGET_DIR="$release_target" \
  cargo +1.85.0 rustc --release --locked --lib -- --emit=llvm-ir

CARGO_TARGET_DIR="$release_target" \
  cargo +1.85.0 rustc --release --locked --bin dsh -- --emit=llvm-ir

ir_files=(
  "$release_target"/release/deps/deepseek_harness_cli-*.ll
  "$release_target"/release/deps/dsh-*.ll
)
test "${#ir_files[@]}" -eq 2
for ir_file in "${ir_files[@]}"; do test -f "$ir_file"; done

forbidden='attach_ui_observer_for_test|word_len_for_test|word_capacity_for_test|allocated_bytes_for_test|capacity_is_acceptable_for_test|from_words_for_test|fail_next_projection|fault_handle_for_test|summarize_turn_for_test|ttyname_error_for_test|render_for_test|with_entropy_for_test|MutationCommitTestPhase|MutationCommitTestHook|ProcessTestHooks|with_test_hooks|InjectedSignalError|OwnedScriptSummary|EntropySource.*injected|new_v4|pty_process|fake_key_scanner'

if rg -n "$forbidden" "${ir_files[@]}"; then
  echo "release IR contains a test-only seam"
  exit 1
fi

rg -n 'system_fill|getrandom' "${ir_files[@]}"
```

The locked graph remains compatible with Rust 1.85. Phase 7 changes the graph
only where the new production boundary requires it:

- direct `getrandom 0.4.3` provides fallible random bytes without a panic-based
  UUID path; it is MIT OR Apache-2.0;
- `tokio 1.53` adds `signal` and `sync` for owned signal streams and bounded
  approval/event channels;
- the normal `rustix 1.1.4` dependency adds only `termios` for production
  terminal facts; its `event` feature is enabled only by the target-specific
  dev dependency used for the PTY test reader's bounded poll;
- `uuid 1.18.1` keeps v4 generation only in dev dependencies; and
- target-only dev dependency `pty-process 0.5.3`, with default features off,
  gives macOS/Linux tests a controlling PTY and is absent from the release
  binary.

Unsafe remains denied by default. The small terminal platform module isolates
the fixed-buffer `ttyname_r`/terminal-capacity calls, documents their pointer and
buffer assumptions, and immediately returns to safe Rust types.

## Upstream behavior validation

The official checkout was clean before and after validation at fixed commit
`47f943859bef60e4160492346772ded9b24f765a`. The focused command was:

```console
pnpm exec vitest run --configLoader runner --config vitest.config.ts \
  packages/acp/acp/tests/bridge.spec.ts \
  packages/acp/acp/tests/turns.spec.ts \
  packages/acp/acp/tests/approval.spec.ts \
  packages/acp/acp/tests/edges.spec.ts \
  packages/interaction/user-approval/tests/approval.spec.ts \
  packages/bundle/headless/tests/headless.spec.ts \
  packages/bundle/headless/tests/startup.spec.ts \
  packages/fs/tool-fs/tests/diff.spec.ts \
  packages/client/ui-primitives/tests/diff-block.client.spec.tsx \
  packages/client/ui-tool/tests/diff-card.client.spec.tsx
```

Vitest 4.1.8 reported 10 files and 134/134 passing tests with no skipped,
pending, or todo cases: ACP 39, approval 32, headless 14, filesystem diff 12,
and client diff projections 37. `--configLoader runner` avoids writing Vite's
temporary config into the read-only upstream checkout without changing the
selected tests.

Node 26.0.0, pnpm 11.19.0, TypeScript 6.0.3, tsx 4.22.4, Vitest 4.1.8, and
Oxlint 1.76.0 then checked and generated the Phase 7 oracle twice:

```console
node /Users/xizheyin/workspace/ds-harness-rs/scripts/typecheck-upstream-interactive-fixtures.mjs \
  /Users/xizheyin/workspace/deepseek-harness-upstream

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-interactive-fixtures.ts \
  /tmp/upstream-phase7-a.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-interactive-fixtures.ts \
  /tmp/upstream-phase7-b.json

cmp -s /tmp/upstream-phase7-a.json /tmp/upstream-phase7-b.json
cmp -s /tmp/upstream-phase7-a.json \
  /Users/xizheyin/workspace/ds-harness-rs/tests/fixtures/cli/upstream_phase7_oracle.json
```

The checker passed, both generated files were byte-identical, and both matched
the committed fixture. SHA-256:

- checker: `e22d39db1f9b9b1a363c552636aa381463339cc2a95bd97efb77aab275579eee`;
- generator: `f99046dfbf0a04ff95affb23a9ef56efcb4ed2e87ab9c803317bac3c607cc794`;
- fixture: `bc293b7e6868cf54acb03edf2534891e8bb0bf0360c49f6a6b2c693a4f464b2f`.

Four default Rust comparisons consume that fixture directly:

- `two_turns_match_the_committed_phase7_acp_oracle`;
- `cancellation_then_continuation_matches_the_committed_phase7_acp_oracle`;
- `approval_outcomes_match_the_committed_phase7_oracle_scope`; and
- `real_script_output_matches_the_committed_phase7_headless_oracle_scope`.

They compare turn/step boundaries, committed chunk order, later-request context,
approval asked/decided relationships, correlated tool results, final text, exit
classification, and real file side effects. Terminal appearance, key bindings,
local slash commands, exact random IDs/timestamps, official profiles, Web
Queue/Steer, and ACP wire encoding are explicitly outside those narrow claims.

## Behavior, failure, and safety categories

| Category | Evidence |
| --- | --- |
| Normal | A real `dsh` process reaches the real Provider → Agent → Session → tool path. PTY tests prove text appears before response completion, later prompts reuse one in-memory Session, read-only tools report stable requested/result states, approved file and Shell actions execute once, and script mode prints only the authoritative final assistant text. |
| Failure | Closed arguments, partial-terminal startup, unsupported terminal facts, missing workspace/key, entropy failure, Provider/HTTP failure, observer failure, output failure, broken pipe, and Agent event/byte exhaustion all return stable bounded outcomes. A 4,096-event or 16-MiB Session endpoint renders its one final error and exits instead of offering a false next prompt. |
| Rejection | Script mode uses deny policies and never waits for an answer. Interactive reject/cancel, EOF/HUP during approval, stale/partial/continuous pre-input, a challenge without LF, an invalid record, and a preview write failure cannot authorize a side effect. Only the exact fresh `allow <challenge>` line after the input fence settles allowed-once. |
| Cancellation | Ctrl+C during a stream or approval cancels the same owned turn, waits for Agent/tool/process cleanup, records the durable aborted boundary, and then offers a fresh prompt. Ctrl+D, HUP, QUIT, and TERM also keep the pinned turn future alive until cleanup settles before exiting with their stable reason. |
| Timeout and backpressure | Non-reading and one-byte-progress PTYs exercise the absolute five-second output/final-drain deadline. A large committed backlog shares one deadline rather than receiving five seconds per event. INT, TSTP, or TERM discards nonessential backlog while continuing resource cleanup. |
| Job control | A real interactive Bash wrapper proves Ctrl+Z first cancels an approved TERM-trapping Shell group, waits until that group is gone, and only then stops `dsh`. Bash `bg` causes the background `dsh` to self-stop without terminal writes; `fg` revalidates the terminal and redraws. HUP/QUIT/TERM override a pending suspend. |
| Terminal integrity | Interactive startup opens independent nonblocking descriptors for one concrete character device without changing inherited descriptor flags. It requires the expected session, foreground group, canonical special characters, `ECHOCTL`, `OPOST`, and `ONLCR`; rejects Linux non-N_TTY/EXTPROC and platform capacity failures; flushes at input fences; and never changes terminal attributes. Exact 1,000/1,001-byte and non-LF Ctrl+D records are covered. |
| Output and privacy | One visible-control renderer escapes C0/C1 controls, ESC/OSC sequences, bidi controls, and invisible format marks in model, tool, diff, path, and diagnostic fields. The fake API key scanner examines the complete PTY byte stream across chunk boundaries, independently of the bounded transcript tail; script stdout/stderr and Session JSON are also checked. |
| Replay and recovery | Two-turn and cancel-then-continue tests derive UI and later Provider context from one append-only in-memory Session. The observer publishes only after a successful Session commit and cannot roll one back. Process-exit persistence, list/resume, damaged-tail repair, and compaction remain Phase 8 work. |
| Resource safety | Input, prompt, preview, frame, transcript, approval, observer, copied-string, source bitmap, event count, Session bytes, HTTP request/response, output time, signal drains, and PTY cleanup all have explicit limits with exact/one-over or fault tests. Cooperative Agent yields prevent an always-ready Provider or tool group from starving signals and deadlines. |

## Independent review

Independent read-only tracks reviewed the Session observer and source bitmap,
Agent cooperative yields and entropy failures, approval joining and stale-input
fences, script input/output and signal ordering, terminal descriptor ownership,
interactive cleanup/job control, PTY resource behavior, upstream comparators,
release feature/test-seam absence, and documentation truthfulness.

Those reviews found and closed concrete issues before this record: post-commit
observer failure could not roll back Session truth; raw tool arguments were
removed from UI projection; final-answer mismatch now replays the whole
authoritative answer; active cleanup polls its pinned Agent future even under a
signal flood; idle and script completion resample signals at their defined
reactor settlement point; approval does not accept bytes from before its fresh
challenge fence; final backlog uses one deadline; `/dev/tty` is reopened through
the concrete terminal path with a distinct open-file description; and PTY
privacy scanning no longer depends on a rolling tail.

The final code, release, and documentation reviews reported no local production
blocker after the focused and whole-tree gates. Remote Ubuntu evidence remains a
separate required gate, not an inferred result of macOS review.

## Known limitations

- Sessions are memory-only. Exiting loses the conversation; persistence,
  listing, resume, damaged-tail repair, and compaction belong to Phase 8.
- Interactive input is canonical line input, not a full-screen editor. There is
  no history search, cursor-editing model, Markdown renderer, syntax highlight,
  mouse support, raw mode, or ANSI color theme.
- Only one prompt is admitted at a time. Input typed while a turn is busy is
  discarded; upstream Web Queue/Steer behavior is not implemented.
- Model text streams after Session commit, but Shell stdout/stderr is displayed
  only after the bounded command settles.
- Script mode cannot stop for an approval, so file changes and Shell calls are
  denied. Its narrow compatible claim covers final text/LF and completed versus
  non-completed exit classification, not upstream profile syntax or JSONL.
- An approved Shell command is unsandboxed native code with the current user's
  permissions. The Phase 6 process owner cleans its observed group but cannot
  make native code safe or guarantee cleanup across SIGKILL, machine loss, or
  an uninterruptible kernel wait.
- Session limits are 4,096 events and 16 MiB of retained compact JSON. Reaching
  either limit safely ends the interactive process rather than silently
  dropping history.
- Phase 7 claims real terminal behavior only on macOS and Ubuntu 24.04 after the
  remote gate below passes. Windows and other systems are not implemented or
  claimed.
- No real DeepSeek request was sent. The production binary path is proven with
  a real DeepSeek adapter against bounded loopback SSE servers and conspicuous
  fake credentials.

## Remote acceptance

The first exact-commit run is retained as failure evidence rather than hidden:

- GitHub Actions run
  [`31842310652`](https://github.com/xizheyin/deepseek-harness-cli-rs/actions/runs/31842310652)
  tested `118e07f8ad87ba656a5acd9f8c977cbe779a219d` on
  `ubuntu-24.04`;
- checkout and the pinned Rust installation succeeded;
- `./scripts/verify.sh` failed with exit 101 after 2 minutes 14 seconds in the
  `Run repository checks` step; and
- a local Ubuntu 24.04.4 reproduction identified
  `cli::terminal::tests::independently_opened_terminal_devices_must_match` as
  the only failure. The corrected fixture then passed both the focused test and
  the complete verification script on Ubuntu and macOS.

Corrected checkpoint `7dd7362af49e6dcea4b03f51fe3825c379a4fd43` is on
`origin/main`. Its GitHub Actions run
[`31844031837`](https://github.com/xizheyin/deepseek-harness-cli-rs/actions/runs/31844031837)
completed successfully:

- the only job, `Rust verification` (`94906709961`), ran on
  `ubuntu-24.04` from `2026-08-14T21:48:46Z` through `21:51:46Z`;
- checkout and installation of the pinned Rust 1.85.0 toolchain succeeded;
- `Run repository checks` executed `./scripts/verify.sh` from `21:48:57Z`
  through `21:51:44Z` and succeeded; and
- every setup and cleanup step also succeeded.

An independent clean Ubuntu 24.04.4 reproduction of the same checkpoint ran
the same complete verification script and listed 500/500 default/all-feature
tests with zero ignored. The platform difference from macOS's 493 tests is the
expected Linux-only process/procfs coverage plus one Linux filename case, not a
skipped test. The final read-only review confirmed that the fix changes only a
test fixture; production terminal identity validation still requires a
character device and matching `st_dev`, `st_ino`, and `st_rdev`.

This closes the Phase 7 checkpoint. Durable storage, resume, damaged-tail
recovery, and context compaction now become the single active Phase 8 scope.
