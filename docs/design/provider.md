# DeepSeek streaming provider design

## Purpose and scope

Phase 2 turns provider-neutral messages into one real DeepSeek chat-completions request and turns the returned SSE byte stream into provider-neutral `StreamChunk` values. “SSE” is the text-event framing used by streaming HTTP responses; network reads may split at any byte, so it must be reconstructed before JSON is parsed.

This phase includes authentication, request serialization, an injectable HTTP boundary, incremental SSE parsing, DeepSeek response translation, stream grammar checks, cancellation, idle timeout, error normalization, resource limits, and secret redaction. It does not run an Agent Loop, execute tools, retry a model step, write session events, or add a CLI command. Those begin in Phase 3 and Phase 7.

The semantic baseline is DeepSeek Harness commit `47f943859bef60e4160492346772ded9b24f765a`. Exact source and test paths are recorded in `docs/upstream.md`.

## Ownership and public boundary

`src/provider/` owns the provider-neutral call boundary:

- `ModelProvider::prepare_call` resolves one proposed route before logging and returns a one-shot `PreparedProviderCall`;
- `PreparedProviderCall` freezes the effective config, exact model context, adapter-default markers, and provider-owned retry policy, and is bound to the exact provider instance that prepared it;
- `ProviderRequest` owns that prepared call plus system prompt, messages, tools, request purpose, and optional session ID;
- `ModelProvider::stream` consumes the request and returns a lazy, pull-based stream of provider-neutral chunks;
- `StreamValidator` owns the whole-stream grammar and rejects an invalid chunk before publishing it;
- `ProviderStreamError` is reserved for a broken live-stream contract. Ordinary credential, HTTP, timeout, cancellation, and DeepSeek response failures become one terminal `finish` chunk.

`src/provider/deepseek/` owns every DeepSeek-specific wire detail:

- immutable connection configuration and credential references;
- provider-neutral message to chat-completions JSON conversion;
- HTTP request/response transport types and the real reusable `reqwest::Client`;
- bounded incremental SSE framing;
- stateful wire JSON to `StreamChunk` translation;
- HTTP/failure classification and redaction.

DeepSeek wire structs remain private. Agent code will depend only on `ModelProvider`, `PreparedProviderCall`, `ProviderRequest`, and the Phase 1 model types. A public unbound `PreparedProviderCall::new` exists for deterministic fake providers; the real DeepSeek provider rejects it as `INVALID_PREPARED_CALL`.

There are two real seams because both will vary independently:

1. `ModelProvider` lets Phase 3 use a deterministic fake model.
2. the DeepSeek module's private `HttpTransport` lets provider tests split bytes, fail connections, and control timing without the internet.

## Runtime dependencies

Phase 2 adds only libraries used by the production path: `reqwest`/Rustls for HTTPS, `tokio` and `tokio-util` for async polling/cancellation, `futures-util` for streams, `secrecy` for redacted secret ownership, and `httpdate` for `Retry-After`. Default features are disabled on `reqwest`; HTTP JSON DTO support, compression, cookies, system/environment proxy use, and HTTP/3 are not used. The client explicitly disables redirects, proxy discovery, and reqwest's low-level automatic retry policy so one provider stream owns exactly one HTTP attempt. `Cargo.lock` is committed, and the complete locked graph is compiled on the pinned Rust 1.85.0 toolchain.

The direct dependencies are MIT, Apache-2.0, or dual-licensed under those terms. A `cargo metadata --locked` audit of the full target-aware graph also found only permissive licenses (MIT/Apache/ISC/BSD/Zlib/Unicode/CC0/Unlicense/CDLA-Permissive and compatible combinations), with no missing license field. Release packaging must preserve any notices required by the exact locked artifacts it distributes.

## Preparation and request path

Before appending a request header, Phase 3 must call `prepare_call`. DeepSeek looks up the exact model ID in its advisory catalog without using that catalog as an allowlist. An exact `contextWindow` or `maxTokens` wins over the adapter-wide fallback; an explicit call `maxTokens` wins over both. Missing reasoning effort and output cap are materialized while unknown config extension fields remain intact. The prepared value also snapshots the default or configured retry policy and a process-local provider identity. Moving it into `ProviderRequest` makes dispatch one-shot; another provider instance cannot accept it.

The default catalog contains `deepseek-v4-flash` and `deepseek-v4-pro`, each with a 1,000,000-token context. Unlisted models remain valid and use the adapter-wide 1,000,000-token context and 256,000-token output cap. The default retry policy records two retries for `EMPTY_RESPONSE`, `RATE_LIMIT`, `SERVER`, `TIMEOUT`, and `TRANSPORT`, with 500 ms initial delay, 10,000 ms maximum delay, and 0.1 jitter. Phase 2 only freezes these facts; Phase 3 decides whether and when to execute a bounded retry.

One stream call consumes one prepared immutable configuration and resolves its named credential once. It then:

1. checks cancellation and verifies that preparation belongs to this provider instance;
2. validates and normalizes the API key;
3. serializes messages and tools before any network I/O;
4. sends exactly one `POST {baseURL}/chat/completions` request;
5. reads one SSE response lazily as the consumer asks for chunks;
6. emits one terminal finish or one explicit protocol error, then stops.

The provider never retries itself: exactly one stream call means at most one HTTP request. A retry changes Agent step semantics, so the prepared call preserves policy facts and a failure preserves `Retry-After`; Phase 3 owns any bounded retry decision.

The request follows the official adapter rules:

- an explicit system prompt is the first wire message;
- text blocks concatenate without inserted separators;
- assistant `content` is always a string, including `""`;
- assistant reasoning is replayed only when that message also contains tool calls;
- tool-result blocks become separate `role: "tool"` messages and empty output becomes `(no output)`;
- image blocks fail before I/O with `UNSUPPORTED_CONTENT`;
- empty tools are omitted;
- `off` disables thinking, while `high` and `max` enable it and set the wire effort;
- a session-title call always disables thinking;
- optional sampling fields are omitted rather than written as `null`.

Configuration stores only an environment-variable name, never a literal key. The default reference is `DEEPSEEK_API_KEY`, the default endpoint is `https://api.deepseek.com`, the default output cap is 256,000 tokens, and the default idle timeout is 300 seconds. Credential resolution happens for every call so a rotated environment value reaches the next request, never an in-flight one.

## Pull-based streaming, cancellation, and timeout

The stream is lazy: it does not spawn a reader task or fill a background channel. One consumer poll advances at most far enough to produce the next chunk. This gives natural backpressure—when the consumer pauses, the provider does not continue downloading an unbounded response.

One child cancellation token covers the HTTP send and every response-body read. Dropping the consumer stream cancels that child and drops the response body. A caller cancellation produces terminal `aborted / ABORTED`; it does not look like a normal stop.

The idle timeout measures one outstanding request for the next provider item, not the whole model call. Time spent by the consumer handling an already-delivered chunk does not count. A complete SSE comment is transport activity and restarts the deadline without producing a model chunk. Incomplete raw bytes that produce neither a comment nor a model event do not restart it, matching the upstream pull boundary.

No block-end is invented after an interrupted response. Previously emitted deltas remain observable, followed directly by error or aborted finish, so Phase 3 cannot execute a half-built tool call.

## SSE and translation state

The SSE parser owns a byte line buffer, current event data, BOM state, total-byte counter, and pending parsed items. It accepts LF, CRLF, bare CR, UTF-8 split between network reads, comments, non-data fields, and multiple `data:` lines. Invalid UTF-8 is decoded with replacement like the upstream `TextDecoderStream`. Only a blank line dispatches an event. An unterminated tail—even `data: [DONE]`—does not dispatch.

The exact data value `[DONE]` completes the provider response. Data after it is not read. EOF before it is `STREAM_CLOSED`.

The translator owns:

- at most one reasoning block and one text block;
- tool-call blocks keyed by DeepSeek's wire index;
- a monotonically assigned internal block index and first-open order;
- the latest finish reason and latest usage record;
- accumulated-output and block budgets.

Reasoning is handled before visible text in each choice. Empty reasoning/text does not open a block. Tool-call fragments with the same wire index append arguments and update the latest ID/name. At `[DONE]`, blocks close in first-open order, then usage is emitted, then the single finish. Prompt cache reads are subtracted from `prompt_tokens` because internal token counts are disjoint.

The generic stream validator tracks open blocks, every index ever used, usage, finish, and chunk count. Deltas and closes must match an open block. Usage occurs at most once. Finish is terminal and required; only error or aborted finish may leave blocks open. Unknown live chunk types are rejected. Unknown chunks already stored in a Phase 1 log remain losslessly replayable; importing history and trusting a live provider are different boundaries.

## Errors and stable outcomes

DeepSeek and transport failures are converted to `LlmFailure` facts and one terminal chunk:

| Condition | Stable code |
| --- | --- |
| missing or unusable credential | `MISSING_CREDENTIAL` / `INVALID_CREDENTIAL` |
| 401 or 403 | `AUTH` |
| exhausted quota/balance/credits | `QUOTA` |
| other 429 | `RATE_LIMIT` |
| context-specific HTTP 400 | `CONTEXT_WINDOW_EXCEEDED` |
| other HTTP 400 | `INVALID_REQUEST` |
| HTTP 5xx | `SERVER` |
| other HTTP status | `HTTP_<status>` |
| send/body transport failure | `TRANSPORT` |
| idle deadline | `TIMEOUT` |
| caller cancellation | `ABORTED` |
| malformed/invalid response JSON | `MALFORMED_RESPONSE` |
| EOF before `[DONE]` | `STREAM_CLOSED` |
| no response body or successful empty completion | `EMPTY_RESPONSE` |
| a configured response budget is exceeded | `RESPONSE_TOO_LARGE` |

HTTP failures retain a valid status, positive `Retry-After`, and request ID (`x-request-id` before `x-deepseek-request-id`). A clean HTTP EOF without `[DONE]` stays distinct from an abrupt socket failure.

A violated provider-neutral grammar yields `ProviderStreamError`, because it indicates a broken adapter or fake provider rather than an ordinary model failure. Phase 3 must close its step truthfully when it sees that error.

## Secret and endpoint policy

The API key is held in a secret wrapper that does not implement ordinary display or serialization, redacts debug output, and zeroizes its owned allocation on drop. This reduces accidental copies; it is not hardware isolation, and the HTTP library may still need a temporary Authorization value.

Requests and header maps have custom debug output that hides sensitive values and body text. Provider error bodies are bounded, control-cleaned, and scrubbed for the exact current key and Bearer-looking tokens before a message can become a durable `LlmFailure`. Raw response payloads and Authorization values never enter errors or session-ready chunks.

The default endpoint is official HTTPS. A custom endpoint must be supplied by a trusted caller or process-level configuration; this project will not load a repository `.env` or workspace file that can silently redirect a credential. URLs with user information, query strings, or fragments are rejected. Remote plain HTTP is rejected; plain HTTP is accepted only for an explicit loopback endpoint used by offline tests. Redirect following is disabled so an Authorization header cannot be forwarded by surprise.

The Rust client identifies itself with this community project's product/version/repository rather than claiming to be the official JavaScript product. It omits the official persistent anonymous-user header. Both are documented intentional differences with no change to model messages or stream ordering.

## Resource limits

The upstream in-memory adapter has no equivalent fixed limits. This CLI rejects oversized untrusted responses before they can consume unbounded memory:

- complete serialized request: 8 MiB;
- total successful SSE response bytes: 8 MiB;
- one SSE line or one dispatched event: 1 MiB;
- one HTTP error body: 64 KiB;
- model blocks/tool calls: 128;
- accumulated reasoning/text/tool arguments: 4 MiB;
- total encoded provider-neutral chunk payloads: 10 MiB;
- emitted chunks from one call: 4,000;
- choices or tool deltas in one wire item: 128 each;
- request messages: 4,096; request tools: 256;
- advisory models: 256; retryable codes: 256.

The chunk limit stays below the Phase 1 session's 4,096-event ceiling, but Phase 3 must also reserve room for step, request, message, tool, and closing events. Byte/framing limits and critical count limits have boundary or over-limit tests; all limits have a rejection test. A rejected request or response leaves the provider reusable for a later small call.

## Intentional differences

Phase 2 deliberately differs from the pinned upstream in these observable ways:

1. A block index cannot be reused anywhere in one live stream. Upstream's invariant permits reuse after close, but its assembler then ignores the reopened block; DeepSeek's own translator always allocates monotonically. Rust rejects the contradictory edge case before publication.
2. Malformed but JSON-valid typed wire fields, impossible usage arithmetic, and unknown live chunk types fail closed instead of passing through a loose JavaScript cast.
3. Response/request budgets reject very large streams that the in-memory upstream adapter attempts to retain.
4. Error text is scrubbed rather than retaining a provider body that may echo a credential.
5. Workspace files cannot redirect the key; remote HTTP and redirects are refused.
6. The User-Agent names this independent Rust project and no persistent anonymous-user ID is sent.
7. Provider connection configuration is immutable for one Rust provider instance; a caller creates a replacement instance to change the endpoint. Credential values still rotate per request. Cordis settings hot reload and registry replacement are outside v0.1.
8. Explicit `maxTokens: 0`, unsafe catalog integers, and oversized catalog/retry strings are rejected before dispatch. The pinned JavaScript preparation path can carry some of these values farther because its layers validate different subsets. Rust treats them as invalid or resource-hostile configuration; a caller must supply a positive safe output cap and bounded metadata.
9. Stable failure codes and structured HTTP facts match the upstream contract, but user-visible failure messages are bounded, control-cleaned, secret-scrubbed summaries rather than byte-for-byte copies of endpoint or transport text.

Each difference must have a paired upstream observation or a focused Rust safety test before its compatibility row can be marked `intentional-difference`.

## Verification plan

The committed TypeScript oracle runs real pinned model/default/retry resolution, `serializeRequest`, `parseSse`, `translate`, stream invariant, and assembler behavior. It records exact model fallbacks, retry facts, full request JSON, fragmented SSE, interleaved reasoning/text/tools, latest usage, finish mapping, partial output before failures, legal and illegal stream traces, and the upstream index-reuse contradiction. It is type-checked and run twice for byte-identical output; normal Rust verification consumes only the committed fixture.

Default Rust tests use synthetic keys and an in-memory fake transport. They cover every byte split, UTF-8/CRLF splits, BOM, comments, multi-data, `[DONE]`, EOF, malformed JSON, interleaved calls, error classification, cancellation at each boundary, timeout/comment keep-alive, consumer drop, one-request-only behavior, backpressure, redaction, endpoint policy, and every resource limit. A small loopback HTTP server proves the public `DeepSeekProvider` reaches the real reqwest transport without internet access.

No default test reads `DEEPSEEK_API_KEY`, contacts a public host, consumes quota, or opens user project files. A future optional live smoke test must require two explicit opt-ins, use a tiny harmless prompt, and remain outside Phase 2 acceptance when no maintainer credential is supplied.
