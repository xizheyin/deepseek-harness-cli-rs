# Phase 3 validation

## Scope

Phase 3 validates a public Rust `AgentLoop` that joins the Phase 1 append-only
session with the Phase 2 provider boundary and a minimal tool-execution seam.
It covers bounded turns, steps and attempts; request reconstruction; stream
assembly; same-step retry; tool correlation; truthful stop reasons;
cooperative cancellation and timeout; exact Session capacity reservation; and
fail-closed recovery when an executor infrastructure failure leaves a
model-visible call without a result.

This does not claim that the `dsh` executable can run an Agent. The loop is
reachable as a tested public Rust API, but the current CLI still exposes only
help, version, and argument errors. Real file/search tools, approval, Shell,
persistence, and interactive assembly belong to later phases.

## Local validation

- Date: 2026-08-14 (Asia/Shanghai)
- Git base before the Phase 3 checkpoint: `989744d45fc8ff3c6adb9cb6aca1865d3538557b`
- Tested Phase 3 checkpoint: `66c91700c4c61f0f0032a4cf6f46e26005693348`
- Host: macOS 27.0, arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Default Rust verification public-network use: no; HTTP tests use loopback only
- Credentials or API keys used: no; tests use no credential or conspicuous fake values
- User project data, processes, or Git repositories modified by tests: no

The repository-wide command completed successfully:

```console
./scripts/verify.sh
```

It ran formatting, locked all-target compilation, all tests, Clippy with
warnings denied, deterministic whitespace self-tests, and complete-tree
whitespace checks on Rust 1.85.0. All 166 Rust tests passed:

- 44 Phase 3 Agent integration, cancellation, error, safety, limit, and oracle tests;
- 1 Agent retry-policy formatting/sorting unit test;
- 9 Session reservation and durable-retry relationship tests added for Phase 3;
- 34 private DeepSeek protocol/transport/security tests;
- 17 public Provider/model/default/retry/stream contract tests;
- 4 real `reqwest` loopback/isolated-child entries;
- 6 CLI foundation tests;
- 9 model-value tests;
- 42 pre-existing Session/replay/projection/resource tests.

`git diff --check` also passed. The locked graph compiled on the repository
MSRV. Phase 3 adds only two direct production dependencies: `uuid 1.18.1`
(opaque v4 IDs; Rust 1.63;
MIT/Apache-2.0) and `ryu-js 1.0.3` (JavaScript-compatible retry-key number
formatting; Rust 1.71; Apache-2.0/BSL-1.0).

## Upstream behavior validation

The official checkout was clean at
`47f943859bef60e4160492346772ded9b24f765a`. Its complete keyless Agent Loop
suite and the related retry/tool/system-prompt suites passed:

```console
cd /Users/xizheyin/workspace/deepseek-harness-upstream

pnpm exec vitest run packages/core/agent-loop/tests
# 18 files, 329 tests passed

pnpm exec vitest run \
  packages/llm/llm-retry/tests \
  packages/core/tools/tests \
  packages/core/system-prompt/tests
# 21 files, 533 tests passed
```

These tests used installed locked dependencies and fakes; they did not contact
the public network or read a real key. The key-controlled request-cache E2E
test was inspected but not run because Phase 3 acceptance does not require
spending user quota.

Node 26.0.0, TypeScript 6.0.3, and the checkout's locked `tsx` 4.22.4 then
type-checked and generated the Phase 3 runtime oracle twice:

```console
node /Users/xizheyin/workspace/ds-harness-rs/scripts/typecheck-upstream-agent-fixtures.mjs

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-agent-fixtures.ts \
  /tmp/upstream-phase3-a.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-agent-fixtures.ts \
  /tmp/upstream-phase3-b.json

cmp -s /tmp/upstream-phase3-a.json /tmp/upstream-phase3-b.json
cmp -s /tmp/upstream-phase3-a.json \
  /Users/xizheyin/workspace/ds-harness-rs/tests/fixtures/agent/upstream_phase3_oracle.json
```

The type check passed; both outputs were byte-identical and matched the
committed fixture. SHA-256:

- type checker: `3c21bb11b3ef37d3ec8182a4585d9efe4e7adc0c2984e8fefcf634a09a4976f1`;
- generator: `d8368691f9b14dd2f214db512ba60d4b192dbfc7a1b915c52e78bd80c6226444`;
- fixture: `9b0249dacd104df417faff37657aa0a71cde0675f66808db793bd52c562b124c`.

The Rust comparison derives its provider chunks directly from that fixture. It
compares five canonical scenarios: text completion, one tool round trip, one
normal same-step retry, max-token tool suppression, and pre-step rejection.
It removes only the explicitly deferred `agent/inbox/spliced` events, then
checks dispatch-time replay, full request fields, core event order, every raw
chunk, surface provenance, retry payloads, tool intention-before-body, final
model history, counters, and closed state.

## Failure and safety categories

| Category | Evidence |
| --- | --- |
| Normal | Text, max-token, one/multiple tool, multi-step, model-facing tool failure, concluding result, stable/resume/change header, and five official oracle scenarios close with expected messages and counters. |
| Provider failure/retry | Non-retryable error preserves its real failure; normal/always policies, Retry-After, retry/attempt caps, missing/oversized effective output limits, prepare/stream/poll errors and panics have stable closure. |
| Tool rejection/failure | Unknown names, valid oversized or malformed arguments, ordinary error results, timeout, duplicate IDs, result limits, executor error, and factory/poll panic are distinguished before unsafe side effects. |
| Cancellation | Pre-entry, provider wait, retry wait, EOF race, one/multiple tools, cooperative cleanup, ignored cancellation, and final boundary races record aborted truth and stop later side effects. |
| Timeout | Paused-time provider turn deadline, tool deadline, retry delay, and one-second tool shutdown grace are deterministic and bounded. |
| Recovery | A balanced ordinary turn can continue; reconstructed headers use `resume`. A dangling model-visible tool call is deliberately rejected by both the same Agent and reconstruction until Phase 8 append-only repair. |
| Atomicity | Invalid turn input fails before `turn/start`; step input is batch-claimed; failed reservation/settlement preserves claims and state; tool call/result/closer floors cannot be consumed by chunks. |
| Resource safety | Every public Agent limit has min/max/one-over tests. Fixed request, turn input, schemas, arguments, results, aggregate preferred results, tokens, attempts, steps, calls, duration, event floor, and payload floor are bounded. |
| Privacy | Provider/executor infrastructure error text and panic payloads do not enter session JSON; public Agent config/result Debug output summarizes sizes/counts rather than prompt/schema/result bodies. Normal model/tool results are deliberately durable, so native extensions remain a trusted boundary and may still write directly to stderr. |

## Independent review

Three independent read-only reviews examined the candidate from different
angles:

- the Agent behavior review checked request/header timing, retry decisions,
  tool order/correlation, limits, official fixture comparison, and README/
  compatibility claim scope;
- the Session/replay review checked exact reservation arithmetic, retry event
  relationships, replay equivalence, surface provenance, and the oracle's
  dispatch-time evidence;
- the safety/lifecycle review checked cancellation races, bounded cleanup,
  extension panics, output amplification, unresolved-call reconstruction,
  secret propagation, and every public hard limit.

The reviews found and drove fixes for terminal-slot reservation, preferred
result fallback, retry validation and JavaScript policy keys, retry-limit/RNG
ordering, cancellation after queued output, tool cleanup races, late tool
success, multi-call infrastructure failure, unknown/duplicate tools, input and
result amplification, extension panic closure, dangling-history replay guards,
surface-provenance spoofing, dispatch-time oracle evidence, and stale broad
compatibility claims. Final reviewers reported no remaining implementation,
safety, Session, scoped-oracle, or documentation blocker on the tested
checkpoint.

## Known limitations

- `AgentLoop` is a public library API but is not assembled into `dsh`; the CLI cannot start a conversation yet.
- Phase 3 has only a serial execution seam. Real read/search tools and the registry/policy/approval pipeline begin in Phases 4–5.
- Rust uses explicit `TurnProposal`, fixed system text, and ordered schemas rather than the upstream inbox, live steering, Cordis prompt waterfalls, or dynamic runtime context.
- All retry modes and turns have Rust safety limits; long valid upstream runs can stop earlier with a stable `AGENT_*` error.
- An executor infrastructure failure preserves the upstream dangling call evidence but Rust refuses continuation until Phase 8 appends a repair result.
- Callers must cancel and continue awaiting `run_turn`; dropping a polled future is a crash-tail case for Phase 8 recovery.
- A caught native-extension panic is kept out of durable facts, but Rust's global panic hook may print its payload to stderr; hostile in-process code is not sandboxed.
- Request-header initial/stable/resume/change production branches are Rust-tested, but the broader two-runtime resume/change row remains `partial` pending a generated comparison.
- No real DeepSeek API call was run. Evidence is the pinned executable oracle, official keyless tests, fakes, and the already-validated real loopback provider path.

## Remote acceptance

The Phase 3 checkpoint was pushed non-forced to `origin/main`. GitHub Actions
run [31759801021](https://github.com/xizheyin/deepseek-harness-cli-rs/actions/runs/31759801021)
completed successfully on Ubuntu 24.04 for the exact checkpoint
`66c91700c4c61f0f0032a4cf6f46e26005693348`.

## 2026-08-18 request-header lifecycle addendum

Phase 9 closed the remaining v0.1 comparison gap without rewriting the
historical Phase 3 checkpoint above. The pinned upstream generator now adds a
sixth scenario that runs three turns with effective `maxTokens` values
`1024 → 1024 → 2048`, then starts a fresh loop over a complete prior event seed.
It records the full canonical `request/header` payloads and reasons, rather than
only their event types.

The Rust producer test uses the same system/config facts and the same three-turn
field change. It compares the complete `initial`/`change` payload pair, plus the
complete seeded `initial`/`resume` pair. The generator type-checked against the
same clean pinned checkout, generated twice byte-identically, and the focused
Rust comparison passed. Addendum SHA-256 values:

- type checker: `3c21bb11b3ef37d3ec8182a4585d9efe4e7adc0c2984e8fefcf634a09a4976f1`;
- generator: `7f1292f3dcbf0a23b80e277222e8be21c7aba57d31376bf69bb285c9d1e00746`;
- fixture: `5377ba8401c346a5266dd425f0b6f2100d983179c289b05f112eeded2b7817e5`.
