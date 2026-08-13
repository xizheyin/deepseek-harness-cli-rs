# Phase 2 validation

## Scope

Phase 2 validates the provider-neutral call boundary and a real DeepSeek chat-completions streaming provider. It includes preparation/default resolution, provider-owned retry facts, request serialization, credential handling, a replaceable and real HTTPS transport, incremental SSE framing, text/reasoning/tool/usage/finish translation, whole-stream validation, cancellation, timeout, error normalization, secret redaction, and resource limits.

It does not claim that `dsh` can run an Agent turn. The provider is a tested Rust library path and is not connected to the executable until the Agent Loop and terminal phases.

## Local validation

- Date: 2026-08-14 (Asia/Shanghai)
- Git base before the Phase 2 checkpoint: `5ed26c6a224070ee3d0e7ac20374862971dafeb3`
- Tested Phase 2 checkpoint: `c208dc1cf2e7529e981900bacb7d04dd6035a7dc`
- Host: macOS 27.0, arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Default Rust verification public-network use: no
- Credentials or API keys used: no; tests use conspicuously fake values
- User project data modified by tests: no

The repository-wide command completed successfully:

```console
./scripts/verify.sh
```

It ran formatting, all-target/all-feature locked compilation, tests, Clippy with warnings denied, deterministic whitespace self-tests, and complete-tree whitespace checks on the pinned toolchain. All 112 Rust tests passed:

- 34 DeepSeek private protocol/transport/security tests;
- 17 public Provider/model/default/retry/stream contract tests;
- 4 real `reqwest` loopback/isolated-child test entries, including 302 no-follow/no-forward and hostile process-proxy checks;
- 6 CLI foundation tests;
- 9 model-value tests;
- 42 Phase 1 session/replay/projection/resource tests.

`git diff --check` also passed. The locked dependency graph compiled on Rust 1.85.0. `cargo metadata --locked` found no dependency with a missing license expression; the recorded graph uses permissive license expressions described in `docs/design/provider.md`.

## Upstream behavior validation

The official checkout was a clean tracked tree at `47f943859bef60e4160492346772ded9b24f765a`. Node 26.0.0, TypeScript 6.0.3, and the checkout's locked `tsx` 4.22.4 statically checked the repository-owned oracle and ran it twice:

```console
cd /Users/xizheyin/workspace/deepseek-harness-upstream

node /Users/xizheyin/workspace/ds-harness-rs/scripts/typecheck-upstream-provider-fixtures.mjs \
  /Users/xizheyin/workspace/deepseek-harness-upstream

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-provider-fixtures.ts \
  > /tmp/dsh-phase2-oracle-1.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-provider-fixtures.ts \
  > /tmp/dsh-phase2-oracle-2.json

cmp -s /tmp/dsh-phase2-oracle-1.json /tmp/dsh-phase2-oracle-2.json
cmp -s /tmp/dsh-phase2-oracle-1.json \
  /Users/xizheyin/workspace/ds-harness-rs/tests/fixtures/provider/upstream_phase2_oracle.json
```

The type check passed; both outputs were byte-identical and matched the committed fixture. SHA-256:

- type checker: `71749297d2442f1cb23117dae53e6101d673a1593a577ccdbb33aed6634e25a1`;
- generator: `7b16cd4c26a49f7aa67c0ee16ed525b07172c0bf719091c619b4f0ebf72c64b6`;
- fixture: `cd1e4dca78ae4c242910e92fa832247d8135f322a892ae26faab2f5d85dcf0ed`.

The oracle covers exact/unlisted model defaults, default/custom retry facts, request serialization and rejection, fragmented UTF-8/BOM/CRLF/comment/multi-data SSE, `[DONE]`, reasoning/text/interleaved tool calls, usage/finish ordering, malformed/truncated streams, whole-stream rules, assembly, and the official closed-index contradiction.

Nine directly relevant upstream test files were also run:

```console
pnpm exec vitest run \
  packages/llm/llm-deepseek/tests/serialize.spec.ts \
  packages/llm/llm-deepseek/tests/sse.spec.ts \
  packages/llm/llm-deepseek/tests/translate.spec.ts \
  packages/llm/llm-deepseek/tests/adapter.spec.ts \
  packages/llm/llm-deepseek/tests/dynamic-config.spec.ts \
  packages/llm/llm/tests/invariant.spec.ts \
  packages/llm/llm/tests/assembler.spec.ts \
  packages/llm/llm/tests/service.spec.ts \
  packages/llm/llm-retry/tests/transport-recovery.spec.ts
```

Result: 9 files and 267 tests passed. These commands used installed dependencies, local fakes, and loopback servers only. They made no public network request and read no real credential. The opt-in upstream `adapter.e2e.ts` was not run because Phase 2 acceptance is deliberately keyless and does not require spending user quota.

## Failure and safety categories

| Category | Evidence |
| --- | --- |
| Normal | Exact preparation produces the logged effective config/context/retry facts; one real loopback POST yields text, usage, and a single successful finish. |
| Failure | Missing/invalid credentials, HTTP classes, quota/context detection, send/body transport failures, malformed JSON, missing body, empty output, EOF, and resource exhaustion produce stable terminal failure facts. |
| Rejection | Wrong provider, unsupported effort, unbound/mismatched preparation, unsafe endpoint, image input, invalid config, malformed stream grammar, redirects, and oversized input fail before unsafe publication. |
| Cancellation | Cancellation before credential work, during send, during success/error body reads, and with queued translated output emits only terminal `ABORTED`; consumer drop cancels owned work and drops the body. |
| Timeout | Initial send and each outstanding success/error body read share a tested idle deadline; complete SSE comments refresh it. |
| Recovery | The immutable provider remains reusable after rejected/failed requests; credentials resolve again for the next call. Provider retry execution is intentionally Phase 3. |
| Atomicity | Preparation is one-shot and instance-bound; invalid configuration does not create a provider, queued chunks are validated before publication, and terminal budget slots cannot be consumed by non-terminal output. |
| Resource safety | Requests, SSE response/line/event/error body, choices/tool deltas/blocks, retained output, encoded emitted chunks, event count, catalog, and retry facts have tested limits. |
| Privacy | Key-bearing types redact debug output and cannot serialize normally; request/header debug hides bodies; provider messages scrub the current key and Bearer patterns; a real 302 is not followed and receives no forwarded Authorization. |

## Independent review

Three independent read-only final reviews examined the candidate working tree from
separate angles:

- the protocol/public-API review checked prepared-call ownership, model and retry
  facts, request/stream compatibility, SSE publication order, and terminal
  outcomes;
- the security/resource/lifecycle review checked credential handling, secret
  propagation, redirects and process proxies, cancellation/drop behavior, and
  every enforced resource category;
- the documentation/release-evidence review checked README honesty, design and
  compatibility claims, oracle provenance and hashes, license metadata, test
  counts, and ignored private files.

The protocol and security reviews found and drove fixes for preparation/config
drift, provider-instance binding, IPv6 loopback recognition, pending-output
cancellation, terminal budget reservation, `[DONE]` and framing errors with later
bytes in the same read, long tool-name output amplification, incomplete
quota/context classification, secret-bearing debug output, control characters,
request-ID credential reflection, integer `Retry-After`, consumer drop, redirect
handling, environment-proxy credential interception, and missing
HTTP/body/credential branches. The reviewers then reran the relevant focused
tests and the repository-wide validation on the final candidate. All three
reported no remaining blocker; the documentation review's sole requested change
was to replace this formerly pending section with the final evidence now recorded.

## Known limitations

- The provider is not reachable from the `dsh` CLI until Phase 3 and Phase 7.
- Phase 2 freezes a retry policy but performs exactly one HTTP request; Agent-owned retry execution begins in Phase 3.
- Only the DeepSeek chat-completions provider is implemented; multiple provider implementations are beyond v0.1.
- Input images are represented by the core but this DeepSeek route rejects them before I/O.
- No real DeepSeek API smoke test was run; correctness is grounded in the pinned executable oracle, official offline tests, fake transport, and real loopback HTTP.
- Configuration is immutable per provider instance and has no Cordis/HMR settings registry.
- Resource, endpoint-trust, attribution, error-message, malformed-value, and closed-index policies are documented intentional differences in `docs/compatibility.md`.

## Remote acceptance

The Phase 2 checkpoint was pushed non-forced to `origin/main`. GitHub Actions
run [31748523315](https://github.com/xizheyin/deepseek-harness-cli-rs/actions/runs/31748523315)
completed successfully on Ubuntu 24.04 for the exact checkpoint
`c208dc1cf2e7529e981900bacb7d04dd6035a7dc`.
