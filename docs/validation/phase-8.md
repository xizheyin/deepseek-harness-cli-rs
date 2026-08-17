# Phase 8 validation

## Status

The user-approved Phase 8 product scope is implemented and has passed its local
acceptance gates. The roadmap remains `in-progress` until this worktree is
committed and pushed as one auditable checkpoint.

This is deliberately not a claim of database-grade durability or complete
upstream persistence/compaction compatibility. Phase 8 now delivers useful
local continuity after a normal exit and one bounded automatic context summary
on the real Agent/CLI path.

## Accepted product scope

- A normally closed current-version CLI session can be listed, resumed, and
  continued from its prior model-visible context.
- Corrupt or unsupported history fails before new Provider/tool work, and an
  unknown old tool outcome is never automatically replayed.
- Durable sessions continue beyond the former 4,096-event in-memory lifetime
  ceiling.
- At 80% committed-context pressure, or after a hard request-size preflight,
  Agent first prunes oversized old tool results and then attempts at most one
  summary in the whole turn.
- The summary request keeps a balanced 16% recent tail, has purpose
  `compaction`, caps output at 8,192 tokens, cannot execute tools, and does not
  see the pending user input.
- A successful summary installs one adjacent
  `compaction/start → compaction/summary → user/message replacement →
  compaction/end` transaction, then rebuilds the same ordinary request.
- Empty, tool-calling, failed, cancelled, max-token, or non-shrinking summaries
  do not install a checkpoint. A pressure-only Provider failure is recorded
  and the still-encodable request continues; a hard-limit failure becomes the
  stable context-limit turn error.

Explicitly deferred hardening includes provider-overflow replay, multiple
summary transactions in one turn, proof for every power-loss/crash prefix,
cross-filesystem `fsync` guarantees, exhaustive old/future schema migration,
cold-scan resident-credit parity, and the complete 32/64/96/192 MiB physical
allocation proof.

## Local validation

- Date: 2026-08-18 (Asia/Shanghai)
- Git base before the current worktree:
  `2495e6b3b710ee4eede61b4c9769c90dfe8ee3e7`
- Host: macOS, arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Public-network use: none; HTTP tests bind only loopback addresses
- Credentials: no real key; CLI tests use a conspicuous loopback-only fake key
- User project data: none; file, process, session, and CLI tests use temporary
  workspaces

The final repository gate is:

```console
./scripts/verify.sh
git diff --check
```

It covers formatting, locked all-target/all-feature compilation, all default
tests, Clippy with warnings denied, and whitespace checks. The accepted tree has
745 default tests: 472 library tests and 273 integration/CLI tests, with zero
ignored.

Focused compaction evidence includes:

```console
cargo +1.85.0 test --locked --lib summary -- --nocapture
cargo +1.85.0 test --locked --test cli_smoke \
  a_real_resumed_script_compacts_once_and_continues_the_same_prompt \
  -- --nocapture
cargo +1.85.0 test --locked --test interactive_cli \
  durable_session_continues_after_crossing_the_old_retained_json_ceiling \
  -- --nocapture
```

The Agent suite fixes pressure and hard-limit success, same-input continuation,
memory and durable non-shrinking output, summary tool-call rejection, Provider
failure, invalid-output usage accounting, cancellation after the bracket starts,
a stalled summary reaching the turn deadline, marker-only prune failure, and one
summary per turn. The real CLI acceptance
starts a new `dsh`, resumes it twice through real JSONL recovery and the
DeepSeek adapter, triggers compaction on the third prompt, and verifies both the
summary request and the final continued request plus the four durable events.

## Upstream evidence and compatibility

The semantic baseline remains DeepSeek Harness commit
`47f943859bef60e4160492346772ded9b24f765a`, inspected on 2026-08-13. Exact
source/test paths, the 12-file/519-test focused upstream run, the 4-file/65-test
token/pairing rerun, fixture generation commands, and fixture hashes are in
`docs/upstream.md`.

The broad compaction row remains `partial`; the narrow one-pass v0.1 row is a
verified `intentional-difference`. It implements one practical transaction
rather than upstream's broader repeated candidate/overflow behavior. README and
the compatibility table describe only the reachable path and its limits.

## Publication gate

The source, tests, and documentation are ready for the normal non-force
checkpoint. Formal Phase 8 completion still requires that commit and its
best-effort push under the repository rules; the landed commit is recorded in
the follow-up status transition.
