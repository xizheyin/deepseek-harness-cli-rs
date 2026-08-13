# Core session design

## Purpose and scope

Phase 1 builds the provider-neutral facts that later phases will share. It does not call DeepSeek, run tools, write session files, or render a terminal UI.

The core has two related views:

1. the **event log**, an immutable, append-only record of everything that happened;
2. the **projection**, state rebuilt from that log, including open turn/step state and the ordered messages visible to the next model request.

The log is the source of truth. A projection can be discarded and rebuilt without changing its result.

This design follows DeepSeek Harness commit `47f943859bef60e4160492346772ded9b24f765a`. The primary evidence is:

- `packages/llm/llm/src/message.ts` and `packages/llm/llm/src/types.ts`;
- `packages/core/session/src/types.ts`, `index.ts`, `surface.ts`, `invariant.ts`, and `json.ts`;
- `packages/core/session/tests/session.spec.ts`, `properties.spec.ts`, `surface.spec.ts`, and `invariant.spec.ts`;
- `packages/core/agent-loop/src/agent.ts` and the rejection/cancellation tests cited in `docs/upstream.md`.

## Provider-neutral model vocabulary

`src/model/` owns messages, content blocks, provider failures, stream chunks, finish reasons, token usage, and tool schemas. `src/json_value.rs` owns the bounded opaque-JSON boundary shared by model and session values. These types contain provider and model names but no DeepSeek HTTP or SSE fields.

Messages have stable typed IDs and an explicit role, source, and ordered content blocks. Tool arguments remain the raw JSON string emitted by the model. A tool-result message contains exactly one tool-result block; its source call ID and block call ID must agree. Raw-backed values preserve extension fields and explicit `null` values while exposing typed facts that this build understands. Merge-extensible message sources, content blocks, finish reasons, and turn-end reasons therefore remain replayable when a plugin or newer producer adds a variant.

`FinishReason` describes why one provider stream stopped. `TurnEndReason` is separate and describes why a complete agent turn ended. They must not be flattened into one enum:

- a rejected pre-step turn is `blocked` and can contain no step;
- a user cancellation is `aborted { reason: user }`;
- `interrupted` is reserved for durable crash repair;
- `max-tokens` is not a timeout.

## Wire shape

The session header carries the session ID and format version once:

```text
SessionHeader { version: 0, id, createdAt, ... }
```

Each event uses the upstream envelope:

```text
{ type, seq, time, data, sourceEventSeqs?, surfaceOp?, ignorable? }
```

Events do not repeat the session ID, format version, or an event UUID. `seq` is the event identity and must equal its zero-based position in the log. Event timestamps are signed JavaScript-safe integer milliseconds because upstream accepts negative timestamps in imported test logs; newly created timestamps are non-negative.

The codec rejects unknown envelope keys and an `ignorable` value other than `true`. An unknown required event is rejected. An unknown event carrying `ignorable: true` is retained losslessly as a projection no-op so later encoding does not change sequence numbers or discard evidence. Unknown events may not claim surface metadata.

Phase 1 exposes a compact JSON snapshot containing one header and its event array for deterministic tests and in-memory interchange. It is not the Phase 8 durable JSONL format. The validated raw header and each event payload are also available through borrowed read-only accessors so extension data is observable without serializing the whole session.

## Event ownership and append order

`Session` owns:

- one validated header;
- an owned `Vec<SessionEvent>` that is never rewritten;
- one projection derived from the complete committed prefix;
- a clock used only to assign creation and event times.

The append boundary accepts owned event data, not a caller-supplied sequence or timestamp. One append proceeds in this order:

1. validate the event payload and its message shape;
2. calculate the next contiguous sequence and read the clock;
3. build a complete candidate event;
4. calculate the next turn/step/tool state without mutating committed state;
5. calculate the next surface without mutating committed state;
6. reserve log capacity;
7. append the event and install both already-validated projections.

Any ordinary error before step 7 leaves the log, next sequence, open boundaries, pending calls, and surface unchanged. Rust ownership prevents callers from retaining mutable aliases into stored `String`, `Vec`, or JSON values. Returned slices are read-only; callers receive clones of projected messages.

Extensible values that expose both typed facts and retained raw JSON are rechecked at live append. Their typed tag must describe the same value that serialization will write, and removed request-header vocabulary cannot be reintroduced through a public enum constructor. This keeps live append and replay from disagreeing about the same event.

## Turn, step, and tool state

Turns start at 1 and increase without gaps. Each turn's steps start at 1 and increase without gaps. Turns and steps cannot nest, and a turn cannot close while a step is open.

Assistant chunks/messages and fresh tool calls/results must name the currently open turn and step. A user message may exist outside a turn. Request-header, request-context, and todo events require an open turn. `session/end-seed` changes no cursor and is valid inside an unfinished turn.

One step tracks the set of call IDs recorded by `tool/call`. A fresh `tool/result` must consume a matching ID from that same step. Ending a step is legal with unresolved calls and clears them, so a later step cannot supply the missing result. This permits truthful error/crash prefixes; Phase 8 will append repair events instead of rewriting history. The upstream `TOOL_NOT_STARTED` synthetic repair result is the sole result allowed without a prior `tool/call`.

DeepSeek Harness installs these relational checks through a Cordis invariant companion. The Rust session always enables them. This is an intentional architecture difference: canonical Harness flows behave the same, while a malformed custom producer is rejected even when no optional plugin-registration step exists.

## Model-visible surface

Only three event types can enter the model-visible surface:

- `user/message`;
- `assistant/message`;
- `tool/result`.

They must carry a surface operation. All other event types must not carry one.

`append` adds the event to the surface tail. `replace { start, end }` replaces an inclusive range of current surface nodes with one new node without deleting any old log event. Both endpoints must be live nodes and appear in order. `sourceEventSeqs`, when present, must contain unique earlier sequences; a replacement must cite every shadowed node. An explicit empty list is allowed only on `assistant/message`, representing a known empty provider stream.

A tool-result replacement can target exactly one current tool result and may change only its nested model-facing result content. It is a rewrite performed during an open turn and does not represent another tool execution.

The model-message projection walks current surface nodes in order. Boundary events and raw chunks never enter it. An assistant message with empty content remains a surface node, so usage remains durable, but produces no model message.

The terminal transcript must later use append-origin log events rather than the current surface. Otherwise compaction would make already-seen conversation disappear from the human display.

## Seed and replay

Pure replay starts with an empty projection and applies the same known-core payload, state-transition, and surface validation as live append. Import alone may retain an unknown event carrying `ignorable:true`; the live typed API cannot manufacture unknown events. A gap, duplicate, reordering, malformed surface operation, or illegal relation fails with the event index and leaves no partially constructed public session.

A fresh session has no seed marker. Constructing from an explicit seed records `firstLiveSeq` as the seed length and appends one `session/end-seed` marker unless the seed already ends in that marker. Even an explicitly empty seed receives one marker. The marker does not close an open turn or step.

Incomplete but internally valid prefixes are accepted. A log ending inside a turn or step describes live work or a crash, not corrupt history. Disk-tail detection and repair belong to Phase 8.

## Errors, cancellation, and side effects

The library uses structured errors for invalid IDs/numbers, headers, codecs, state transitions, surfaces, clocks, and capacity. Production code does not panic for external input.

Phase 1 performs no network, filesystem, subprocess, approval, or asynchronous work. Cancellation and rejection are represented as tested event sequences; automatic orchestration and resource cleanup begin in Phase 3 and Phase 6.

## Limits and intentional differences

- Cordis services, scoped observers, hot reload, and TypeScript declaration merging are not reproduced. Relational invariants are always enabled rather than installed as an optional companion.
- Unknown plugin message blocks/sources, future finish/turn-end values, header extensions, and fields added to known event payloads are retained as bounded raw JSON. Unknown event types are accepted only when `ignorable:true`; this moves the official persistence coordinator's fail-closed policy into the Phase 1 snapshot import seam.
- The raw upstream session admits some malformed values under already-known field names because it validates only their minimum event shell. Rust rejects examples such as `maxTokens:null`, a non-string stop item, or an incomplete tool schema during import. This fail-closed choice prevents malformed typed data from reaching a future provider; the compatibility table records its user impact and paired oracle/Rust test.
- Rust admits at most 16 MiB of snapshot input and 16 MiB of compact header/event payload data in one live session. A value is limited to 8 MiB, 128 nesting levels, and one million nodes; a header to 64 KiB; a session and provenance list to 4096 entries. These are application budgets, not a claim that peak process memory is 16 MiB. Serialization/parsing has bounded temporary overhead and can be made streaming when Phase 8 adds durable storage.
- Opaque integral JSON must remain within JavaScript's safe-integer range. The official JavaScript runtime can accept a larger literal and silently round it; Rust rejects it so persisted evidence is never changed without notice.
- JSON objects are serialized in Rust's deterministic key order. Field order has no JSON meaning, but bytes can differ from upstream `JSON.stringify` output. Business comparisons must use explicit known-field equality rather than serialized bytes or derived equality.
- JavaScript-only getter, prototype, sparse-array, and `Object.freeze` behavior is not copied. Rust tests instead prove owned snapshots and no externally mutable log aliases.
- Observer publication and durability checkpoints are deferred until a real consumer exists. They must remain post-commit and cannot change accepted history when introduced.

## Test evidence

Deterministic tests use fixed IDs and a fixed or sequence clock. They cover:

- complete, error, blocked, aborted, max-token, and interrupted turn outcomes;
- contiguous sequences and exact event JSON round trips;
- invalid headers/envelopes, illegal turn/step relations, and call/result mismatch;
- unresolved calls at step end and the repair-result exception;
- append and replace surface operations, provenance, tool-result rewrite limits, and atomic failure;
- empty assistant messages and exclusion of chunks/boundaries from model history;
- explicit empty seeds, open-tail seeds, marker idempotence, unknown required rejection, and unknown ignorable preservation;
- equality between incremental projection and full replay at every tested prefix.

The official fixture is generated by running `scripts/generate-upstream-session-fixtures.ts` inside the clean pinned upstream checkout. `scripts/typecheck-upstream-session-fixtures.mjs` first checks that oracle against the pinned upstream TypeScript source graph; successful `tsx` execution alone is not counted as type evidence. The fixture records the commit and exact model, attachment, session source/test paths, and two consecutive generator runs must be byte-identical. Rust comparison consumes complete messages/events and projections as JSON values; only timestamps and IDs are fixed rather than randomly generated. It covers the complete Phase 1 model-value vocabulary, canonical and illegal session traces, forward-compatible/null values, request projections, numeric replacement equality, and the documented architecture differences.
