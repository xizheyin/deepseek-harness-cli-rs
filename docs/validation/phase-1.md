# Phase 1 validation

## Scope

Phase 1 validates the provider-neutral Rust value vocabulary and bounded in-memory session core. It includes append-only events, replay, turn/step/tool relations, model-visible surface replacement, request projections, explicit outcome facts, and deterministic JSON interchange.

It does not claim that `dsh` can call DeepSeek, run tools, persist a session, or provide an interactive terminal. Those remain later phases.

## Local validation

- Date: 2026-08-14 (Asia/Shanghai)
- Git base before the Phase 1 checkpoint: `369160dd90b9f4ac891fe9dfe3d76ba4c1148dc6`
- Tested Phase 1 checkpoint: `cdaf4ba3ba91bf465e0187dd11a8965b8a78eb0c`
- Host: macOS 27.0, arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Default Rust verification network use: no
- Credentials or API keys used: no
- User project data modified by tests: no

The repository-wide command completed successfully:

```console
./scripts/verify.sh
```

It ran the pinned-toolchain formatting, all-target/all-feature check, tests, Clippy with warnings denied, deterministic whitespace self-test, and complete-tree whitespace check. Fifty-seven Rust tests passed:

- 6 CLI foundation smoke tests;
- 9 model-value and JSON-boundary tests;
- 15 session codec/oracle tests;
- 17 state, relation, surface, and atomicity tests;
- 5 resource-limit tests;
- 5 request projection tests.

`git diff --check` also passed.

## Upstream oracle validation

The checked-out official baseline was a clean tracked tree at `47f943859bef60e4160492346772ded9b24f765a`. Node 26.0.0, TypeScript 6.0.3, and that checkout's locked `tsx` 4.22.4 first type-checked the repository-owned oracle against the pinned source graph and then ran it twice:

```console
cd /Users/xizheyin/workspace/deepseek-harness-upstream

node /Users/xizheyin/workspace/ds-harness-rs/scripts/typecheck-upstream-session-fixtures.mjs

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-session-fixtures.ts \
  > /tmp/dsh-phase1-oracle-1.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-session-fixtures.ts \
  > /tmp/dsh-phase1-oracle-2.json

cmp -s /tmp/dsh-phase1-oracle-1.json /tmp/dsh-phase1-oracle-2.json
```

The type check passed. The two outputs were byte-identical and matched the committed fixture. SHA-256:

- type checker: `bc5ea8221fac3863e64d68feb2e38aa36024943da46eaff218d75bf05ddae5a3`;
- generator: `a966e0bbd11e1be2e41302e87dd233381e312e59908cab838496ecbad8eb0e2a`;
- fixture: `3fddc4bfdce1b2cc414d6f1a2bf55eb7edc51ce8774a5d2fe3b91d3a5ee37f78`.

The oracle uses fixed time and IDs and covers the complete Phase 1 model-value vocabulary, canonical tool flow, event/message extension preservation, future/null values, request header/context folds, surface replacement, illegal atomic appends, optional-invariant behavior, unknown-event policy, and documented JSON admission differences.

Four directly relevant official test files were also run:

```console
pnpm exec vitest run \
  packages/core/session/tests/session.spec.ts \
  packages/core/session/tests/request-header.spec.ts \
  packages/core/session/tests/surface.spec.ts \
  packages/core/session/tests/invariant.spec.ts
```

Result: 4 files and 163 tests passed. These upstream-only commands used already-installed dependencies; they made no network request and used no credentials.

## Failure and safety categories

| Category | Evidence |
| --- | --- |
| Normal | Canonical user → assistant/tool → result trace has contiguous events and the same surface/messages as official runtime output. |
| Failure | Provider failure, max-token, interrupted, malformed JSON/header/message, missing tool call, bad sequence, and illegal boundaries return structured errors. |
| Rejection | Pre-step blocked flow closes a zero-step turn; oversized, unsafe-number, unknown-required, malformed-current-vocabulary, and surface-invalid inputs fail explicitly. |
| Cancellation | User/hook cancellation facts round-trip as `turn/end` with `aborted`; orchestration and cleanup begin in Phase 3/6. |
| Timeout | N/A: Phase 1 performs no network or subprocess work. |
| Recovery | Explicit seed markers are idempotent; valid open tails replay without being rewritten. Durable crash repair is Phase 8. |
| Atomicity | Clock, transition, surface, size-budget, and codec failures leave events, next sequence, state, and messages unchanged; a small append succeeds after a budget rejection. |
| Resource safety | Values and sessions have tested size/depth/node/event/provenance limits. Unknown events cannot retain unchecked recursive JSON. |
| Privacy | Tests are offline/keyless and contain only synthetic messages and fake provider identifiers. |

## Independent review

Independent read-only reviews covered upstream semantics and licensing, public API/type design, compatibility evidence, documentation truthfulness, and hostile-input/resource behavior. Findings corrected before this record included:

- extra header/event/message fields and explicit `null` values were initially rejected or lost;
- future plugin message, finish, turn-end, and request-reason values needed raw-backed preservation;
- safe-integer notation, negative zero, failure facts, nullable tool results, request context, and JavaScript numeric equality had edge mismatches;
- surface validation ordering and tool-result replacement identity needed exact official semantics;
- the first hand-written fixture did not prove official behavior, so it was replaced by a deterministic executable TS oracle;
- the executable oracle initially lacked a static type-check gate and complete model/attachment source provenance;
- public values needed read-only accessors for later Provider use;
- live sessions lacked a whole-session retained-payload budget;
- a publicly constructible unknown event could retain deeply nested raw JSON and stack-overflow while being rejected; it now requires bounded `JsonValue`;
- public extensible enums could construct the removed request-header reason `fallback` or a turn-end typed tag that disagreed with retained JSON; both are now rejected atomically just like imported logs;
- two always-successful validation passes were removed after their types made the checked property unrepresentable;
- compatibility and design documents previously described planned or obsolete behavior.

The final resource review found no remaining Phase 1 blocker. A 16 MiB snapshot is parsed as a whole tree and projection updates clone bounded state, so temporary memory/CPU can exceed the compact wire size; all growth is nevertheless capped. Streaming decode and projection optimization are appropriate Phase 8 work, not claims made here.

The final complete-tree independent review reported no blocker after re-running `./scripts/verify.sh` (57 tests), `git diff --check`, the oracle TypeScript checker, and two byte-identical oracle generations against the clean pinned upstream checkout. It also confirmed that the checker/generator/fixture hashes match this record and `docs/upstream.md`, and that the compatibility table, README, and roadmap make no stronger product claim than the staged implementation. Remote CI evidence is added after the non-force push.

## Known limitations

- The implemented core is a Rust library and is not reachable from the `dsh` CLI yet.
- Stream-chunk sequence grammar, adapter routing, HTTP/SSE, and secret handling are Phase 2.
- Agent Loop closure/cancellation orchestration is Phase 3; Phase 1 only represents and validates the facts.
- The snapshot is not durable JSONL persistence, resume, fork, or crash repair.
- Rust's tested resource, safe-integer, object-order, always-on-invariant, unknown-event, and malformed-known-payload policies are documented intentional differences in `docs/compatibility.md`.
- Cordis/npm plugin ABI and lifecycle are outside v0.1 scope.

## Remote acceptance

Checkpoint `cdaf4ba3ba91bf465e0187dd11a8965b8a78eb0c` was pushed non-force to `origin/main`. GitHub Actions [CI run #3](https://github.com/xizheyin/deepseek-harness-cli-rs/actions/runs/31741014694) completed successfully on Ubuntu 24.04; its checkout, pinned Rust installation, and repository verification steps all passed.
