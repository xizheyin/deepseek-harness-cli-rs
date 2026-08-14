# Phase 6 validation

## Scope

Phase 6 validates one public Rust foreground-Shell path on macOS. The new
`LocalToolRegistry` keeps `list`, `glob`, `grep`, `read`, and `apply_patch`, then
adds a closed `bash` schema. All six tools share the same retained Workspace
capability. The Agent records the tool call before process work, resolves the
configured `Allow`, `Deny`, or `Ask` policy, and only an allowed sealed Action
may start `/bin/bash`.

Each allowed call starts a fresh Bash process with null standard input, a held
workspace-relative starting directory, a fixed bounded environment, separate
stdout and stderr pipes, and its own POSIX session/process group. Command
timeout, caller cancellation, Agent timeout, output overflow, and unsupported
background work converge on owned group cleanup. A normal started result is not
published until the direct child is reaped and the runner has finished its
same-group observation and pipe obligations. If that ownership proof is lost,
the call stays unresolved instead of inventing a successful cleanup result.

This is not a command sandbox. After startup, an approved command has the same
filesystem and network permissions as the current user. Phase 6 also does not
claim that the `dsh` executable can run Shell commands: `LocalToolRegistry`,
`AgentLoop`, Session events, and the approval-provider seam are public Rust APIs,
but the executable still exposes only help, version, and argument errors.
Terminal approval and wiring Ctrl+C to the current turn belong to Phase 7.

## Local validation

- Date: 2026-08-14 (Asia/Shanghai)
- Git base before Phase 6: `7cd5fef4969c5aac3d7920ed5f372bbe5f1ca5e7`
- Tested tree: stabilized pre-checkpoint Phase 6 working tree based on that
  commit; the final immutable commit is pending
- Total Rust tests in the repository-wide run: 351
- Host: macOS 27.0 (build 26A5406e), arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Default Rust verification public-network use: no; existing HTTP tests use
  loopback only
- Credentials or API keys used: no; privacy tests use conspicuous fake values
- User project data modified/read by tests: no; Shell and workspace tests use
  fresh temporary directories

The repository-wide gate completed successfully:

```console
./scripts/verify.sh
git diff --check
```

`verify.sh` ran formatting, the locked all-target/all-feature build, all default
tests, Clippy with warnings denied, deterministic whitespace tests, and
complete-tree whitespace checks. All 351 Rust tests passed. Phase 6 added 88
tests:

- 42 process runner, capture, spawn, host-observer, parser, and real-process
  tests;
- 12 Shell schema, argument, environment, rendering, and result unit tests;
- 12 private Agent Action state-machine tests;
- 4 public Agent Action integration tests;
- 15 public local-registry, approval, Shell, environment, and upstream-oracle
  integration tests;
- 3 Workspace Shell working-directory capability tests.

The other 263 tests are the accepted Phase 0–5 CLI, model, Provider, Agent,
Session, read-only workspace, file-change, and approval suites.

The first repository-wide rerun exposed a pre-existing test-only race in the
Phase 2 loopback proxy fixture: its nonblocking listener could yield an accepted
socket whose first request read returned `WouldBlock`. The server now restores
that accepted socket to bounded blocking reads before parsing the request. This
does not change Provider production code; the focused proxy test passed three
consecutive reruns and the final repository-wide gate passed afterward.

The locked graph still compiles on the repository MSRV. Phase 6 adds one direct
production dependency and enables two narrowly required existing features:

- `libc 0.2.189`: the POSIX signal/process ABI used by the host-contract check
  and macOS process observer; MIT OR Apache-2.0;
- `rustix 1.1.4` adds its `process` feature for typed process IDs, waiting,
  signals, `setsid`, and `fchdir`;
- `tokio 1.53` adds its `net` feature so Unix pipe file descriptors can use
  Tokio's readiness driver without blocking an async worker.

Unsafe code remains denied by default. It is allowed only in the small
`process/{spawn,host,macos}.rs` modules around `pre_exec`, `sigaction`, and
macOS `libproc` calls. Each unsafe block states the required memory or
async-signal-safety rule; the surrounding parsers and state machine remain safe
Rust.

## Upstream behavior validation

The official checkout was clean at
`47f943859bef60e4160492346772ded9b24f765a`. Research used its locked local
dependencies and no credential or public network request. The accepted runs
recorded in `docs/upstream.md` covered:

- 13 Shell/subprocess test files: 270/270 passed;
- an expanded 23-file process, sandbox, approval, and Agent-cancellation matrix:
  490/490 passed;
- four generic tool-contract files: 166/166 passed;
- two macOS Seatbelt E2E research files: 10/10 passed.

The Seatbelt results describe upstream only. This Rust implementation does not
include or claim Seatbelt, Landlock, or another operating-system sandbox.

Node 26.0.0, TypeScript 6.0.3, and the upstream lockfile's `tsx 4.22.4`
type-checked and generated the Phase 6 oracle twice. From the pinned upstream
checkout, the exact offline provenance commands were:

```console
node /Users/xizheyin/workspace/ds-harness-rs/scripts/typecheck-upstream-shell-fixtures.mjs \
  /Users/xizheyin/workspace/deepseek-harness-upstream

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-shell-fixtures.ts \
  /tmp/upstream-phase6-a.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-shell-fixtures.ts \
  /tmp/upstream-phase6-b.json

cmp -s /tmp/upstream-phase6-a.json /tmp/upstream-phase6-b.json
cmp -s /tmp/upstream-phase6-a.json \
  /Users/xizheyin/workspace/ds-harness-rs/tests/fixtures/tools/upstream_phase6_oracle.json
```

The checker passed; both generated files were byte-identical and matched the
committed fixture. SHA-256:

- type checker: `e67df63e45f5acd639e50dc346b681127ed15adec9d4503b38d45a01241b3d1e`;
- generator: `17756b2ccaf1f36a71367abbe7d80d628c73cd2971a96263bb300b5077bb8221`;
- fixture: `15a7a05dcf36f5c1fcbc97946baf21b69d91ea573d1d51fcc8bf848735caacde`.

The Rust comparison consumes that committed fixture directly. Its narrow
`compatible` claim covers five small foreground cases at an explicit
25,000-millisecond timeout: success, silence, mixed stdout/stderr, nonzero exit,
and a real self-signal. It compares exact rendered text plus exit code, signal,
timeout, and timeout-value facts. The timeout case additionally fixes the
ordinary-result marker and process facts without claiming a stable platform
termination signal.

The same fixture keeps upstream schema, workdir, environment, executable,
caller-abort, and service-cleanup observations. Rust tests assert those facts
before checking the documented differences. They do not normalize away the
smaller foreground-only schema, default approval, stricter workdir, fixed Bash,
environment allowlist, bounded output, or stronger per-call cleanup.

## Behavior, failure, and safety categories

| Category | Evidence |
| --- | --- |
| Normal | An explicitly allowed real `bash` call runs through `AgentLoop`, preserves call/result provenance, keeps stdout and stderr separate, renders stdout before a marked stderr section, reports silence and nonzero status truthfully, and replays the correlated result to the next provider request. Every invocation uses a fresh process; stdin and profile/startup hooks are closed. |
| Failure | Closed typed arguments reject missing, null, extra, control-character, oversized, and out-of-range fields before approval or spawn. Missing/file/outside/changed working directories, process-observer/runtime preflight failure, spawn failure, pipe setup/read failure, signal-delivery warnings, output overflow, and pipe-drain timeout have bounded explicit outcomes. A started ownership failure is never rewritten as an ordinary tool failure or success. |
| Rejection | `Deny`, rejected/cancelled/unavailable approval, direct `execute`, legacy forwarding, and a dispatch-binding mismatch cannot reach the process capability. Sentinel tests prove that denial starts no process and creates no file. `Ask` records paired approval events; only `allowed-once` starts the Action. |
| Cancellation | Cancellation before preparation or spawn returns truthful `started: false` facts and no side effect. Real post-spawn cancellation sends signals to the group and waits for settlement. Layered Agent tests prove that the first observed outer stop remains cancellation even if the turn deadline arrives while cleanup is still running, and that a correlated started/aborted result is retained only after quiescent settlement. |
| Timeout | The default command timeout is 25 seconds and the accepted range is 1–295,000 milliseconds. A real timeout is an ordinary process result with `timedOut: true`, independently of its eventual signal or exit. TERM-ignoring work reaches SIGKILL after the fixed three-second grace. Agent tool and turn deadlines remain independent; the first outer stop is not overwritten while cleanup finishes. |
| Process ownership | Spawn is the ownership linearization point: after it succeeds, a retained leader status and group guard prevent future-drop cleanup gaps. The normal path keeps the leader waitable with `WNOWAIT`, requires two no-live group observations, disarms signalling, performs exact reap, and drains or explicitly closes both pipes. Real tests stop a same-group background member, handle both full pipes, and leave no detached Tokio task. Observer/reaper panic, status mismatch, or stolen ownership yields unresolved `StartedOwnershipLost`. |
| Replay and recovery | Settled results cite exactly one prior call and replay without rerunning the command. An ownership-lost or infrastructure tail deliberately has no fabricated result and poisons same-instance and reconstructed Agent reuse. Append-only repair of that rare tail belongs to Phase 8; Phase 6 does not guess from a saved PID or signal a process after a crash. |
| Resource safety | Limits include a 32 KiB command, 1 KiB description, 4 KiB workdir, 24 environment entries/32 KiB environment, five-second Action preparation, 295-second command timeout, 8 KiB pipe reads, 8 MiB combined observed output, 64,000-byte tail per stream, one-second escaped-pipe drain, three-second TERM grace, 64 KiB compact model content, and 128 KiB Action result event. Exact-limit/one-over tests cover the important argument, environment, output, tail, and event boundaries. The first byte over 8 MiB causes immediate SIGKILL even if cancellation or timeout won first. |
| Privacy | The child receives only 19 copied names plus five fixed terminal overrides; startup hooks, credential-shaped names, proxy variables, ambient `DSH_*`, and non-Unicode/oversized snapshots are omitted or fail closed. Conspicuous fake secret, proxy, hook, and ordinary ambient values are absent from Session JSON and registry `Debug`. Errors are bounded and redacted. The command, description, and bounded command output are intentionally model/session-visible, so callers must not place secrets in those fields. |

## Independent review

Five independent tracks reviewed different ownership boundaries:

1. The Agent Action review checked private claim profiles, per-call dispatch
   binding, setup versus started outcomes, preferred-only result settlement,
   event-size claims, and first-observed turn-stop precedence. Its 12 private and
   4 public tests passed, and a cached Action from one call cannot run as
   another call.
2. The Shell frontend review checked the public schema, direct-execute guard,
   policy and approval side effects, working-directory capability, shared
   registry authority, fixed environment, compact renderer, and oracle pairings.
   It found no blocker after the 12 private Shell tests and 15 public Shell
   integration tests passed.
3. The process-lifecycle audit first reviewed a stabilized tree fingerprint
   beginning `5046ed…`, then reviewed the completed P0 additions. It independently
   reran build/Clippy checks, all 42 final process tests, and all 15 public Shell
   tests. It found no production blocker and specifically confirmed the spawn
   ownership point, immediate 8-MiB-plus-one-byte kill, `WNOWAIT` → two
   observations → disarm → reap order, absence of detached join handles,
   and unresolved ownership-loss behavior.
4. The P0 audit checked the real `LocalToolRegistry` → Agent → Bash →
   Session cancellation and timeout paths, the 12 private Action tests, the 42
   process tests, and the Linux-only PID 1, child-subreaper, and `CLONE_PARENT`
   source paths. It found and caused fixes for two test-ordering races: both
   TERM-dependent tests now wait for an explicit ready signal before triggering
   cancellation or timeout. The repaired tests passed 10 and 5 consecutive
   reruns respectively; no production defect remained.
5. The final whole-tree gate reran `verify.sh` with 351/351 tests and zero
   ignored tests, then ran the complete 145-test library suite ten consecutive
   times. It also reran the focused process, Action, and Shell suites, Rustdoc
   with warnings denied, the release build and CLI help/version checks, release
   LLVM-IR test-hook scans, formatting, diff checks, and upstream fixture/hash
   verification. All passed and the reviewer reported no local production
   blocker. An earlier concurrent acceptance run reported one library failure
   without retaining its test name; the failure did not recur in the ten
   complete library reruns or the final whole-tree gate. This unexplained but
   unreproduced observation is recorded here rather than silently omitted; the
   repeated frozen-tree evidence did not support classifying it as a production
   defect.

This closes the local/macOS implementation gate. Phase 6 must nevertheless
remain `in-progress` until the remote Ubuntu acceptance below passes, because
the Linux-only observer and exact-reap tests have not yet run in the official
repository workflow.

## Known limitations

- `LocalToolRegistry` and the Shell Action path are public Rust APIs only. The
  `dsh` executable cannot run a command, ask for approval, or turn Ctrl+C into a
  current-turn cancellation yet.
- Local real-process acceptance has run only on macOS 27.0 arm64. The Linux
  `/proc` implementation exists, but Ubuntu CI has not yet run; Windows and
  other operating systems are not implemented or claimed.
- An approved command is unsandboxed native code. The retained workdir prevents
  a startup path escape, not a later `cd`, absolute-path access, network access,
  or other action permitted to the current user.
- Background jobs, PTYs, interactive stdin, persistent Shell state, job handles,
  and completion notifications are not supported. A same-group background
  process is stopped before settlement rather than becoming a managed job.
- A descendant that deliberately calls `setsid` or otherwise leaves the owned
  group is outside containment. If it keeps a pipe open, Rust closes its side
  after the bounded drain window; that does not prove the escaped process died.
- Uninterruptible D-state work, a process that changes privilege, a broken
  kernel observer, or a host that steals child wait status can delay settlement
  or force `StartedOwnershipLost`. Ownership loss leaves the call unresolved
  and blocks reuse until Phase 8 can append repair evidence.
- Linux namespace PID 1, explicit child-subreaper hosts, invalid/hidden procfs,
  and a host that ignores SIGCHLD or uses `SA_NOCLDWAIT` are rejected because
  this runner does not own unrelated adopted children.
- The fixed environment intentionally omits proxy, SSH/agent socket, loader,
  credential, and arbitrary application variables. A command that depends on
  them behaves differently from upstream unless a future explicit capability is
  designed.
- Output has no spill file. Only bounded tails and markers survive; the process
  is killed after the first byte over 8 MiB combined output.
- No real DeepSeek API call was run. Evidence uses the pinned executable oracle,
  official keyless tests, fake providers, temporary workspaces, real local
  processes, and the already validated loopback Provider path.
- Only the five committed small foreground scenarios support the narrow
  `compatible` claim. Approval, schema, workdir, environment, executable,
  output, timeout ownership, cancellation envelope, and group settlement are
  documented and tested intentional differences.

## Remote acceptance

Pending. No Phase 6 checkpoint has been committed or pushed yet, and no Ubuntu
GitHub Actions run has exercised the Linux process observer or real-process
matrix. After a coherent non-force push, this section must record the exact
checkpoint commit, workflow URL, Ubuntu image, and the exact platform test
result. Until then, Phase 6 remains `in-progress` and Linux is not an accepted
platform claim.
