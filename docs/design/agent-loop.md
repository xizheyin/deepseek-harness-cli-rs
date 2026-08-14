# Bounded Agent Loop design

## Purpose and scope

Phase 3 connects the Phase 1 append-only session to the Phase 2 provider and a
minimal tool-execution seam. One public `run_turn` call can make several model
requests: a model response may request tools, the tool results become logged
model context, and the next step asks the model to continue. Every ordinary
completion, provider failure, tool failure, rejection, cancellation, timeout,
or configured limit closes the events it opened.

The semantic baseline is DeepSeek Harness commit
`47f943859bef60e4160492346772ded9b24f765a`. Phase 3 reproduces its observable
turn, step, request, retry, chunk, message, and tool-correlation rules. It does
not reproduce Cordis, live inbox steering, Hooks, a real tool registry,
approval, filesystem access, Shell, persistence, or a terminal UI. Those
boundaries belong to Phases 4–8.

The three units must not be confused:

- a **turn** is one admitted batch of user work;
- a **step** is one successful model response and the tool calls that response
  requested;
- an **attempt** is one provider request inside a step. A retry is another
  attempt in the same open step.

## Public ownership boundary

`AgentLoop` exclusively owns one `Session` while a turn is running. Its other
dependencies are immutable shared values:

- the existing `ModelProvider` prepares and streams one attempt;
- `AgentLoopConfig` owns the ordered schemas, while `ToolExecutor` executes one
  already-recorded invocation without writing session events itself;
- `AgentRuntime` mints opaque IDs and supplies retry jitter; Tokio's paused
  clock is the deterministic timer boundary in tests;
- `AgentLimits` validates all configurable step, attempt, token, tool, time,
  and result budgets before the loop runs.

`run_turn` takes an explicit proposal. An entered proposal contains owned
user-role messages; a rejected proposal closes a `blocked` zero-step turn.
This is the Phase 3 replacement for the upstream `agent/pre-step` waterfall.
An empty entered proposal closes a `completed` zero-step turn. The real inbox,
runtime-context projection, and live steering arrive with the interactive
driver in Phase 7.

The public return value is the reason actually committed in `turn/end` plus
turn counters. Ordinary model/tool/limit outcomes return successfully because
the durable log is their result. A fatal error is reserved for a failure to
maintain the log itself—for example a clock failure while closing a boundary.
The API never reports a balanced turn when its closing event did not commit.

## State machine and event order

One entered turn follows this state machine:

```text
turn/start
  ├─ rejected or empty
  │    └─ turn/end
  └─ step/start
       ├─ user/message × N
       ├─ request/header? → request/context?
       └─ provider attempt
            ├─ failed chunks → llm/retry → wait → llm/retry-started
            │                    └─ another attempt in the SAME step
            └─ successful chunks → assistant/message
                                  ├─ no tools → step/end → turn/end
                                  ├─ max-tokens → step/end → turn/end
                                  └─ tool/call → execute → tool/result × N
                                                   └─ step/end → next step
```

The exact invariants are:

1. `turn/start` commits before a proposal is rejected or entered.
2. `step/start` commits only for an entered model step.
3. Every message visible to the request is first represented by a surface
   event. Every attempt rebuilds messages from `Session::messages`; it never
   accumulates a second private history.
4. A full `request/header` and changed `request/context` commit before provider
   dispatch. A new loop writes `initial` over a fresh log and `resume` over a
   log with an existing header; later equivalent headers are not repeated.
5. Every provider chunk commits before assembly. Failed-attempt chunks remain
   log-only and never receive an `assistant/message` surface anchor.
6. A retry never starts another step. `llm/retry` commits before the
   cancellable delay; `llm/retry-started` commits only after the delay finishes
   and immediately before the next attempt.
7. A successful response has exactly one `assistant/message` whose
   `sourceEventSeqs` are exactly the successful attempt's chunk sequences.
8. `max-tokens` keeps safe text/reasoning, removes every tool-call block, never
   executes tools, and remains the turn reason.
9. Every `tool/call` commits before its executor is entered. Every ordinary
   result cites that call event and results commit in model order.
10. Tool results cause a following model step even when that step has no new
    direct human message.
11. `step/end` commits before `turn/end` on every path that opened a step.

The fixed system string and ordered schemas are full request-header facts. The
loop uses field-wise `LlmCallConfig` equality plus adapter-default fields,
system text, and ordered schema JSON; it does not use derived Rust equality or
JSON object property order to decide a header change.

On resume, adapter-owned defaults are removed from the next proposal and
re-resolved by the current provider. An explicit reasoning effort is restored
only for the same provider/model route when it was not an adapter default.
Each attempt receives a newly prepared one-shot call; a dispatched preparation
is never reused.

## Stream assembly

Phase 2 already enforces a strict live-stream grammar. The Agent assembler
therefore records block order at `block-start`, takes the authoritative block
at `block-end`, keeps the single usage record, and requires terminal finish.
For a successful finish all blocks are closed. Error or aborted finishes can
leave partial blocks, but they do not create an assistant message.

An adapter `finish {kind:"aborted"}` is a provider failure and closes the turn
as `error` unless the turn cancellation token was actually cancelled. It must
not be mistaken for Ctrl+C. A provider protocol error or a stream that ends
without finish is a structured turn error; partial chunks remain non-surface.

Before an assistant message containing tool calls becomes model-visible, the
loop validates the complete call set. Duplicate call IDs or a call-count limit
fail the attempt without committing the assistant message or executing any
side effect. Empty arguments become `{}`. Invalid or oversized arguments keep
their raw value in `tool/call`, skip the executor, and produce a bounded error
`tool/result` so the model can correct itself.

## Tool seam and cancellation

Phase 3 intentionally uses a serial executor. It is enough to prove intention
before execution, call/result correlation, bad-input feedback, cancellation,
and multi-step context without pre-implementing the Phase 4 registry and
policy pipeline.

`ToolExecutor` receives the call ID, name, raw arguments, parsed bounded JSON,
and a child cancellation token. Its result distinguishes model-facing tool
failure from executor infrastructure failure:

- success, unknown tool, policy rejection, invalid arguments, tool error,
  cancellation, and timeout become an ordered `tool/result`;
- an executor contract failure or panic closes the turn as an infrastructure
  error and does not invent a result for the started call.

External turn cancellation has the typed `user` cause. It cancels the current
provider, retry wait, or tool. A started tool gets an `ABORTED` result after
it settles or reaches its bounded shutdown deadline. Later unstarted calls get
synthetic `tool/call`/`tool/result` pairs with
`ABORTED_BEFORE_DISPATCH`. No reader, timer, or tool task is detached from the
turn future.

Phase 5 will replace deterministic fake rejection with the real approval
pipeline. Phase 6 will add process-group termination; cancellation here is a
cooperative async contract and a bounded future drop, not an operating-system
sandbox or process kill guarantee.

The caller must cancel through the supplied token and continue awaiting
`run_turn` until it returns. Dropping a polled `run_turn` future is equivalent
to a process crash: Rust cannot run async closing work from `Drop`, so Phase 8
must repair that open tail before resume.

## Retry execution

The prepared provider call freezes the retry facts, while the Agent Loop
executes them. This merges two upstream modules (`agent-loop` and the optional
`llm-retry` plugin) into one owned Rust state machine without changing normal
event order.

Normal policy retries only listed stable failure codes and no more than its
`maxRetries`. Always policy retries every provider failure. Both use capped
exponential backoff and symmetric jitter. A positive provider `Retry-After`
not exceeding the configured maximum is used verbatim. In normal mode an
over-maximum value means no retry; in always mode it falls back to local
backoff.

Every chain has one opaque retry ID and a stable policy key. Retry numbering is
one-based and scoped to the open turn/step/provider/policy. A cancellation
during backoff leaves the scheduled `llm/retry` event but writes neither
`llm/retry-started` nor another request.

Unlike upstream's unbounded always mode, every policy is also constrained by
the Rust per-step retry, per-turn attempt, duration, event, and token limits.
Hitting a Rust safety limit closes the turn with the corresponding stable
`AGENT_*` error code rather than pretending the model completed.

## Exact session reservation

The Session admits at most 4,096 events and 16 MiB of retained header/event
payload JSON, while one Phase 2 provider stream can contain 4,000 chunks. The
Agent therefore cannot append until failure and merely hope that closing
events still fit.

`SessionReservation` maintains two exact counters:

```text
committed events + reserved fallback events <= event capacity
committed payload bytes + reserved fallback payload bytes <= byte capacity
```

A claim is created from a concrete fallback event, whose payload is validated
and encoded by the same code used by `Session::append`. Callers cannot reserve
an invented byte count. Ordinary appends may use only capacity above all live
claims. Settling a claim first releases that claim's floor: the preferred
event commits if it fits without consuming other floors; otherwise the exact
fallback commits.

The Agent uses claims as follows:

- before `turn/start`, claim a small `turn/end` event carrying
  `AGENT_EVENT_BUDGET`;
- before `step/start`, claim its exact `step/end`;
- before a tool-bearing assistant message, atomically claim that message, each
  exact call, and one bounded result fallback per call;
- chunks, headers, contexts, and retry records are ordinary appends and cannot
  consume closing or result floors.

If a stream reaches the floor, the loop cancels/drops it and truthfully closes
with `AGENT_EVENT_BUDGET`; recorded partial chunks stay out of model history.
If a preferred tool result is too large for its claim, a bounded
`TOOL_OUTPUT_BUDGET_EXCEEDED` result commits and later pairs/closers remain
available.

The proof covers resource admission, not a broken clock, OOM, an invalid event
constructed by Agent code, or process crash. Those are fatal/crash-tail cases.
The retained-payload counter also does not promise that Phase 1's compact
snapshot envelope is below 16 MiB; durable streaming persistence is Phase 8.

## Configurable and hard limits

The upstream in-memory loop has no turn budget and always retry is unbounded.
Rust defaults and hard ceilings are an intentional safety difference:

| Resource | Default | Hard ceiling |
| --- | ---: | ---: |
| steps per turn | 16 | 64 |
| model attempts per turn | 24 | 64 |
| retries per step | 8 | 8 |
| tool calls per step | 16 | 64 |
| tool calls per turn | 64 | 256 |
| effective output tokens per request | 1,000,000 | 1,000,000 |
| reported output tokens per turn | 1,000,000 | 4,000,000 |
| turn duration | 30 minutes | 2 hours |
| one tool duration | 30 seconds | 5 minutes |
| one tool argument string | 256 KiB | 256 KiB |
| one preferred executor result | 256 KiB | 256 KiB |
| preferred executor results per turn | 4 MiB | 4 MiB |

Usage is provider-reported and optional, so the turn token counter is not a
billing meter. The true upper bound also relies on the effective per-request
output cap and attempt count. Small mandatory call/result-correlation fallbacks
are exempt from the two preferred-result limits; the tool-call and exact
Session reservation caps still bound them. Boundary tests cover configured
values and hard ceiling rejection.

Stable limit failures include `AGENT_MAX_STEPS`,
`AGENT_MAX_MODEL_ATTEMPTS`, `AGENT_MAX_RETRIES`,
`AGENT_MAX_TOOL_CALLS`, `AGENT_TOKEN_BUDGET`,
`AGENT_TURN_TIMEOUT`, and `AGENT_EVENT_BUDGET`.

## Intentional differences and deferred behavior

Phase 3 records these differences rather than hiding them:

1. Rust bounds turns, attempts, always retry, tools, time, tokens, events, and
   bytes; upstream's in-memory loop has no equivalent total turn budget.
2. Phase 3 tools are serial. Upstream can run explicitly parallel-safe calls
   in a bounded rolling pool while still committing results in model order.
   A later tool phase may add that optimization without changing event order.
3. Rust does not pass malformed or oversized JSON arguments into a tool. It
   logs a bounded error result; upstream passes invalid non-empty JSON as a raw
   string into its tool pipeline.
4. Duplicate call IDs are rejected before any tool side effect. The upstream
   session invariant uses set-like pending-call tracking and does not define a
   safe two-side-effect result mapping for duplicates.
5. Tool output has fixed byte limits and a durable fallback. Upstream validates
   JSON representability but has no generic result-size cap.
6. A fixed system string and ordered tool schemas replace Cordis system-prompt
   sections for now. Dynamic runtime context, inbox events, pre-step hooks,
   steering, HMR, and observers are not claimed by Phase 3.
7. Panics at provider/tool extension seams become fixed safe infrastructure
   failures rather than allowing an opaque panic payload into the log. These
   are trusted in-process traits: Rust's global panic hook runs before
   `catch_unwind`, so this is not a promise that a hostile implementation's
   panic text is absent from process stderr.
8. Upstream retains a model-visible tool call without a result after an
   executor infrastructure failure, closes the turn, and may accept a later
   explicit follow-up even though a real provider can reject that dangling
   history. Rust retains the same append-only prefix, then refuses more work:
   the same instance returns `Poisoned`, and reconstruction returns
   `UnresolvedToolCall`. This prevents new model/tool side effects until Phase
   8 appends a repair result without rewriting old events.
9. A token already cancelled before entry still produces a balanced aborted
   Rust turn. Upstream checks cancellation before `turn/start` and can produce
   no turn events. The Rust event makes the accepted API call auditable.
10. The official Agent waits without a fixed deadline for all started tool
    tasks to settle after cancellation. Rust polls the same future for at most
    one second and then drops it, because a broken in-process extension must
    not hang the whole CLI indefinitely. Cooperative cleanup and grace expiry
    are both tested; a tool needing longer cleanup must implement its own
    promptly cancellable resource owner.
11. Official retry execution is supplied by a removable Cordis plugin. Rust
    owns provider-policy execution inside `AgentLoop`, so it cannot be disposed
    independently. This removes runtime lifecycle drift but means embedders
    cannot disable retry execution without changing the prepared policy.

These differences do not change the five canonical oracle scenarios. They can
change valid long turns, parallel tools, cancellation cleanup, and integrations
that rely on Cordis/inbox behavior; those impacts are listed above rather than
hidden behind a broad compatibility claim.

## Dependency decisions

Phase 3 adds two small, locked production dependencies:

- `uuid 1.18.1` creates opaque version-4 message and retry identifiers. The
  standard library has no UUID generator, and IDs are never used as security
  tokens. This release declares Rust 1.63 and is MIT/Apache-2.0 licensed.
- `ryu-js 1.0.3` formats retry-policy numbers with JavaScript's observable
  `Number.toString` rules. Ordinary Rust/Serde formatting differs around values
  such as `0.000001`, which would change the durable upstream `policyKey`.
  This release declares Rust 1.71 and is Apache-2.0/BSL-1.0 licensed.

Both are below the repository's Rust 1.85 MSRV and are compiled by the locked
Rust 1.85 verification lane. No async framework, plugin runtime, or tool
registry dependency was added for this phase.

## Verification plan

The Phase 3 TypeScript oracle runs the real pinned Agent Loop with a fake
adapter and fake tool under fixed time and UUIDs. It captures each request at
adapter entry together with the already-committed event prefix, folded header,
request context, and derived messages. Scenarios cover text completion, a tool
round trip, retry in one step, max-token tool suppression, and pre-step
rejection. Default Rust tests consume only the committed fixture.

Rust tests use fake providers, fake tools, fixed IDs/jitter, and paused
time. They cover normal text, single/multiple tools,
multi-step continuation, bad arguments, tool failure/rejection/timeout/panic,
provider prepare/stream/final errors, retry and Retry-After, cancellation at
every boundary, max-token behavior, every configured limit, duplicate calls,
and event/byte reservation edges. They prove that request messages equal the
session projection at dispatch and that every non-fatal path ends with balanced
boundaries.

`ModelProvider`, `ToolExecutor`, and `AgentRuntime` are trusted in-process Rust
extension boundaries, not a sandbox for arbitrary native code. Their contracts
require non-blocking polling, cooperative cancellation, no detached work, and
no unauthorized secret-bearing durable values. Phase 3 tests catch ordinary
panics and persist only fixed failure facts, but a malicious implementation can
still block a runtime thread or write directly to stderr; OS/process isolation
is not claimed.

No Phase 3 default test reads an API key, contacts the internet, accesses user
files, executes a process, or depends on real time or random output.
