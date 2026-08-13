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

## Local research copy

Developers may create a clone outside this repository and detach it at the baseline:

```console
git clone https://github.com/deepseek-ai/deepseek-harness.git ../deepseek-harness-upstream
git -C ../deepseek-harness-upstream checkout --detach 47f943859bef60e4160492346772ded9b24f765a
```

The upstream clone is research input and must not be committed here.
