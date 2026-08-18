# Upstream baseline

DeepSeek Harness is the semantic reference for this project's agent core. The Rust implementation targets observable behavior, not a line-by-line translation of TypeScript or Cordis.

## Pinned revision

- Repository: <https://github.com/deepseek-ai/deepseek-harness>
- Commit: [`47f943859bef60e4160492346772ded9b24f765a`](https://github.com/deepseek-ai/deepseek-harness/tree/47f943859bef60e4160492346772ded9b24f765a)
- Commit date: 2026-08-13
- Baseline checked: 2026-08-13
- Upstream license at this revision: MIT

The baseline must not move as part of ordinary feature work. Updating it requires a dedicated compatibility review and regenerated behavioral fixtures.

## Phase 0 inspection

The following files were inspected at the pinned revision:

- [`LICENSE`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/LICENSE): upstream license.
- [`AGENTS.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/AGENTS.md): repository invariants, validation, and keyless-test rules.
- [`package.json`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/package.json): upstream build and test gates.
- [`docs/architecture.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/docs/architecture.md): plugin architecture, event domains, turn flow, and append-only session log.
- [`docs/testing.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/docs/testing.md): deterministic, keyless, snapshot, and live-API test tiers.
- [`apps/cli/README.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/apps/cli/README.md): official launcher purpose and modes.
- [`apps/cli/package.json`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/apps/cli/package.json): official package and binary naming.
- [`apps/cli/src/args.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/apps/cli/src/args.ts): official CLI grammar and non-zero error behavior.
- [`apps/cli/tests/args.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/apps/cli/tests/args.spec.ts): CLI behavior tests.
- [`.github/workflows/ci.yml`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/.github/workflows/ci.yml): upstream automated gates.
- [`THIRD_PARTY_NOTICES.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/THIRD_PARTY_NOTICES.md): notices for upstream's own dependency and vendored-source closure.

The current Phase 0 tree copies no upstream source code, test code, or fixture. It carries forward only the engineering intent of a named `dsh` executable, honest non-zero failures, pinned dependencies, and automated checks. Upstream's third-party notice set therefore does not describe this zero-dependency Rust tree and is not copied wholesale.

Later phases will add exact source/test paths as their behavior is studied. If implementation copies or adapts a substantial portion of upstream source, tests, or fixtures, that change must preserve the applicable DeepSeek MIT notice and audit any embedded third-party material.

## Phase 1 inspection

The following files define the provider-neutral vocabulary, in-memory event log, and projections studied for Phase 1:

- [`packages/llm/llm/src/brand.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/brand.ts): message, call, and provider-request identities.
- [`packages/llm/llm/src/message.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/message.ts): shared message shape, sources, and tool-result construction.
- [`packages/llm/llm/src/types.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/types.ts): content blocks, stream chunks, failures, finish reasons, usage, and tool schemas.
- [`packages/llm/llm/src/call-config.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/call-config.ts): provider-neutral request-header configuration.
- [`packages/llm/llm/src/invariant.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/invariant.ts): whole-stream grammar researched for the Phase 2 boundary.
- [`packages/attachment/attachment/src/types.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/attachment/attachment/src/types.ts): durable image-reference metadata used by image content blocks.
- [`packages/core/session/src/types.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/src/types.ts): header version, event vocabulary and envelope, turn outcomes, and surface operations.
- [`packages/core/session/src/index.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/src/index.ts): header/seed validation, atomic append, snapshots, seed marker, and message projection.
- [`packages/core/session/src/json.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/src/json.ts): lossless JSON domain and snapshot boundary.
- [`packages/core/session/src/surface.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/src/surface.ts): append/replace fold, provenance, tool-result rewrite, and model-message projection.
- [`packages/core/session/src/invariant.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/src/invariant.ts): turn/step numbering and tool-call correlation.
- [`packages/core/session/src/request-header.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/src/request-header.ts): canonical full request-header folding.
- [`packages/core/session/src/repair.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/src/repair.ts): recovery-only result codes and the meaning of an interrupted open tail.
- [`packages/core/session/src/known-event-types.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/src/known-event-types.ts) and [`packages/session/session-persistence/src/coordinator.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/session/session-persistence/src/coordinator.ts): required versus ignorable unknown-event import policy.

The deterministic behavior scenarios are based on these upstream tests:

- [`packages/core/session/tests/session.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/tests/session.spec.ts): derivation, outcome round trips, seed markers, malformed message/envelope rejection, contiguous sequences, and immutable snapshots.
- [`packages/core/session/tests/properties.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/tests/properties.spec.ts): deterministic derivation, replay equality, and non-message interleaving.
- [`packages/core/session/tests/surface.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/tests/surface.spec.ts): surface eligibility, provenance, replacement ranges, atomic rejection, and empty-assistant projection.
- [`packages/core/session/tests/invariant.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/tests/invariant.spec.ts): legal and illegal turn/step/tool traces, unresolved calls, seed replay, and open-tail markers.
- [`packages/core/session/tests/request-header.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/session/tests/request-header.spec.ts): latest full header projection, canonical optionals, and removed legacy formats.
- [`packages/core/agent-loop/src/agent.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/agent-loop/src/agent.ts): real step/turn closing order for errors and cancellation.
- [`packages/core/agent-loop/tests/interception.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/agent-loop/tests/interception.spec.ts): a pre-step rejection closes a zero-step turn as `blocked`.
- [`packages/core/agent-loop/tests/cancel.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/agent-loop/tests/cancel.spec.ts): cancellation closes a turn as `aborted` with its cause.

### Phase 1 runtime oracle

[`scripts/generate-upstream-session-fixtures.ts`](../scripts/generate-upstream-session-fixtures.ts) is a maintainer-only oracle written for this repository. It imports the public upstream packages and runs real `Session`, surface, request-projection, and invariant behavior. It refuses any checkout whose HEAD differs from the pinned commit or whose tracked tree is dirty, fixes the clock and identities, and emits [`tests/fixtures/session/upstream_phase1_oracle.json`](../tests/fixtures/session/upstream_phase1_oracle.json).

From the pinned upstream root, regenerate it with:

```console
node ../ds-harness-rs/scripts/typecheck-upstream-session-fixtures.mjs

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  ../ds-harness-rs/scripts/generate-upstream-session-fixtures.ts \
  > ../ds-harness-rs/tests/fixtures/session/upstream_phase1_oracle.json
```

The first command checks the oracle itself against the pinned upstream TypeScript source graph; it does not treat `tsx` execution as a substitute for static type checking. The validated Phase 1 checkpoint uses Node 26.0.0, TypeScript 6.0.3, and upstream's locked `tsx` 4.22.4. The checker, generator, and output SHA-256 values are:

- type checker: `bc5ea8221fac3863e64d68feb2e38aa36024943da46eaff218d75bf05ddae5a3`;
- generator: `a966e0bbd11e1be2e41302e87dd233381e312e59908cab838496ecbad8eb0e2a`;
- fixture: `3fddc4bfdce1b2cc414d6f1a2bf55eb7edc51ce8774a5d2fe3b91d3a5ee37f78`.

Two consecutive runs must compare byte-for-byte equal before accepting a changed fixture. Default Rust verification reads the committed JSON and needs neither Node, an upstream clone, network access, nor credentials.

The generator source is independently authored and does not copy upstream test implementation. Its output records observable JSON facts from the pinned MIT-licensed runtime, not upstream source text. If a future fixture begins embedding substantial upstream-authored text or code, attribution must be reassessed. Cordis lifecycle, durable persistence codecs, crash repair, and compaction producers remain separate later-phase research areas.

## Phase 2 inspection

The following files define the DeepSeek streaming boundary studied for Phase 2:

- [`packages/llm/llm-deepseek/src/serialize.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-deepseek/src/serialize.ts): provider-neutral message/tool conversion, thinking controls, and image rejection.
- [`packages/llm/llm-deepseek/src/sse.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-deepseek/src/sse.ts): SSE framing, comments, `[DONE]`, and truncated-stream behavior.
- [`packages/llm/llm-deepseek/src/translate.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-deepseek/src/translate.ts): reasoning/text/tool block ordering, usage, finish mapping, and empty completion.
- [`packages/llm/llm-deepseek/src/types.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-deepseek/src/types.ts): private chat-completions request and streamed response vocabulary.
- [`packages/llm/llm-deepseek/src/adapter.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-deepseek/src/adapter.ts): HTTP/authentication, cancellation, idle timeout, error classification, and consumer cleanup.
- [`packages/llm/llm-deepseek/src/index.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-deepseek/src/index.ts): defaults, configuration snapshots, credential resolution, model metadata, and retry-policy ownership.
- [`packages/llm/llm/src/api-key.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/api-key.ts): trimming and printable-ASCII API-key admission.
- [`packages/llm/llm/src/adapter-failure.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/adapter-failure.ts): conversion of adapter failures into serializable terminal facts.
- [`packages/llm/llm/src/error.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/error.ts): stable quota, context-window, credential, and empty-response codes.
- [`packages/llm/llm/src/invariant.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/invariant.ts): block, delta, usage, and terminal-finish grammar.
- [`packages/llm/llm/src/assembler.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/assembler.ts): canonical chunk assembly and the closed-index behavior used to audit stream grammar.
- [`packages/llm/llm/src/index.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/index.ts): registration-bound one-shot preparation, effective call config, context, adapter defaults, and retry-policy snapshot.
- [`packages/llm/llm/src/retry-policy.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/retry-policy.ts) and [`packages/llm/llm-retry/README.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-retry/README.md): retry facts belong to the provider, while retry execution is a later Agent step extension.
- [`packages/util/timeout/src/index.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/util/timeout/src/index.ts): per-read watchdog and timeout provenance.

The directly relevant behavior tests are:

- `packages/llm/llm-deepseek/tests/serialize.spec.ts`;
- `packages/llm/llm-deepseek/tests/sse.spec.ts`;
- `packages/llm/llm-deepseek/tests/translate.spec.ts`;
- `packages/llm/llm-deepseek/tests/adapter.spec.ts`;
- `packages/llm/llm-deepseek/tests/dynamic-config.spec.ts`;
- `packages/llm/llm/tests/invariant.spec.ts`;
- `packages/llm/llm/tests/assembler.spec.ts`;
- `packages/llm/llm/tests/service.spec.ts`;
- `packages/llm/llm-retry/tests/transport-recovery.spec.ts`.

Research runs covered 200 DeepSeek adapter tests and 252 stream/assembler/provider tests; all passed using local fakes or loopback servers, without a real API key or public network request. A focused model/default/retry review also ran 179 relevant tests successfully. The key-controlled `adapter.e2e.ts` was intentionally not run.

### Phase 2 runtime oracle

[`scripts/generate-upstream-provider-fixtures.ts`](../scripts/generate-upstream-provider-fixtures.ts) runs the pinned runtime's real model/default/retry resolution, `serializeRequest`, `parseSse`, `translate`, whole-stream invariant, and `BlockAssembler`. It records exact-model and unlisted-model defaults, retry-policy facts, full request JSON, fragmented SSE, interleaved reasoning/text/tool output, legal and illegal stream traces, and the official index-reuse contradiction. It fixes all inputs, rejects the wrong or dirty upstream checkout, and performs no network request or credential lookup.

From the pinned upstream root, verify and regenerate it with:

```console
node ../ds-harness-rs/scripts/typecheck-upstream-provider-fixtures.mjs

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  ../ds-harness-rs/scripts/generate-upstream-provider-fixtures.ts \
  > /tmp/upstream-phase2-a.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  ../ds-harness-rs/scripts/generate-upstream-provider-fixtures.ts \
  > /tmp/upstream-phase2-b.json

cmp -s /tmp/upstream-phase2-a.json /tmp/upstream-phase2-b.json
cmp -s /tmp/upstream-phase2-a.json \
  ../ds-harness-rs/tests/fixtures/provider/upstream_phase2_oracle.json
```

The accepted Phase 2 tree uses Node 26.0.0, TypeScript 6.0.3, and upstream's locked `tsx` 4.22.4. The checker, generator, and committed output SHA-256 values are:

- type checker: `71749297d2442f1cb23117dae53e6101d673a1593a577ccdbb33aed6634e25a1`;
- generator: `7b16cd4c26a49f7aa67c0ee16ed525b07172c0bf719091c619b4f0ebf72c64b6`;
- fixture: `cd1e4dca78ae4c242910e92fa832247d8135f322a892ae26faab2f5d85dcf0ed`.

The type checker checks this repository's oracle source against the pinned TypeScript source graph; `tsx` execution is not treated as static checking. Two generated files must compare byte-for-byte equal and match the committed fixture. Default Rust verification consumes only that fixture, so it needs neither Node nor an upstream clone and remains offline/keyless.

The oracle is independently authored for behavioral comparison and does not copy upstream test implementation. Its JSON output contains observed API facts and short stable error messages under the upstream MIT license; it contains no source code, user data, or credential.

## Phase 3 inspection

Phase 3 studies how provider attempts, model-visible history, tools, retries,
and cancellation are joined into balanced turns. The primary source files are:

- [`packages/core/agent-loop/src/agent.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/agent-loop/src/agent.ts): turn/step driver, request reconstruction, raw-chunk logging, successful message anchoring, and stop reasons.
- [`packages/core/agent-loop/src/tool-calls.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/agent-loop/src/tool-calls.ts): intention-before-execution, parallel/exclusive groups, model-order commits, cancellation draining, and skipped-call results.
- [`packages/core/agent-loop/src/runtime-context.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/agent-loop/src/runtime-context.ts): dynamic context snapshots, which are researched but deferred from the Phase 3 Rust boundary.
- [`packages/core/agent-loop/src/invariant.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/agent-loop/src/invariant.ts): requests must be built from logged headers and derived history.
- [`packages/core/agent-loop/src/constants.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/agent-loop/src/constants.ts): upstream parallel-tool default.
- [`packages/core/system-prompt/src/index.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/system-prompt/src/index.ts): identity/persona/section and tool-schema assembly order.
- [`packages/core/tools/src/index.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/core/tools/src/index.ts): execution/result vocabulary, cancellation normalization, additional context, and tool failures.
- [`packages/llm/llm/src/assembler.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm/src/assembler.ts): successful chunk assembly, max-token tool suppression, usage, and replay state.
- [`packages/llm/llm-retry/src/index.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-retry/src/index.ts): provider-routed retry decisions, delay calculation, Retry-After, and cancellation.
- [`packages/llm/llm-retry/src/types.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-retry/src/types.ts), [`invariant.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-retry/src/invariant.ts), and [`history.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/packages/llm/llm-retry/src/history.ts): durable retry schema, correlation, numbering, and route lookup.

The directly relevant deterministic tests are:

- `packages/core/agent-loop/tests/{loop,request-reconstruction,request-error,tool-calls,cancel,interception,contract-regressions,resume,tool-order,invariant}.spec.ts`;
- `packages/core/system-prompt/tests/{system-prompt,tool-order}.spec.ts`;
- `packages/llm/llm-retry/tests/{retry,transport-recovery,invariant}.spec.ts`;
- `packages/core/tools/tests/{tools,execution-mode,invariant}.spec.ts`;
- timeout and approval-policy tests used only to establish later-phase fail-closed boundaries.

The final research rerun exercised the complete keyless Agent Loop suite
(18 files, 329 tests) and the combined retry, tool, and system-prompt suites
(21 files, 533 tests). All passed without network access or a credential. The
key-controlled request-cache end-to-end test was read but intentionally not run
because it requires a real `DEEPSEEK_API_KEY`.

The inspected upstream loop has no total turn, step, attempt, token, tool-call,
or duration budget, and the `always` retry policy is unbounded until success or
cancellation. Rust limits are therefore a recorded safety difference, not an
upstream feature. Tool parallelism, live inbox steering, dynamic system prompt
context, approval, real tools, subprocess cleanup, persistence, and compaction
remain in their assigned later phases.

### Phase 3 runtime oracle

The independently authored Phase 3 oracle runs the real pinned Agent Loop with
a public fake adapter and fake tool under fixed time and UUIDs. At adapter entry
it captures the already-committed event prefix, folded request header, request
context, derived messages, and the complete normalized request. This proves the
important timing invariant—model-visible content is logged before it is sent—
rather than reconstructing evidence only after the turn finishes.

[`scripts/generate-upstream-agent-fixtures.ts`](../scripts/generate-upstream-agent-fixtures.ts)
records text completion, a tool round trip, a retry in the same step,
max-token tool suppression, pre-step rejection, and a separate request-header
lifecycle covering stable suppression, a changed full snapshot, and a fresh
loop over a seeded log. The fixture retains the official inbox events; the Rust
core-trace comparison removes exactly
`agent/inbox/spliced`, then compares the remaining core trace. Provider chunks
are read back from the fixture instead of being duplicated in a handwritten
Rust script.

From the pinned upstream root, type-check and regenerate it with:

```console
node ../ds-harness-rs/scripts/typecheck-upstream-agent-fixtures.mjs

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  ../ds-harness-rs/scripts/generate-upstream-agent-fixtures.ts \
  /tmp/upstream-phase3-a.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  ../ds-harness-rs/scripts/generate-upstream-agent-fixtures.ts \
  /tmp/upstream-phase3-b.json

cmp -s /tmp/upstream-phase3-a.json /tmp/upstream-phase3-b.json
cmp -s /tmp/upstream-phase3-a.json \
  ../ds-harness-rs/tests/fixtures/agent/upstream_phase3_oracle.json
```

The accepted Phase 3 research run used Node 26.0.0, TypeScript 6.0.3, and the
upstream lockfile's `tsx` 4.22.4. Its SHA-256 values are:

- type checker: `3c21bb11b3ef37d3ec8182a4585d9efe4e7adc0c2984e8fefcf634a09a4976f1`;
- generator: `7f1292f3dcbf0a23b80e277222e8be21c7aba57d31376bf69bb285c9d1e00746`;
- fixture: `5377ba8401c346a5266dd425f0b6f2100d983179c289b05f112eeded2b7817e5`.

The checker loads the oracle into the pinned TypeScript source graph; merely
executing it through `tsx` is not counted as type checking. Two generations
were byte-identical and matched the committed fixture. The default Rust suite
uses only that JSON file, so ordinary verification stays offline, keyless, and
independent of Node or the upstream clone.

## Phase 4 inspection

Phase 4 studies the real read-only filesystem tools and the common tool runtime.
The primary source files at the pinned revision are:

- `packages/core/tools/src/{index,schema}.ts`: lookup, argument snapshot and
  validation, policy/body hooks, result validation, cancellation normalization,
  and model-facing failures;
- `packages/core/agent-loop/src/{agent,tool-calls}.ts`: assistant/call/result
  ordering, intention-before-execution, cancellation draining, and ordered
  durable result commits;
- `packages/core/session/src/types.ts`: durable tool call/result vocabulary;
- `packages/fs/fs/src/{index,types}.ts` and
  `packages/fs/fs-local/src/{index,fsio}.ts`: target resolution, regular-file
  reads, one-level directory listing, errors, and cancellation;
- `packages/fs/fs-sandbox/src/{index,containment}.ts`: the important fact that
  the upstream sandbox passes reads through and confines only changes;
- `packages/fs/tool-fs/src/{read,read-target,read-render,session-cwd}.ts`: read
  schema, UTF-8/binary decisions, line/page limits, and exact rendering;
- `packages/fs/tool-fs-search/src/{glob,grep,search-core}.ts`: fixed ripgrep argv,
  parsing, sorting, truncation, errors, timeout declaration, and cancellation;
- `packages/fs/tool-fs/README.md` and `docs/tool-catalog.md`: shipped model-tool
  surface, including the explicit absence of a model-facing directory-list tool;
- `apps/cli/config/agent-presets/{standard,cordis,code}/agent.cordis.yml` and
  `packages/bundle/base/cordis.patch.yml`: shipped CLI glob sampling policy.

The directly relevant deterministic tests are:

- `packages/core/tools/tests/{tools,schema,json-schema,execution-mode}.spec.ts`;
- `packages/core/agent-loop/tests/{tool-calls,tool-order,cancel,contract-regressions}.spec.ts`;
- `packages/fs/fs/tests/{service,invariant}.spec.ts`;
- `packages/fs/fs-local/tests/{filesystem,fsio}.spec.ts`;
- `packages/fs/fs-sandbox/tests/fs-sandbox.spec.ts`;
- `packages/fs/tool-fs/tests/{tools,read-render,integration,error}.spec.ts`;
- `packages/fs/tool-fs-search/tests/{tools,integration,presentation,rg-path,load-path}.spec.ts`.

The research run exercised 30 default keyless filesystem/tool test files: 886
tests passed and one macOS-inapplicable Windows DACL test was skipped. A focused
Agent tool-order/cancellation run exercised another 56 tests, all passing. The
`fs-tools.e2e.ts` file requires `DEEPSEEK_API_KEY` and is not part of the default
`*.spec.ts` suite; it was read but intentionally not run. No research command
used a credential or public network request.

### Observed contracts and boundaries

`read` takes `file_path`, optional one-based `offset`, and optional `limit` up to
2,000. It accepts regular UTF-8 files, treats a NUL in the first 8,192 bytes as
binary, scans the full file for line count and late UTF-8 errors, caps selected
text near 50 KiB, and emits numbered XML-like text with one of three pagination
footers. The input root object is open to unknown fields, while the structured
output is closed.

`glob` takes `pattern` and optional `path`. The shipped CLI invokes packaged
ripgrep without a shell, with `--no-config --files --sort=modified --no-ignore
--hidden`, excludes `.git`, `.svn`, `.hg`, `.bzr`, `.jj`, and `.sl`, returns only
files, and inlines the oldest 100 matches with sampling disabled. Equal-mtime
ordering is not fixed.

`grep` takes `pattern`, optional file/directory `path`, and one positive `include`
glob. It invokes ripgrep JSON mode with `--no-config`, but unlike `glob` inherits
ripgrep's ordinary hidden/ignore behavior. It groups matches by first file
appearance, previews 2,000 bytes per line, represents invalid UTF-8 match bytes
with a placeholder, and inlines 250 matches. Cross-file order and binary-search
details are not a stable package-level promise.

There is no model-callable upstream `list`. The internal `FileSystem.listDir`
lists one level, returns file/directory/other facts, and sorts with JavaScript
`localeCompare`; it is evidence for a filesystem primitive, not compatibility
evidence for the Rust product extension.

Most importantly, the upstream read side is ambient. Absolute paths ignore cwd,
`..` can escape, local reads follow symlinks, and `fs-sandbox` deliberately passes
reads through. Rust's fixed workspace capability and rejection of outside
symlinks are therefore intentional security differences. They must not be
described as upstream-compatible behavior.

### Phase 4 runtime oracle

Phase 4 committed an independently authored, type-checked oracle:

- `scripts/generate-upstream-tool-fixtures.ts`;
- `scripts/typecheck-upstream-tool-fixtures.mjs`;
- `tests/fixtures/tools/upstream_phase4_oracle.json`.

From the clean pinned checkout, the accepted reproduction was:

```console
node /Users/xizheyin/workspace/ds-harness-rs/scripts/typecheck-upstream-tool-fixtures.mjs \
  /Users/xizheyin/workspace/deepseek-harness-upstream

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-tool-fixtures.ts \
  /tmp/upstream-phase4-a.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-tool-fixtures.ts \
  /tmp/upstream-phase4-b.json

cmp -s /tmp/upstream-phase4-a.json /tmp/upstream-phase4-b.json
cmp -s /tmp/upstream-phase4-a.json \
  /Users/xizheyin/workspace/ds-harness-rs/tests/fixtures/tools/upstream_phase4_oracle.json
```

The checker loaded the generator into the pinned TypeScript source graph. Both
generations were byte-identical and matched the committed fixture. SHA-256:

- type checker: `aadc60f480c1d6cff1625ab96c143dd500dde154808e30dcb74dda1a217e58ec`;
- generator: `cc18b5233da64026336c209d9ba63d304ab79d010c4f5bfe0819a897ef7763c9`;
- fixture: `32b47ea94ec65168084a31b0a1ee5a2b614865241a380bc97d1ada141296ee0a`.

The fixture uses a fresh temporary workspace, normalized paths, and fixed mtimes.
It records schema surfaces, relevant shipped configuration, the internal one-level
list primitive, small canonical `read`/`glob`/`grep` success/no-match cases, one
missing-file failure, and exact ambient parent/symlink-outside outcomes. It does
not pretend to cover every large-result, encoding, cancellation, or resource
boundary; those are Rust safety-policy tests. Rust compares the model-facing
canonical text and error code, while explicitly recording that upstream also has
structured `value`/presentation `meta` and displays an absolute read path.

The default Rust suite consumes only the committed JSON, so it needs neither Node
nor the upstream checkout. The generator does not present Rust's model-facing
`list` as an official tool and distinguishes the shipped CLI's
`sampleOverCapGlobResults: false` from the alternate sampled catalogue setting.

The final targeted upstream regression command was:

```console
pnpm exec vitest run \
  packages/core/tools/tests \
  packages/fs/fs-local/tests \
  packages/fs/tool-fs/tests \
  packages/fs/tool-fs-search/tests
```

Vitest 4.1.8 reported 26 files: 848 tests passed and one Windows-specific test
was skipped on macOS. It used locked local dependencies, no credential, and no
public network request.

## Phase 5 inspection

Phase 5 studies guarded text-file changes, applied diff facts, approval, and the
commit boundary. The primary source files at the pinned revision are:

- `packages/fs/tool-fs/src/{write,edit,diff,error,sandbox}.ts`: full-file write,
  literal edit, applied contextual diff, error remediation, and sandbox
  escalation;
- `packages/fs/tool-str-replace-editor/src/index.ts`: the alternate
  view/create/replace/insert surface used by the minimal upstream preset;
- `packages/fs/fs/src/{index,types}.ts` and
  `packages/fs/fs-local/src/{index,fsio,win32}.ts`: target/version vocabulary,
  per-target serialization, guarded create/update, staging, synchronization,
  permissions, and atomic publication;
- `packages/fs/fs-observation-policy/src/index.ts`: the process-local
  unseen/absent/present observation fold and write/edit intent decisions;
- `packages/core/tools/src/index.ts` and
  `packages/core/agent-loop/src/tool-calls.ts`: allow/deny/ask pre-decisions,
  intention-before-body, post-processing, cancellation, and durable result order;
- `packages/interaction/user-approval/src/{index,types,invariant}.ts`: ask/never
  session policy, one-shot outcomes, paired audit events, and cancellation;
- `packages/interaction/permission-presets/src/index.ts` and
  `packages/sandbox/sandbox/src/escalation.ts`: the shipped workspace/full-access
  bundles and the precise cases that ask for a wider capability.

The directly relevant deterministic tests are:

- `packages/fs/tool-fs/tests/{tools,integration,diff,error}.spec.ts`;
- `packages/fs/tool-str-replace-editor/tests/tools.spec.ts`;
- `packages/fs/fs-local/tests/{filesystem,fsio}.spec.ts` and
  `packages/fs/fs-observation-policy/tests/policy.spec.ts`;
- `packages/fs/fs-sandbox/tests`, `packages/util/atomic-write/tests`, and the
  core filesystem service tests;
- `packages/core/tools/tests/{tools,invariant}.spec.ts` and
  `packages/core/agent-loop/tests/{interception,tool-calls}.spec.ts`;
- `packages/interaction/user-approval/tests/{approval,invariant}.spec.ts`,
  `packages/interaction/permission-presets/tests`, and
  `packages/sandbox/sandbox/tests/escalation.spec.ts`.

There is no upstream `apply_patch` or unified-diff input tool. Official `write`
creates or replaces a whole file, while `edit` performs one literal replacement
(or every match when requested). Their result-time presentation computes
three-context-line hunks from the actual `before` and `after` content and stores
those diffs in tool-result metadata. Call-time cards are only argument previews:
an overwrite is shown like a new file and an edit shows just the requested
old/new strings. Rust Phase 5 therefore treats its strict single-file patch and
pre-approval exact applied diff as an intentional product/safety difference,
while comparing final content, conflict facts, result order, and approval pairs.

The upstream filesystem observation policy is process-local and session-owned.
An unseen/known-absent write uses `createIfAbsent`; a known-present write and edit
carry an opaque version derived from device, inode, size, mtime, and ctime.
Guarded create publishes with a no-replace hard link. Existing-file update checks
the version, stages and synchronizes a complete sibling file, then renames it.
That check and rename are not one portable cross-process CAS: a deterministic
upstream hook demonstrates that an external write in the last window can still be
overwritten. Rust can narrow and test ordinary conflict windows but must not claim
absolute linearizability against an uncooperative external process.

Approval has three distinct vocabularies. A pre-tool listener returns `allow`,
`deny`, or `ask`; the session ask policy is `ask` or `never`; and one question
settles `allowed-once`, `rejected`, `cancelled`, or `unavailable`. Only
`allowed-once` executes the body. Missing/throwing answerers and absent approval
composition fail closed. A cancellation while waiting writes a matching
`approval/decided {cancelled}` and discards a late answer. Normal workspace writes
do not ask merely because the session policy is `ask`; asks come from an explicit
pre-rule or wider sandbox escalation. Rust's default of asking for every
`apply_patch` is consequently an intentional difference.

The stable durable order is `tool/call`, optional `approval/asked` and
`approval/decided`, external side effect, then correlated `tool/result`.
Approval events are log-only and do not duplicate arguments. The applied
before/after value is execution-local; model-visible result content and optional
diff metadata are durable. A crash does not replay a mutation or restore a
one-shot grant. Observation versions also are not persisted.

The accepted Phase 5 research run exercised 27 keyless test files: 669 tests
passed and one platform-specific case was skipped on macOS. A separate focused
approval/tool-pipeline run exercised 271 tests, all passing. Neither run used a
credential, network request, or user project.

### Phase 5 runtime oracle

The independently authored Phase 5 oracle is tied to the same clean upstream
checkout at commit `47f943859bef60e4160492346772ded9b24f765a`:

- `scripts/typecheck-upstream-file-change-fixtures.mjs` checks the generator
  against the pinned TypeScript source graph;
- `scripts/generate-upstream-file-change-fixtures.ts` runs real upstream
  filesystem tools, observation policy, approval service, and Agent Loop code
  only in fresh temporary workspaces;
- `tests/fixtures/tools/upstream_phase5_oracle.json` retains the normalized,
  deterministic observations consumed by the offline Rust tests.

The accepted focused upstream run was:

```console
pnpm exec vitest run \
  packages/core/tools/tests/tools.spec.ts \
  packages/fs/fs-local/tests/filesystem.spec.ts \
  packages/fs/fs-local/tests/fsio.spec.ts \
  packages/fs/fs-observation-policy/tests/policy.spec.ts \
  packages/fs/tool-fs/tests/diff.spec.ts \
  packages/fs/tool-fs/tests/integration.spec.ts \
  packages/interaction/user-approval/tests/approval.spec.ts
```

Vitest 4.1.8 reported seven files and 375 passing tests; one platform-specific
test was skipped on macOS. The run used the checkout's locked dependencies and
no credential, public network request, or user project.

Node 26.0.0, TypeScript 6.0.3, and the checkout's locked `tsx` 4.22.4 then
type-checked the generator and reproduced the fixture twice:

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

The type check passed, both generated files were byte-identical, and both
matched the committed fixture. SHA-256:

- checker: `47fab188d4e452ebdad0d6e2e19cad569b192302aa70cabe758b3b5c0193ef3a`;
- generator: `f8bac3d965e95d6bb626659bf221d75c7f980effd9c798171b0257b650f884e0`;
- fixture: `604e047677431366200c7b9550e62815847ccb03baa4e63e9b07aed8aa9f451f`.

The fixture records the official tool surface, four canonical mutations,
unobserved and stale failures, one windowed observation, the final
check-to-rename race, and four approval paths. Its own named checks all pass.
The Rust comparison tests consume it directly, so normal Rust verification
remains offline and needs neither Node nor an upstream checkout.

## Phase 6 inspection

Phase 6 studies the foreground `bash` tool, its subprocess owner, and the
boundary between caller cancellation and observable process-group cleanup.
The primary source files at the pinned revision are:

- `packages/bundle/base/cordis.patch.yml`: the shipped 60-second bash timeout
  override and default sandbox composition;
- `packages/core/tools/src/index.ts`: caller-signal fusion, started-body
  draining, and final `ABORTED`/`ABORTED_BEFORE_DISPATCH` materialization at the
  public tool boundary;
- `packages/shell/tool-bash/src/{index,render,invariant}.ts`: the model-facing
  schema, argument checks, working-directory routing, foreground result shape,
  rendering, and abort mapping;
- `packages/shell/bash-local/src/{index,invariant}.ts`: command/default timeout
  resolution, `bash -c`, terminal-friendly environment overrides, output caps,
  and timeout-versus-caller-abort classification;
- `packages/shell/shell/src/{types,index,render,invariant}.ts`: the executor
  request/result seam and shared exit-marker vocabulary;
- `packages/subprocess/subprocess/src/{types,index,invariant}.ts`: explicit
  argv/cwd/env/stdio/grace ownership, bounded collection, and tree-scoped
  termination contracts;
- `packages/subprocess/subprocess-local/src/{spawn,process-inspector,index}.ts`:
  detached POSIX sessions/process groups, Windows tree termination, tail
  collection, private spill files, TERM-to-KILL escalation, inherited-pipe
  draining, and whole-tree liveness;
- `packages/shell/shell-env/src/{index,invariant}.ts`: the managed `DSH_*`
  environment namespace contributed by trusted plugins.

The official foreground input requires `command` and display-only
`description`; it optionally accepts `timeoutMs` and `workdir`. It starts a
fresh `bash -c` process for every call, so shell state does not persist. A
relative workdir is resolved against the session workspace, while an absolute
workdir is accepted. The executor library defaults to 120 seconds; the shipped
base composition overrides that value to 60 seconds. It caps one request at 600
seconds, keeps 64,000 bytes per output stream, permits at most 64 MiB in a
complete spill file, and gives a TERM-trapping tree three seconds before
SIGKILL.

Normal exit zero, nonzero exit, and command timeout all return ordinary tool
results. Nonzero status is rendered as `[exit code: N]`, stderr follows a
`[stderr]` heading, and a timeout marker remains visible even if a TERM trap
exits zero. Caller cancellation is different: the completed subprocess facts
have `aborted: true`, after which the model-facing tool converts the call to the
standard abort error. Spawn/infrastructure failures are tool errors rather than
invented process exit statuses.

The executable oracle now fixes that public caller-abort boundary with a real
TERM-trapping Bash process. It waits until the command has published its PID,
aborts the actual `ToolRuntime.execute` caller signal, and observes that the
promise resolves only after the direct leader is gone. The returned value is the
generic `AbortError`/`ABORTED` tool failure, contains no foreground shell value,
and does not expose the caller's abort reason. `ToolBash` intentionally discards
the completed `ShellRunResult` when `aborted` is true, so the public tool result
cannot stably expose its exact internal exit code or signal; the fixture records
that absence instead of reconstructing hidden process fields.

The subprocess layer's detached POSIX spawn makes the child a new session and
process-group leader, then signals the negative group ID. Termination is
idempotent: it sends SIGTERM, keeps the escalation
alive even if the direct shell exits, then sends SIGKILL after the grace period
when a descendant survives. The foreground `LocalBashExecutor.run` awaits the
handle's direct completion; the separate subprocess service `waitForExit` and
service disposal own the stronger whole-group wait. On Linux that observer
scans `/proc` after direct-child settlement so a group containing only zombies
does not look like active work. Collected pipes are drained concurrently but
only for a bounded grace after the direct child exits, preventing a descendant
that inherited stdout or stderr from holding result settlement forever. Rust's
per-call requirement to finish same-group cleanup before `tool/result` is
therefore a stronger intentional difference, not part of the narrow compatible
foreground-result claim.

A second executable oracle case makes this split directly observable. A real
foreground tool call starts a TERM-trapping, stdio-detached Bash helper in the
same process group and then lets the direct shell exit zero. The foreground
result is already a normal `direct-complete` success while the helper PID is
still alive. The oracle then awaits the owning Context's subprocess-service
disposal and verifies that the same PID is gone when disposal resolves. The PID
is used only for live probes and is never serialized; no duration, numeric PID,
or inferred cleanup status is placed in the fixture.

A local Node/macOS kernel probe recorded `[pid, pgid, sid]` as parent
`[48976, 48976, 48976]`, ordinary child `[48978, 48976, 48976]`, and detached
child `[48979, 48979, 48979]`. The numeric IDs are ephemeral, but the equality
relations confirm the detached child is both session and group leader; committed
Rust tests, not these particular PIDs, will provide the lasting platform gate.

Each stream keeps a byte-exact tail. Upstream can also spill the complete stream
to a random owner-only file inside a private temporary directory, but deletes or
stops advertising a spill once it can no longer prove completeness. The child
environment begins with the parent environment after removing credential-shaped
names (`KEY`, `PASSWORD`, `SECRET`, or `TOKEN`) and every ambient `DSH_*` name.
Trusted explicit entries may deliberately restore values. Bash adds `NO_COLOR=1`,
`TERM=dumb`, `PAGER=cat`, and `GIT_PAGER=cat`. The local executor invokes the
name `bash` through `PATH` with `-c`; ordinary inherited names such as `BASH_ENV`
and `SHELLOPTS` are not removed by the credential-name scrub. Rust's fixed
`/bin/bash --noprofile --norc`, cleared startup environment, and explicit
`argv[0] = "bash"` are therefore a paired executable/startup-policy difference.
Two real executor probes supported that source reading: replacing `PATH` with a
nonexistent directory produced `ENOENT: spawn bash ENOENT`, while an explicit
fake `BASH_ENV` hook printed `<from-bash-env>`. The probes used no credential or
network access.

The official plugin also exposes `run_in_background` by default and hands such
work to the jobs subsystem. Background jobs, completion wakes, and job tools
are explicitly outside this project's v0.1 scope. Rust Phase 6 must therefore
omit that field, reject injected attempts, and record the smaller foreground-only
surface as an intentional difference. Official ordinary shell calls also do not
ask merely because an approval service exists; approval appears for a policy or
sandbox escalation. Rust's ask-by-default unsandboxed shell policy is a
separate safety difference, not a claim of byte-identical policy behavior.

The directly relevant deterministic tests are:

- `packages/shell/tool-bash/tests/{tools,integration}.spec.ts`;
- `packages/shell/bash-local/tests/{executor,settings}.spec.ts`;
- `packages/shell/shell/tests/{service,render}.spec.ts` and
  `packages/shell/shell-env/tests/shell-env.spec.ts`;
- `packages/subprocess/subprocess/tests/service.spec.ts`;
- `packages/subprocess/subprocess-local/tests/{spawn,local,process-exit,process-inspector}.spec.ts`.

The accepted initial Phase 6 research run used the pinned checkout's Vitest
4.1.8 and locked dependencies:

```console
pnpm exec vitest run \
  packages/shell/tool-bash/tests \
  packages/shell/bash-local/tests \
  packages/shell/shell/tests \
  packages/shell/shell-env/tests \
  packages/subprocess/subprocess/tests \
  packages/subprocess/subprocess-local/tests
```

Vitest reported 13 files and 270 passing tests. The run used no credential,
public network request, or user project.

The independent Phase 6 review then ran this expanded process, sandbox,
approval, and Agent-cancellation matrix:

```console
pnpm exec vitest run \
  packages/shell/tool-bash/tests/tools.spec.ts \
  packages/shell/tool-bash/tests/integration.spec.ts \
  packages/shell/bash-local/tests/executor.spec.ts \
  packages/shell/bash-local/tests/settings.spec.ts \
  packages/shell/shell/tests/service.spec.ts \
  packages/shell/shell/tests/render.spec.ts \
  packages/shell/shell-env/tests/shell-env.spec.ts \
  packages/subprocess/subprocess/tests/service.spec.ts \
  packages/subprocess/subprocess-local/tests/spawn.spec.ts \
  packages/subprocess/subprocess-local/tests/local.spec.ts \
  packages/subprocess/subprocess-local/tests/process-exit.spec.ts \
  packages/subprocess/subprocess-local/tests/process-inspector.spec.ts \
  packages/subprocess/subprocess-local/tests/terminal.spec.ts \
  packages/sandbox/sandbox-local/tests/acl-grants.spec.ts \
  packages/sandbox/sandbox-local/tests/local.spec.ts \
  packages/sandbox/sandbox/tests/roots.spec.ts \
  packages/sandbox/sandbox/tests/vocabulary.spec.ts \
  packages/shell/bash-sandbox/tests/partial-landlock.spec.ts \
  packages/shell/bash-sandbox/tests/sandbox.spec.ts \
  packages/sandbox/sandbox-policy/tests/policy.spec.ts \
  packages/core/agent-loop/tests/cancel.spec.ts \
  packages/interaction/user-approval/tests/approval.spec.ts \
  packages/interaction/permission-presets/tests/projection.spec.ts
```

Vitest reported 23 files and 490/490 passing tests. The related generic tool
contract was rerun separately:

```console
pnpm exec vitest run \
  packages/core/tools/tests/tools.spec.ts \
  packages/core/tools/tests/schema.spec.ts \
  packages/core/tools/tests/invariant.spec.ts \
  packages/core/tools/tests/execution-mode.spec.ts
```

That run reported four files and 166/166 passing tests. The real macOS Seatbelt
matrix used its E2E configuration:

```console
pnpm exec vitest run \
  --config vitest.e2e.config.ts \
  packages/sandbox/sandbox-local/tests/seatbelt.e2e.ts \
  packages/shell/bash-sandbox/tests/seatbelt.e2e.ts
```

It reported two files and 10/10 passing tests. These commands were rerun on
macOS 27.0 arm64 with Node 26.0.0, pnpm 11.19.0, and Vitest 4.1.8. The pinned
checkout was clean before and after. These sandbox tests are upstream research
evidence only: Rust Phase 6 deliberately makes no Seatbelt, Landlock, or general
sandbox claim.

The committed Phase 6 oracle is generated by the real upstream `ToolBash`,
`LocalBashExecutor`, and local subprocess runtime. It records the foreground-only
and default-background schema surfaces, library and shipped defaults, small
success/silent/mixed/nonzero/real-self-signal/timeout results, pure rendering, three workdir
forms, environment scrubbing, bare-`bash` PATH failure, a real `BASH_ENV` hook,
`$0 == "bash"`, a real started caller abort at the public tool boundary, direct
foreground completion while a same-group helper remains alive, awaited
subprocess-service cleanup of that helper, and stable safety checks. Ordinary cases use a fixed PATH and
clear ambient `BASH_ENV`, so a developer's shell configuration cannot change the
fixture. The four short foreground comparisons and the self-signal case all
carry the same explicit `timeoutMs: 25000` that Rust will consume; they therefore
do not accidentally compare different implementation defaults. Random temporary
paths and the platform timeout signal are the only normalized values. Lifecycle
PIDs are asserted but omitted rather than normalized. The generator
was type-checked and run twice against the clean pinned checkout; both outputs
were byte-identical to `tests/fixtures/tools/upstream_phase6_oracle.json`.

SHA-256:

- checker: `e67df63e45f5acd639e50dc346b681127ed15adec9d4503b38d45a01241b3d1e`;
- generator: `17756b2ccaf1f36a71367abbe7d80d628c73cd2971a96263bb300b5077bb8221`;
- fixture: `15a7a05dcf36f5c1fcbc97946baf21b69d91ea573d1d51fcc8bf848735caacde`.

The exact offline provenance commands, run from the pinned upstream checkout,
were:

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

The type check and both comparisons passed. Normal Rust tests consume the
committed fixture and do not need Node or the upstream checkout.

## Phase 7: interactive and script drivers

The fixed upstream repository does not contain a built-in human terminal UI.
The inspected launcher paths are:

- `apps/cli/src/{args,bin,profile-boot,process-shutdown}.ts` and their tests:
  the published `dsh` is a profile launcher, first SIGINT starts whole-process
  cleanup, and the help surface deliberately omits/rejects the old `tui` name;
- `apps/cli/README.md` and `apps/cli/reference/README.md`: the built-in product
  surfaces are Web and one-shot headless, while a TUI requires the external
  `deepseek-harness/turtle-ui` repository;
- `packages/terminal/**`: a PTY capability used by the model, not the human
  conversation input UI.

The external TUI is not part of commit
`47f943859bef60e4160492346772ded9b24f765a`, so this repository does not claim
terminal visual or key-binding compatibility. Phase 7 instead uses these
committed upstream seams as its semantic baseline:

- `packages/bundle/headless/src/{startup,index}.ts`: one nonblank task, one new
  Session, wait-until-idle and flush-before-output, final committed nonempty
  assistant text on stdout, completed exit 0 and other durable outcomes exit 1;
- `packages/acp/acp/src/{index,codec}.ts`: multiple sequential prompts in one
  Session, user cancellation of one active Session, fail-closed one-shot
  approval, and owner cleanup on stdio EOF/disconnect;
- `packages/core/agent-loop/src/{agent,tool-calls}.ts`: partial chunk, tool, and
  cancellation event ownership;
- `packages/interaction/user-approval/src/index.ts`: paired asked/decided audit,
  cancellation-first outcomes, and no side effect before allowed-once;
- `packages/client/ui-conversation/src/client/conversation-nodes/{assistant,chat-snapshot-builder,turn-tail}.ts`,
  `packages/client/ui-conversation/src/client/input/{submission-policy,hub}.ts`,
  and `packages/client/ui-conversation/src/client/skeleton/ApprovalPanel.tsx`,
  together with
  `packages/client/ui-conversation/tests/{coverage-tails,submission-policy,input-scenarios,chat-view}.client.spec.*`:
  committed chunks form partial display state, the final assistant message is
  authoritative, cancellation retains partial output with stopped state,
  approval takes over the composer, and running input can queue or steer;
- `packages/interaction/commands/**`: Web-composition commands such as goal,
  compact, plan, permission, and feedback create command audit events rather
  than model turns; the fixed tree has no built-in terminal `/help` or `/exit`;
- `packages/fs/tool-fs/src/diff.ts`,
  `packages/client/ui-primitives/src/DiffBlock.tsx`, and
  `packages/client/ui-tool/src/client/tool/models/diff-card-model.ts`: newline,
  hunk, intended-versus-settled, and Web-card display facts.

Read-only research also inspected Web input submission policy, approval composer
takeover, and diff cards. Web supports Queue/Steer while running and displays
approval and intended diff in separate UI regions. Rust Phase 7 deliberately
uses one prompt at a time and places the complete Phase 5 canonical diff directly
inside the terminal approval question. Interactive Ctrl+C follows Web Stop's
turn-level user cancellation, not the launcher's process-level SIGINT contract.

On 2026-08-18 the local terminal UX was clarified without changing the upstream
semantic oracle. After the quiet-input fence, a human uses a bounded inline
Allow once / Reject / Cancel selector; arrows, `h/j/k/l`, Tab, and `y/n/c` move
the selection, while Enter alone confirms it and Escape cancels. Reject is the
safe default. Only this selector temporarily disables canonical input and echo,
keeps `ISIG`, and restores the exact terminal settings before the decision is
delivered. This is Rust product UX, not a new compatibility claim about the
pinned Web or ACP presentation.

The focused upstream test command was:

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

Vitest reported 10 files and 134/134 passing tests: ACP 39, approval 32,
headless 14, filesystem diff 12, and client diff projections 37. There were no
skipped, pending, or todo tests. A broader read-only research run also covered
launcher shutdown, Agent cancellation, Web input orchestration, and approval
composer behavior; those counts are research notes, while the reproducible
134-test command above is the fixture's exact focused provenance.

`--configLoader runner` keeps Vite's transient config loading inside the
process, so the read-only pinned checkout does not need a writable
`node_modules/.vite-temp`; it does not change the selected config or tests.

The committed Phase 7 oracle runs real upstream public/test compositions. It
records:

- two ACP turns in one Session, including the second request's prior context;
- a committed partial text chunk followed by user cancellation, balanced
  step/turn closure, and a later successful prompt in the same Session;
- allow-once, reject, and cancel approval flows through the real
  `ApprovalService` and tool pipeline, with a temporary sentinel changed only by
  allow-once;
- headless final-assistant-only stdout, flush-before-output, completed/other exit
  classification, and provider-error rendering;
- real `computeHunkDiffs` output plus source/test-derived Web rendering facts.

The standalone generator does not pretend to render the React/CSS/jsdom diff UI.
It instead locks the pinned source assertions and the two real official UI test
files, which passed 37/37 in the command above. It similarly uses the exported
headless runner with the official test-style Agent/Session factory rather than a
credentialed full model process.

The generator and checker are:

- `scripts/generate-upstream-interactive-fixtures.ts`;
- `scripts/typecheck-upstream-interactive-fixtures.mjs`;
- `tests/fixtures/cli/upstream_phase7_oracle.json`.

The exact offline provenance commands, run against the clean pinned checkout,
were:

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

The type check and both comparisons passed. SHA-256:

- checker: `e22d39db1f9b9b1a363c552636aa381463339cc2a95bd97efb77aab275579eee`;
- generator: `f99046dfbf0a04ff95affb23a9ef56efcb4ed2e87ab9c803317bac3c607cc794`;
- fixture: `bc293b7e6868cf54acb03edf2534891e8bb0bf0360c49f6a6b2c693a4f464b2f`.

The run used macOS 27.0 arm64, Node 26.0.0, pnpm 11.19.0,
TypeScript 6.0.3, tsx 4.22.4, Vitest 4.1.8, and Oxlint 1.76.0. The
upstream tree was clean before and after. It made no network or real model call,
read no credentials, and wrote only the requested output files and a fresh
platform temporary directory.

## Phase 8: persistence, recovery, resume, and compaction

Phase 8 research was performed against the same clean pinned checkout.

On 2026-08-18 the user explicitly narrowed the v0.1 product gate. The fixed
upstream remains the semantic reference, and the already implemented Rust
locks, barriers, repair rules, and limits remain in production. However, v0.1
now promises best-effort local continuity after a normal exit rather than
database-grade power-loss durability or proof for every crash prefix. The Rust
Agent now implements the remaining product slice: one bounded summary
transaction that reduces an old balanced prefix and continues the same ordinary
request.
Automatic post-provider context-overflow replay, multiple summary transactions,
exhaustive cold-recovery stress, complete physical-memory ownership proof, and
the full persistence-producer comparison are intentional post-v0.1 hardening,
not hidden compatibility claims.

The following core files establish that complete history and current model
context are different things:

- `packages/core/session/src/{index,types,surface,invariant,repair,known-event-types,chunk-rows}.ts`:
  append-only logical events, model-visible surface replacement, interrupted
  turn repair, required/ignorable event vocabulary, and lossless packing of
  adjacent physical chunk rows;
- `packages/core/session/tests/{session,surface,invariant,repair}.spec.ts`:
  continuous sequences, replacement without deletion, synthetic tool-result
  ordering, and seed-boundary behavior;
- `packages/core/agent-loop/src/{agent,tool-calls}.ts` and
  `packages/core/agent-loop/tests/resume.spec.ts`: resumed request-header reason,
  call-before-side-effect ordering, and continuation from a persisted prefix;
- `packages/llm/token-meter/src/{index,estimate,surface-fold,surface-projection,usage-projection,breakdown-projection}.ts`:
  pressure is derived from the current request header and current surface, and a
  replacement subtracts the price of shadowed nodes without deleting history.

The persistence implementation and contract evidence are:

- `packages/session/session-persistence/src/{coordinator,write-behind,index}.ts`:
  ordered append, durable cursors, flush ownership, suffix reads, format
  refusal, legacy normalization, torn-tail adoption, and cold repair;
- `packages/session/session-persistence/tests/{contract,coordinator-contract,persistence}.ts`:
  creation, append, recovery, repair, unknown/legacy events, read-from, resume,
  and failure behavior;
- `packages/session/session-persistence-jsonl/src/{format,index}.ts`:
  one tagged header, safe path components, plaintext/Zstandard artifacts,
  append/fsync/rollback, bounded prefix scanning, and frame repair;
- `packages/session/session-persistence-jsonl/tests/{jsonl,zstd}.spec.ts`:
  lazy creation, exact logical replay, chunk-row packing, incomplete final
  records/frames, corruption classification, rollback, and never-rewrite facts;
- `packages/session/session-checkpoint-policy/src/index.ts` and its tests:
  model-visible facts are flushed before dispatch, `tool/call` is flushed before
  side effects, and a completed response/result prefix is flushed before the
  next step.

The lightweight listing seam was also checked directly. The abstract metadata
API is in `packages/session/session-persistence/src/index.ts:223-240`; the JSONL
implementation in
`packages/session/session-persistence-jsonl/src/index.ts:446-509,705-770`
reads only the first complete line in 8 KiB chunks, skips empty/half-written or
non-header artifacts, and does not load the event body. Root absence versus
other I/O errors is fixed by `index.ts:850-873`, header-only parsing by
`format.ts:396-413`, and the user-facing stable order by
`packages/workspace/workspace/src/index.ts:82-83`. The focused fixed-commit run
was:

```console
cd /Users/xizheyin/workspace/deepseek-harness-upstream
pnpm exec vitest run --configLoader runner --config vitest.config.ts \
  packages/session/session-persistence-jsonl/tests/jsonl.spec.ts \
  -t 'list'
```

Vitest 4.1.8 reported one file passed, 12 tests passed, and 139 skipped in
562 ms. The checkout was at the pinned commit and clean before and after. This
supports header-only discovery, not an official `dsh --list-sessions` command:
the Rust CLI surface, flat canonical filenames, 64 KiB header ceiling,
128/256-entry bounds, no-follow permission checks, workspace-identity filter,
and escaped TSV output are documented Rust product/security differences.

At this revision the core `Session` appends with `seq = log.length` and has no
4,096-event or 16 MiB lifetime ceiling. A read-only local probe appended 5,000
`assistant/chunk` events and then another event successfully with continuous
sequences. This is a direct contrast with the current Rust Phase 1 in-memory
limit and is the basis of the Phase 8 long-reasoning regression; it is not a
claim that upstream memory or disk use is unlimited in every host composition.

The scanner behavior is deliberately conservative. A final record without LF
is ignored. A complete bad record or sequence gap preserves the earlier valid
prefix, but later evidence of a committed `turn/end` makes damage inside that
committed region a hard corruption error. Cold inspection is read-only; cold
load truncates only a recoverable suffix, synchronizes it, appends deterministic
repair events, and does not repeat repair on reload. The repair order for an
open step is model-call order: `TOOL_NOT_STARTED` for an assistant call without
a durable `tool/call`, `TOOL_OUTCOME_UNKNOWN` for a durable call without a
result, then `step/end` and interrupted `turn/end`. It never executes a tool.

The context-compaction implementation and tests inspected are:

- `packages/compaction/compaction/src/{types,invariant,index}.ts` and
  `packages/compaction/compaction/tests/{compaction,invariant}.spec.ts`:
  the durable start/summary/end bracket, ownership, source relations, and stale
  bracket behavior across `session/end-seed`;
- `packages/compaction/compaction-basic/src/{config,index,region,summarizer}.ts`:
  80% pressure, 16% retained tail, 8,192-token summary default, balanced range
  selection, summary framing, shrink validation, and overflow recovery;
- `packages/compaction/compaction-basic/tests/{compaction-basic,compaction-loop-repro,manual-compaction}.spec.ts`:
  automatic/manual transactions, errors, cancellation, changed surfaces,
  orphan prefixes, tool-pair boundaries, retries, and durability;
- `packages/compaction/compaction-tool-result-pruner/src/{config,index}.ts` and
  `packages/compaction/compaction-tool-result-pruner/tests/tool-result-pruner.spec.ts`:
  model-free tool-result replacement and its adjacent shadow-price event.

A successful basic compaction appends `compaction/start`, a log-only
`compaction/summary`, an immediately adjacent replacement `user/message`, and
`compaction/end`. The replacement changes only the derived surface. Old events,
including every raw assistant chunk, remain in the log, so successful
compaction increases the logical event count by four while reducing the next
model request. Start-only, start+summary, and start+summary+replacement crash
prefixes remain distinguishable and recoverable.

The default tool-result pruner runs only after ordinary pressure is reached, or
unconditionally before context-overflow range selection. It counts Unicode code
points only across text blocks. A result strictly over the default 8,192-point
threshold keeps a global 4,096-point head, the exact 39-point middle marker, and
a 1,024-point tail while preserving non-text blocks. It then appends adjacent
`compaction/prune` and replacement `tool/result` events. The replacement keeps
the original IDs/error/meta and cites the original singleton sequence; the full
original remains in history. The pair is sequential rather than transactional,
so a rejected second append can leave a known log-only prune marker with the
surface unchanged. These facts come from the cited source, focused tests, and a
read-only fixed-commit failure probe; the official suite does not itself contain
a crash snapshot for that marker-only prefix.
Its `pruneSession` candidate loop is synchronous and accepts no abort signal;
fixed upstream checks cancellation around the enclosing pressure/overflow
listener rather than between candidates. Rust's cancellation-aware row reads
and between-candidate stop are recorded as intentional difference 16 in the
Phase 8 design.

The current Rust checkpoint implements the provider-neutral event, recovery,
and model-free raw-row pruning path. It has validated codecs for the four
event tags, a strict adjacent start → summary → checkpoint replacement → end
projection, exact prune-marker pairing, and cold recovery for all three legal
interrupted bracket stages. The active projection now prices messages, system
text, and tool schemas with the pinned UTF-16/JavaScript-number heuristic,
maintains the exact current-surface total, chooses an oldest balanced prefix
without splitting tool pairs, enforces the 4,094-node provenance cap, and checks
exact summary/prune shadow prices plus genuine checkpoint shrinkage. Recovery reports an orphan bracket or
prune marker, closes only the enclosing step/turn facts that can be proved, and
never invents a successful `compaction/end` or a missing prune replacement. A
single-owner positional reader now authenticates a current durable tool-result
row in 64 KiB cancellation-aware chunks, and a sealed Session path preserves
its unknown raw fields while appending one adjacent prune/replacement prefix as
one owned writer command. The real Agent now calls this pass when exact
pre-step provider preflight reports a hard request limit, keeps the prospective
step/input claims protected across row reads, re-preflights the new surface,
and starts no model stream until that generation fits. A Session-owned attempt
fold now gives hot append and cold scan one exact chunk/source/usage truth,
closes only through an opaque token after a durable barrier, repairs unfinished
attempts as interrupted without fabricating an assistant, and installs the
pinned output-usage-or-heuristic token anchor from the exact successful source
span. The Session/cold-projection substrate also validates the one exact
`CONTEXT_WINDOW_EXCEEDED` boundary, atomically closes it with the first prune
marker, requires a validated prune/checkpoint replacement after that boundary
before replay can begin,
and rejects a second context compaction start in the same step. Focused Rust
tests also lock the frozen camel-case wire shape, reader-only
legacy forms, resource ceilings, exact estimator examples, positional cut
boundaries, restart from each partial repair, and payload-free UI projection.
The journal-byte substrate for the future 32 MiB resident-attempt budget now
charges every live row encoding before the clock/sequence commit. An ordinary
row moves the same buffer and credit through the Session batch, writer command,
cancellation-safe flight, and acknowledgement; a prune pair separately charges
its aggregate two-row buffer and each transient row encoding while both are
resident. Each durable `PreparedEvent` also carries credit for its independently
materialized `original_data` JSON tree while it is pending. A durable claim owns
that charged candidate plus an empty, worst-case row buffer; exact/fallback
settlement moves both into Session, and preferred-only settlement reuses the row
after an irreversible side effect instead of competing for fresh row memory.
An ordinary preferred settlement separately charges its candidate and row; if
resident pressure rejects either allocation, it uses the already charged
fallback. Claim growth allocates a replacement buffer before atomically swapping
counters, while rebind preserves the existing buffer. Clock rejection, injected clock
panic, writer poison, cancelled waits, and shutdown are covered as distinct
owner transfers; the Agent-level panic regression proves that the reserved
`step/end` and `turn/end` still close. A preferred-only result is never replaced
by its fallback: one transient Clock failure retries the exact Session-owned
candidate, with durable and memory-mode error classification tested separately.
A second Clock rejection preserves that root error and leaves the exact
candidate Session-owned for shutdown/recovery rather than obscuring it with
`NeedsAppendSettle`. Durable hot-attempt admission now also precharges its bounded
validator tables, block/source vectors, and partial-block table before opening
the stream. A plugin-defined block-type string is charged before Clock and is
rolled back with a rejected chunk; committed bookkeeping remains Session-owned
through seal, closure, barrier, and explicit attempt retirement. Provider/model
route facts share the immutable request config instead of duplicating strings.
After semantic stream validation, every hot durable chunk also charges its
complete typed `StreamChunk` graph before Clock. `block-end`, usage, and finish
split the child allocations retained by the fold from the transient chunk
owner and move that credit into a Session-owned account exactly once;
successful finish separately charges the assembled content-vector backing.
Clock or resident rejection rolls the candidate back without changing the
fold. Seal moves the values to named `PreparedAttemptParts` whose shared guard
keeps the account live across Agent cancellation or panic until closure,
barrier, and retirement. Retry and successful tool paths explicitly drop their
attempt-only finish/source aliases while that guard is still authoritative, so
later retry delay or tool execution cannot outlive the corresponding credit.
The final `Message` of a hot durable token-owned committed assistant append now
receives a Session-local surface lease after semantic validation and before any
pending wait or Clock call. The authoritative charged-assistant subset has a
separate 64 MiB steady gate; its physical lease pool is 128 MiB so old and
candidate surfaces may coexist. Projection, receipt, outcome, and request clones
share that wrapper lease, while every new durable node gets a fresh conservative
lease. A dropped wait remains Session-owned, and a claim-aware Clock rejection
restores that same leased candidate without reacquiring it; an ordinary
non-claim Clock rejection discards the candidate and releases its lease. This
remains a narrow substrate. A selected provider-usage baseline now receives a
separate non-surface-index lease under a 32 MiB steady gate and 64 MiB
old-plus-candidate pool, so it remains charged after attempt retirement;
estimated baselines retain no usage graph. Memory-mode and cold-recovered
messages/anchors remain deliberately unleased. Terminal failures cloned into a
turn outcome, non-chunk typed `EventKind` allocations, other surface event
types, containers/replacements, cold-recovery work, and complete closure
payload headroom are not charged yet. The cold scanner's fixed 9 MiB line
scratch is an independent bound and is not evidence that either scan or its
repair suffix participates in the shared 32 MiB live pool.
Consequently the complete shared 32/64/96/192 MiB invariants and Agent-level
provider-overflow interception/replay remain later hardening slices. The narrow
v0.1 Agent path now has the fixed 80% pressure trigger, 16% balanced tail,
8,192-token summary cap, summary dispatch, shrink check, checkpoint construction,
same-input rebuild, and one-attempt-per-turn guard.

The Phase 8 oracle is generated by
`scripts/generate-upstream-phase8-fixtures.ts`, checked by
`scripts/typecheck-upstream-phase8-fixtures.mjs`, and stored as
`tests/fixtures/session/upstream_phase8_oracle.json`. It executes the real
upstream Session, basic compactor, tool-result pruner, token projection, repair,
seed construction, and bounded JSONL scanner. It deliberately does not claim
that this compact fixture itself executes cold-load truncation/fsync/rollback;
those facts remain supported by the cited official tests and must receive Rust
failure-path tests.

The checkpoint was generated twice from the clean pinned checkout; both outputs
were byte-identical and matched the committed fixture. The checker, generator,
and fixture SHA-256 values are:

- checker: `8d331cceaeea192a28166660b7dbf0636452d8d64b31bbbd5676b3553f2fd68b`;
- generator: `7f56661f2898e4ed94327bdd79659a423f05c1e7ccea1cd7e60f8870417206f8`;
- fixture: `a1b505e769175e5f78d3ab7a15972e488ed504661bc16f8d17cf9253654ed96c`.

The focused official run passed 12 files and 519 tests. In addition to the
surface-delta suite, the two direct token-meter suites lock output-inclusive
usage and the max-of-usage-and-heuristic anchor:

```console
node /path/to/ds-harness-rs/scripts/typecheck-upstream-phase8-fixtures.mjs /path/to/deepseek-harness-upstream
cd /path/to/deepseek-harness-upstream
pnpm exec tsx /path/to/ds-harness-rs/scripts/generate-upstream-phase8-fixtures.ts /tmp/phase8-a.json
pnpm exec tsx /path/to/ds-harness-rs/scripts/generate-upstream-phase8-fixtures.ts /tmp/phase8-b.json
cmp -s /tmp/phase8-a.json /tmp/phase8-b.json
cmp -s /tmp/phase8-a.json /path/to/ds-harness-rs/tests/fixtures/session/upstream_phase8_oracle.json
pnpm exec vitest run packages/core/session/tests/session.spec.ts packages/core/session/tests/repair.spec.ts packages/core/session/tests/invariant.spec.ts packages/core/session/tests/surface.spec.ts packages/compaction/compaction/tests/invariant.spec.ts packages/compaction/compaction-basic/tests/compaction-basic.spec.ts packages/compaction/compaction-basic/tests/manual-compaction.spec.ts packages/compaction/compaction-tool-result-pruner/tests/tool-result-pruner.spec.ts packages/llm/token-meter/tests/context-breakdown-projection.spec.ts packages/llm/token-meter/tests/token-meter.spec.ts packages/llm/token-meter/tests/token-usage-projection.spec.ts packages/session/session-persistence-jsonl/tests/jsonl.spec.ts --configLoader runner
```

After wiring the Rust estimator and balanced-prefix selector, the three pinned
token-meter files plus `packages/compaction/compaction/tests/tool-pairing.spec.ts`
were rerun from the same clean checkout: Vitest 4.1.8 reported 4 files and 65
tests passed. This focused rerun validates the exact vocabulary used by the new
Rust producer. Deterministic Rust Agent tests now cover pressure and hard-limit
success, same-input continuation, invalid/tool-calling/non-shrinking output,
cancellation, and a one-summary-per-turn bound.

The generated oracle remains the comparison input for broader producer work.
The compaction row stays `partial`, rather than `compatible`, because the
user-approved v0.1 path intentionally omits provider-overflow replay, multiple
summary transactions, cold resident-credit parity, and exhaustive persistence
compatibility.

## Local research copy

Developers may create a clone outside this repository and detach it at the baseline:

```console
git clone https://github.com/deepseek-ai/deepseek-harness.git ../deepseek-harness-upstream
git -C ../deepseek-harness-upstream checkout --detach 47f943859bef60e4160492346772ded9b24f765a
```

The upstream clone is research input and must not be committed here.
