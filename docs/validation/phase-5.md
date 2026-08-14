# Phase 5 validation

## Scope

Phase 5 validates one public Rust file-change path. `WorkspaceToolRegistry`
keeps the Phase 4 `list`, `glob`, `grep`, and `read` tools and adds a strict,
single-file `apply_patch`. The Agent records the tool call before work starts,
prepares a complete canonical diff without changing the file, applies the
configured `Allow`, `Deny`, or `Ask` policy, and records a correlated result
after the commit outcome is known.

An `Ask` request writes paired, log-only `approval/asked` and
`approval/decided` events. Only `allowed-once` may commit. A rejection,
unavailable approval channel, malformed patch, conflict, cancellation before
publication, or other pre-commit failure leaves the target unchanged. Once a
link or rename publishes the new file, the result must say `committed: true`
even if cancellation, a parent-directory sync error, or staging cleanup happens
afterward.

This phase does not claim that the `dsh` executable can edit a project. The
registry, Agent Loop, Session events, and approval-provider seam are reachable
public Rust APIs, but the executable still exposes only help, version, and
argument errors. Terminal prompting is Phase 7. Shell commands and process
cleanup are Phase 6. Persistent repair of an interrupted session tail is Phase
8.

## Local validation

- Date: 2026-08-14 (Asia/Shanghai)
- Git base before the Phase 5 checkpoint: `12e9d46bded6e8099e7dc534c06a0c49a803fe78`
- Tested Phase 5 checkpoint: `57834fd1a24957beb04f70771b01460fbc06fb9a`
- Total Rust tests in the final pre-checkpoint run: 263
- Host: macOS 27.0 (build 26A5406e), arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Default Rust verification public-network use: no; existing HTTP tests use loopback only
- Credentials or API keys used: no; tests use no credential or conspicuous fake values
- User project data modified/read by tests: no; file-change tests use fresh temporary trees

The tested checkpoint contains the Phase 5 implementation plus the Linux
capability-directory synchronization fix found by the first Ubuntu run. The
commands and test count below were rerun against the exact final code before it
was committed.

The repository-wide commands completed successfully:

```console
./scripts/verify.sh
git diff --check
```

These commands covered formatting, the locked all-target/all-feature build, all
default tests, Clippy with warnings denied, deterministic whitespace checks,
and complete-tree whitespace checks on Rust 1.85.0. All 263 Rust tests passed.
Phase 5 added 56 tests:

- 8 strict patch parser, canonicalization, and resource tests;
- 10 capability-relative publication, cancellation, sync, and cleanup tests;
- 18 public real-registry, filesystem, and upstream-oracle tests;
- 13 Agent approval, timeout, cancellation, and commit-truth tests;
- 4 Session approval codec/invariant/replay tests;
- 3 Session claim growth, rebinding, and preferred-only settlement tests.

The other 207 tests are the previously accepted CLI, model, Provider, Agent,
Session, and read-only workspace suites.

The locked graph compiles on the repository MSRV. Phase 5 adds one direct
production dependency:

- `diffy 0.5.1` with its `std` feature: parsing bounded unified-diff syntax and
  representing hunk lines; MIT OR Apache-2.0; crate `rust-version` 1.85.0.

Rust does not use `diffy`'s fuzzy apply behavior. The project applies parsed
hunks itself at their exact declared positions, rejects trailing or multi-file
input, and generates its own complete three-context-line approval preview.

## Upstream behavior validation

The official checkout was clean at
`47f943859bef60e4160492346772ded9b24f765a`. The accepted focused, keyless run
used Vitest 4.1.8:

```console
cd /Users/xizheyin/workspace/deepseek-harness-upstream

pnpm exec vitest run \
  packages/core/tools/tests/tools.spec.ts \
  packages/fs/fs-local/tests/filesystem.spec.ts \
  packages/fs/fs-local/tests/fsio.spec.ts \
  packages/fs/fs-observation-policy/tests/policy.spec.ts \
  packages/fs/tool-fs/tests/diff.spec.ts \
  packages/fs/tool-fs/tests/integration.spec.ts \
  packages/interaction/user-approval/tests/approval.spec.ts
# 7 files: 375 passed, 1 platform-specific test skipped on macOS
```

The run used locked local dependencies, no credential, no public network
request, and only fresh temporary workspaces.

Node 26.0.0, TypeScript 6.0.3, and the checkout's locked `tsx 4.22.4`
type-checked and generated the Phase 5 oracle twice:

```console
node /Users/xizheyin/workspace/ds-harness-rs/scripts/typecheck-upstream-file-change-fixtures.mjs \
  /Users/xizheyin/workspace/deepseek-harness-upstream

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-file-change-fixtures.ts \
  /tmp/upstream-phase5-a.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-file-change-fixtures.ts \
  /tmp/upstream-phase5-b.json

cmp -s /tmp/upstream-phase5-a.json /tmp/upstream-phase5-b.json
cmp -s /tmp/upstream-phase5-a.json \
  /Users/xizheyin/workspace/ds-harness-rs/tests/fixtures/tools/upstream_phase5_oracle.json
```

The checker passed; both generated files were byte-identical and matched the
committed fixture. SHA-256:

- type checker: `47fab188d4e452ebdad0d6e2e19cad569b192302aa70cabe758b3b5c0193ef3a`;
- generator: `f8bac3d965e95d6bb626659bf221d75c7f980effd9c798171b0257b650f884e0`;
- fixture: `604e047677431366200c7b9550e62815847ccb03baa4e63e9b07aed8aa9f451f`.

The Rust comparison tests consume the committed fixture directly. They compare
four canonical create/update/edit outcomes by final bytes, operation, and
normalized applied hunk facts. They also compare four policy paths by first-step
event order, approval outcome, call/result provenance, approval-provider and
model-dispatch counts, and final side effect. Upstream `defaultAllow` maps to an
explicit Rust `Allow`; the different Rust default is tested separately.

The comparator does not hide real interface differences. Official Harness has
`write` and literal `edit`, while Rust has strict `apply_patch`; official create
metadata has no applied hunk, while Rust approval shows the complete create
diff; result envelopes and path presentation also differ. Those facts are
recorded as `intentional-difference` in `docs/compatibility.md` rather than
normalized away.

## Behavior, failure, and safety categories

| Category | Evidence |
| --- | --- |
| Normal | The real registry runs through `AgentLoop`; create and update publish exact bytes, LF and CRLF updates are covered, ordinary modes are preserved, create uses mode `0600`, special bits are stripped, the approval preview equals durable result metadata, and the next model step receives the correlated result. |
| Failure | Wrong hunk positions, stale baselines, missing targets or parents, invalid UTF-8, NUL, mixed line endings, non-regular files, oversized input, and ordinary filesystem failures become bounded correlated tool failures. A failure after publication is distinguished from a failure before publication. |
| Rejection | Closed arguments, strict one-file headers, traversal and lexical aliases, multiple files, unsupported patch forms, symlink components, final symlinks, hard links, disabled policy, rejected approval, and unavailable approval all fail closed. Non-grants never invoke the commit capability. |
| Cancellation | Cancellation while waiting for approval records `approval/decided {cancelled}`, discards a late allow, and never commits. Once blocking commit starts, the Agent cancels cooperatively but waits for a definite disposition; a real committed outcome wins over cancellation and remains durable. |
| Timeout | Preparation uses the existing bounded tool timer. A commit that crosses the timer is still awaited until it reports committed or not committed. Only a definite pre-publication outcome becomes `TOOL_TIMEOUT`; a late committed outcome cannot be rewritten as a timeout. A stuck kernel call is not hard-killable. |
| Replay | Approval events are log-only, do not enter model messages, and round-trip with future fields preserved. The tool result cites exactly one prior call and is replayed into the next provider request without rerunning the file operation. An unmatched `approval/asked` tail is retained but blocks Agent reconstruction until Phase 8 can append repair evidence. |
| Atomic publication | A private sibling staging area receives the complete candidate. Create publishes with a no-replace hard link and update with same-directory rename. Two plans from one baseline allow only one publication, concurrent readers see only the complete old or new bytes, and late target/parent/mode/link changes preserve the competing version. Parent sync or cleanup trouble after publication is reported as committed uncertainty or warning, not as a false rollback. |
| Resource safety | Fixed limits cover the 256 KiB patch, 1,024 hunks, 100,000 patch lines, 64 KiB patch line, 4,096-byte path and 64-component depth, 16 MiB file, 100,000 file lines, 1 MiB file line, 64 KiB canonical diff JSON, 4 KiB approval reason, 128 KiB mutation-result event, and existing Session/turn budgets. Exact-limit and one-over tests cover the main parser, path, file, metadata, and reservation boundaries. |
| Privacy | Built-in mutation, approval, and execution types do not reveal the raw patch, file body, approval reason, or preview through `Debug`. Errors use workspace-relative paths and bounded stable text, and discard provider panic/error text and raw OS errors. The asked event does not duplicate the diff; the authorized canonical diff remains deliberately durable in the correlated result metadata. |

## Independent review

Three independent review tracks covered different failure classes:

1. Agent/Session truthfulness reviewed approval pairing, dynamic event sequence
   claims, result capacity, cancellation, timeout, replay, and the commit point.
   It drove exact claim rebinding, preferred-only settlement after a side
   effect, rejection of ambiguous commit-result states, and tests proving that
   cancellation or timeout cannot erase `committed: true`.
2. Patch/filesystem safety reviewed parser behavior, path handles, time-of-check
   versus time-of-use windows, publication, synchronization, and cleanup. It
   drove replacement of fuzzy patch application with exact-position matching,
   `NOFOLLOW` directory traversal, parent and target identity rechecks,
   symlink/hard-link rejection, private non-recursive cleanup, and truthful
   durability/cleanup warnings after publication.
3. Oracle/test-matrix review checked the pinned upstream facts, schema and
   preview differences, event correlation, side effects, boundary coverage,
   and compatibility wording. It drove the two real Rust-to-fixture comparators,
   the explicit default-Ask case, and exact/+1 tests for path depth, UTF-8 path
   bytes, canonical metadata, patch/file limits, conflicts, modes, and
   old-or-new visibility.

The final independent read-only reviews found no safety, audit, compatibility,
or test-matrix blocker. One independent rerun exposed a scheduling race in a
paused-time timeout test; the fixture was changed to poll the Agent through the
timeout branch before releasing the blocked worker, and both affected cases then
passed 50 consecutive repetitions each. The tested filesystem fault seam is
private under `cfg(test)` and does not enter the release API or release build.

## Known limitations

- `WorkspaceToolRegistry` and the approval pipeline are public Rust APIs only;
  the `dsh` executable is not wired to them yet and cannot edit a project.
- Mutation support is compiled and tested only on Unix. Windows file mutation
  behavior is not implemented or claimed.
- Rust narrows ordinary conflict windows but cannot provide an absolute
  cross-process compare-and-swap between the last check and `rename`. An
  uncooperative external writer can still win or be overwritten in that final
  portable-filesystem window.
- Cancellation is cooperative between bounded operations. A broken FUSE,
  network filesystem, or other stuck kernel call can keep the blocking commit
  alive past the Agent timer; no hard-kill helper process exists before Phase 6.
- There is no terminal approval prompt. The safe default is `Ask`, and the
  default no-UI provider returns `Unavailable`, so an unassembled Agent fails
  closed instead of writing.
- Delete, rename, copy, binary patches, mode-only patches, multi-file
  transactions, directory creation, and Shell operations are outside Phase 5.
- Update publication creates a new inode. Ordinary permission bits are
  preserved, but owner, group, ACLs, extended attributes, flags, resource
  forks, and hard-link topology are not preserved.
- A process crash or Session clock failure after filesystem publication can
  leave an unresolved durable tail. Phase 8 must append repair evidence; replay
  never guesses or reruns the patch.
- Only the committed canonical fixture cases support the narrow `compatible`
  claim. Tool schema, approval preview, default policy, observation timing,
  path/link policy, result envelope, and resource limits are documented
  intentional differences.
- No real DeepSeek API call was run. Evidence uses the pinned executable oracle,
  official keyless tests, fake providers, temporary workspaces, and the already
  validated loopback Provider path.

## Remote acceptance

The Phase 5 feature checkpoint and its Linux follow-up were pushed non-forced to
`origin/main`. The first Ubuntu run
[31775483542](https://github.com/xizheyin/deepseek-harness-cli-rs/actions/runs/31775483542)
correctly exposed that `cap-std` opens the ambient workspace root with Linux
`O_PATH`: publication succeeded, but that handle could not be directory-synced,
so two exact commit-truth tests failed. The fix reopens `.` relative to the
retained capability as a read-only directory before `fsync`; it does not reopen
an ambient display path or weaken either test.

GitHub Actions run
[31776599425](https://github.com/xizheyin/deepseek-harness-cli-rs/actions/runs/31776599425)
then completed successfully on Ubuntu 24.04 for the exact accepted checkpoint
`57834fd1a24957beb04f70771b01460fbc06fb9a`. Its Rust verification job ran the
same repository gate and passed all 263 tests.
