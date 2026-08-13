# Upstream baseline

DeepSeek Harness is the semantic reference for this project's agent core. The Rust implementation targets observable behavior, not a line-by-line translation of TypeScript or Cordis.

## Pinned revision

- Repository: <https://github.com/deepseek-ai/deepseek-harness>
- Commit: [`47f943859bef60e4160492346772ded9b24f765a`](https://github.com/deepseek-ai/deepseek-harness/tree/47f943859bef60e4160492346772ded9b24f765a)
- Commit date: 2026-08-13
- Baseline checked: 2026-08-14
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

## Local research copy

Developers may create a clone outside this repository and detach it at the baseline:

```console
git clone https://github.com/deepseek-ai/deepseek-harness.git ../deepseek-harness-upstream
git -C ../deepseek-harness-upstream checkout --detach 47f943859bef60e4160492346772ded9b24f765a
```

The upstream clone is research input and must not be committed here.
