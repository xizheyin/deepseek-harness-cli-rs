# Phase 8 persistence, resume, and compaction design

This document defines Phase 8 before production code is added. It is based on
DeepSeek Harness commit
`47f943859bef60e4160492346772ded9b24f765a`. Exact upstream source and test
paths are recorded in `docs/upstream.md`.

The immediate product bug is concrete: a long reasoning stream currently adds
one `assistant/chunk` event for every provider fragment, while the in-memory
`Session` permits only 4,096 events for its entire lifetime. A few long model
responses can therefore end a healthy conversation with
`AGENT_EVENT_BUDGET`. Increasing that number would only delay the same bug and
would make memory consumption larger. Phase 8 instead separates complete
history from the small amount of state needed to continue working now.

In this document, a **journal** is the append-only JSONL file containing every
session fact. A **durability barrier** means waiting until a batch has been
written and synchronized to stable storage before a model request or tool side
effect is allowed to proceed. The **active projection** is the bounded in-memory
fold used for the next request; it is not a second source of truth.

## Scope and non-goals

Phase 8 adds:

- a protected local JSONL journal for every real CLI session;
- `--list-sessions` and `--resume <SESSION_ID>` on the real `dsh` binary;
- exclusive single-writer ownership, durable checkpoints, torn-tail recovery,
  and append-only repair facts;
- streaming history readers so complete history need not remain in memory;
- a bounded active projection whose global event sequence can pass 4,096;
- automatic context compaction before a provider request becomes too large;
- deterministic upstream fixtures and offline Rust tests for persistence,
  recovery, resume, compaction, and long reasoning;
- clean shutdown that flushes the journal after normal completion, Ctrl+C,
  Ctrl+D, `/exit`, terminating signals, and tool cleanup.

Phase 8 does not add:

- cloud synchronization, shared sessions, NFS lock guarantees, encryption at
  rest, or a secrets manager;
- transcript editing, deletion, export, fork, history browsing, or switching
  sessions while a turn is active;
- Zstandard compression, an SQLite database, or a background indexing daemon;
- a manual `/compact` command. Automatic pressure and provider-overflow paths
  are sufficient for this phase; a later command may use the same transaction;
- automatic replay of a tool whose outcome is unknown;
- persistence of API keys, authorization headers, inherited environment, or
  approval challenge strings;
- an unlimited conversation. Disk, records, batches, resident state, provider
  requests, and compaction work all retain explicit limits.

## Upstream behavior and intentional differences

The fixed upstream core keeps an append-only `SessionEvent[]`; its sequence is
the array length and it has no equivalent 4,096-event or 16 MiB lifetime cap.
The persistence coordinator appends continuous sequences to JSONL and the
projection cache may checkpoint and replay a tail. Context compaction replaces
only the model-visible surface. It does not delete old events or raw chunks; a
successful compaction appends four more logical events:

```text
compaction/start
compaction/summary
user/message with surface replace
compaction/end
```

The default upstream bundle also runs a model-free tool-result pruner before a
basic summary when pressure is already high, and unconditionally before
context-overflow range selection. Each changed current tool result appends an
adjacent `compaction/prune` plus replacement `tool/result`; the original full
result remains in the log.

The Rust design preserves those observable facts. In particular, old reasoning
chunks remain available from the journal and global `seq` never restarts.

Rust deliberately differs in these physical and safety details:

1. Rust initially writes plain JSONL with one logical event per line. Upstream
   normally uses Zstandard frames and may pack adjacent chunk rows. Decoding
   produces the same logical events, so physical compression is not part of the
   compatibility claim.
2. The Rust CLI adds an operating-system advisory lock because two separate
   CLI processes are otherwise able to corrupt one append-only file. Upstream
   delegates cross-process single-writer ownership to its host.
3. Rust applies fixed local quotas and strict file permissions. Upstream has no
   equivalent total event or file quota.
4. Rust accepts only its canonical `session-<uuid-v4>` CLI identifier as a path
   key. The provider-neutral in-memory core may still parse other branded IDs,
   but they are not silently turned into filesystem paths.
5. A compaction response containing a tool call, image, no text block whose
   `trim()` is nonempty, or a
   max-token finish is rejected rather than executed or accepted as a partial
   checkpoint. A response containing reasoning plus nonempty text is accepted:
   the checkpoint filters reasoning but preserves every text block, its order,
   and its retained extension fields while the complete normalized raw output
   remains in `compaction/summary`, matching upstream's filtering. The
   exact Rust summarization instruction is product-owned; it keeps the upstream
   sections and checkpoint meaning without claiming prompt byte identity.
6. Rust closes a crash-left `approval/asked` with
   `approval/decided(cancelled)` before repairing its tool result. Upstream's
   normal approval service records a decision, but its cold repair helper does
   not synthesize one after a hard crash. Released Rust transitions prohibit
   closing a step or turn while approval is pending, so the explicit cancelled
   fact is required for deterministic recovery.
7. Durable Rust sessions additionally refuse `step/end` while a model-declared
   tool call has no result. The fixed upstream invariant permits a closed turn
   with such a dangling call and its repair helper leaves it unchanged. Phase 8
   has no older durable Rust format to import, so a closed dangling durable file
   is corruption; the finite Phase 1 in-memory mode retains its existing
   upstream-compatible transition.
8. Rust records the opened workspace directory's device/inode identity as a
   bounded header extension and refuses resume after path replacement. Upstream
   stores `cwd` but delegates retained workspace authority to its host. The
   stricter binding prevents an old approval/history from being replayed in a
   different directory. This is intentionally fail-closed: moving/copying a
   project, or a reboot/remount that changes the platform's device identity,
   may require a new session in Phase 8.
9. Upstream's 80% token-pressure check measures only the already committed
   surface. Rust does the same for compatibility, then separately preflights a
   claimed input against the hard 4,096-message and 8 MiB request-encoder
   limits. If the proposed request cannot be encoded, Rust may compact before
   admitting that input or fail it without appending a partial step. This is a
   safety admission check, not part of the upstream token-pressure number.
10. One Rust replacement may cite at most
    `MAX_SOURCE_EVENT_SEQS - 2` shadowed surface nodes because the existing
    provenance type has a fixed bound and start/summary consume two entries.
    Upstream has no equivalent source-list cap. Rust splits a larger safe range
    across the allowed successful transactions and fails clearly if that still
    cannot produce an encodable request; it never truncates provenance.
11. A failed Rust `compaction/end` stores the existing bounded provider-neutral
    `LlmFailure` code/sanitized message rather than an upstream plugin's raw
    JavaScript error chain. This keeps the close claim auditable and prevents
    opaque payloads or secrets from entering the journal. Users retain the
    actionable failure class/message; comparison tests normalize this envelope
    instead of claiming byte identity.
12. Rust writes the complete, bounded compaction-dispatch recipe in
    `compaction/start` and synchronizes it before sending the auxiliary model
    request. The fixed upstream writes the selected range and several route
    facts only with the later summary. Rust records them earlier because this
    repository's stronger rule is that everything visible to a model must be
    reconstructible after a crash. The recipe includes a serialized snapshot of
    the actual prepared compaction call (effective config including extension
    fields, adapter-default markers, context window, and bounded retry policy),
    the exact bounded system text and tool schemas sent to the summarizer, plus
    the entire canonical final instruction `Message`, including deterministic
    ID and source, rather than only its text. A process-local
    Provider binding is deliberately not serialized and is not model-visible;
    a cold orphan is never resumed as an in-flight request. The extra start
    fields do not alter surface messages; fixtures compare their meaning rather
    than claiming identical event bytes.
13. After a complete bad JSON row or sequence gap, upstream requires a later
    valid `turn/end` before classifying the damage as committed/interior. Rust
    rejects when *any* later complete event envelope is structurally valid.
    Rust barriers can make provider-visible facts and a `tool/call` durable
    inside an open turn, so truncating them merely because no turn end followed
    could erase the only evidence of a side effect. This may reject a suffix
    upstream would discard, but never silently drops a later durable intent.
14. Durable Rust approval facts are tool-pipeline facts, not free-standing UI
    annotations. `approval/asked.callId` must be present and exactly identify
    the currently unresolved `tool/call`, and `toolName` must match that call.
    Upstream's general event type permits an omitted association, but released
    Rust has no non-tool durable approval producer. Live mismatch is rejected;
    a cold missing/mismatched association is corruption rather than evidence
    that an unrelated tool definitely did not run.
15. Durable Rust also requires every top-level `tool/call` to be the exact
    intent previously declared by the current assistant message: call ID, tool
    name, and raw arguments all match, in model order, and each declaration is
    promoted at most once. The upstream core can retain an isolated raw call and
    its repair helper ignores it; the Rust CLI has no legitimate producer for
    that state. Rejecting it prevents a model-visible declaration and an
    externally executed intent from silently disagreeing.
16. The fixed upstream tool-result pruner walks one synchronous surface
    snapshot and has no inner cancellation check. Rust reads potentially large
    source rows through its single journal owner, so it checks cancellation
    before the pass and between complete candidates. It also checks an owned
    cancellation token every 64 KiB while reading one candidate. Once a
    marker/replacement pair begins, Rust does not split it with cancellation.
    A cancellation after one pair may therefore leave fewer complete pairs
    than upstream, but never a cancellation-created marker-only row; a later
    uncancelled pass can prune the remaining current results. This difference
    avoids starting new disk work after a terminal user stop while preserving
    append-only recovery.

These differences will be recorded and tested in `docs/compatibility.md` before
Phase 8 is marked complete.

## Three-layer ownership model

```text
append-only JSONL journal (complete facts, global seq)
                         |
                         v
bounded active projection (surface, boundaries, small indexes)
                         |
                         v
Agent / Provider request / committed-event terminal observer
```

### Complete journal

The journal owns the immutable `SessionHeader`, every logical event, the next
global sequence, the durable byte offset, and one held writer lock. It is the
only complete history. An event is never removed or rewritten. A streaming
reader can scan a range without constructing one giant `Vec<SessionEvent>`.

`next_seq` is explicit. It must never again be inferred from a resident vector
length. The journal rejects a duplicate or skipped sequence and stops before
JavaScript's exact-integer maximum.

### Active projection

The active projection owns only what is needed to validate the next append and
construct the next model request:

- current turn/step and unresolved calls;
- pending approvals;
- latest request header and route context;
- current model-visible surface nodes, with each node's message, sequence,
  estimated token price, and tool-pairing facts. A current `tool/result` also
  keeps its journal row locator, target-node revision/token, a SHA-256 digest of the full
  raw row, and a second SHA-256 fingerprint of every immutable raw event/data
  field other than prunable text content;
- current-turn retry schedules plus exact durable-lifetime approval and retry
  ownership indexes;
- an unmatched live compaction bracket;
- the small pending journal batch and current provider-attempt source seqs.

The current Agent-private `AssistantAssembler` is extracted into the
provider-neutral `session::attempt_anchor` owner; Session never depends back on
Agent. The hot `AttemptAnchor` retains at most 10 MiB of aggregate encoded
provider-neutral chunk payload while one attempt is open. The separate 32 MiB
resident-attempt credit must cover that fold plus pending journal ownership;
the 10 MiB semantic counter alone is not a physical-memory proof. Immediately
before each ordinary Provider dispatch, the reservation
creates an opaque `AttemptToken`; every committed chunk for that request must
present that token, feeds exactly one anchor, and advances its source span.
Before max-token/error policy can transform a terminal response into the final
durable `Message`, the reservation seals the anchor and *moves*, rather than
clones, its owned raw provider output into `PreparedAttempt { token, raw }`.
Session retains only a compact token entry containing span/source/usage/raw-
output and allowed-normalization digests. Agent applies the shared max-token/
error policy to `raw` and presents the same token to the event that closes the
attempt.

The closed disposition is explicit and exhaustive:
`AttemptDisposition::{Committed, Retry, ContextOverflow, Failed, Cancelled,
Interrupted}`.
`assistant/message` consumes `Committed`; `llm/retry` consumes `Retry`; a live
terminal `step/end` after stream/protocol/policy failure or cancellation
consumes `Failed` or `Cancelled`; cold repair consumes `Interrupted` with its
synthetic `step/end`. A committed disposition must exactly validate the final
message and source span against the sealed raw anchor. Every other disposition
discards the partial/raw assembler and creates no assistant surface node or
token-meter assistant anchor. Usage chunks are different: they update the
bounded cumulative usage projection when each chunk commits, so closing an
unfinished attempt preserves those durable usage totals without pretending a
final assistant message exists. `Failed`, `Cancelled`, and `Interrupted` have
the same durable fold; their live tag explains which owner requested closure,
but `step/end` does not serialize that tag, so cold replay normalizes any
already closed noncommitted attempt to the same fold and takes the user/error
distinction from the following `turn/end`. Only after the closing event's
barrier does `retire_attempt(token)` erase the compact entry. If a request
emitted no chunk, the same token still closes normally but owns no assembler.

Context-overflow replay is deliberately not an ordinary network retry and does
not append `llm/retry`. When an overflowed attempt is open, the first durable
compaction fact closes it as `ContextOverflow`: the first
`compaction/prune` marker in the model-free pass, or, if pruning emitted
nothing, `compaction/start { trigger: context-overflow }`. The specialized pair
or start append consumes the token atomically, then its barrier permits
retirement before any summary call or replay begins. Cold scan recognizes the
same two boundaries. DurableStrict permits this disposition only for the same
turn/step as an already sealed terminal failure whose code is exactly
`CONTEXT_WINDOW_EXCEEDED`; a different failure, an unfinished stream, or a
cross-step token is rejected. Pressure/hard-limit compaction runs before an ordinary
attempt and therefore rejects an open token; if overflow produces no prune or
start fact, it cannot replay and the terminal `step/end` closes the attempt as
Failed. This keeps a prune-only replay's later chunks out of the old assembler
without inventing retry events, delay, or retry-counter use.

The Session, not only the Agent stack frame, owns the identity of the one open
attempt. Normal assistant/retry code presents its token explicitly, while the
outer step owner always settles its reserved `step/end` through
`settle_step_end_with_attempt_settled`. That method atomically consumes any
still-open token for the same turn/step using the latched Failed/Cancelled
disposition, or closes an already clean step when no token remains. Therefore a
panic caught around `run_step`, including one after the raw fold moved into
Agent code, cannot lose the only handle needed to close and later retire the
attempt. It cannot consume a token belonging to another step.

Cold scan instantiates the same fold and normalization code from journal chunks.
An EOF with an open attempt feeds an `Interrupted` close into the cloned repair
fold before synthetic `step/end`; it never asks a normal live `step/end` rule to
silently drop the anchor. A batch already flushed to disk does not discard the
hot anchor, and the final normalized message is not mistakenly used as the
provider-attempt anchor. For `max-tokens`, the pinned upstream
`BlockAssembler` removes tool-call blocks before this provider-assistant price
is computed; Rust must not charge the discarded raw tool calls as though the
fixed upstream retained them. At no point do Agent and ValidationIndex each
own a 10 MiB fold.

The provider-neutral stream grammar needed by both hot Agent dispatch and cold
Session scan moves from `provider::stream` to `model::stream`; `provider`
re-exports the existing public names to avoid needless caller churn. Session
does not import `agent` or a concrete DeepSeek type.

Large immutable model values use one explicit sharing boundary before durable
mode lands: `Message`, `ToolSchema`, and `LlmCallConfig` become internally
`Arc`-backed immutable values while keeping their public constructors, getters,
serialization, and semantic equality. Their `Clone` copies only the handle; it
never recursively clones `ContentBlock`/`JsonValue` trees. Agent system text and
the ordered tool-schema collection are likewise stored behind immutable shared
owners. Surface nodes, `TurnOutcome`, request construction, and compaction
selection therefore share the same message allocation. The physical allocation
charge is computed when the immutable inner is created. Concretely, the
model-facing wrapper has the ownership shape `Message { inner:
Arc<MessageInner>, resident_lease: Option<Arc<ResidentCreditLease>> }`; the
lease is not stored inside the shareable semantic inner. Durable admission
reserves the charge and returns a wrapper carrying a crate-private,
session-local lease. Every surface/outcome/request clone of that wrapper shares
both handles, and the last lease drop releases the credit. Memory-mode messages
carry `None`. Reusing one semantic inner in another durable node or Session
creates a new wrapper with another conservative lease, so cross-session aliasing
cannot undercharge memory. Extra handles have their own small vector/handle
credits but cannot duplicate the payload after commit.

Surface nodes own their shared `Message`; they do not use a global sequence as
an index into an all-history vector. Raw `assistant/chunk` bodies are written to
the journal and sent to the live observer, but they leave resident state after
the attempt is durably anchored by `assistant/message`, retry/error closure, or
cancellation closure. This retirement happens after every attempt, including
between tool steps in one turn.

Every handle vector uses `try_reserve` before it becomes observable. A
`ProviderRequest` or `TurnOutcome` is assembled from shallow handles, and the
bounded wire encoder has a separately reserved scratch budget. Phase 8 replaces
DeepSeek's current `serde_json::Value` request tree followed by `to_vec`, which
duplicates the complete request. It first validates and fallibly reserves one
bounded 8 MiB flattening pool for the few wire fields that must concatenate
multiple blocks, then serializes a borrowing request view directly into a
second `Vec<u8>` whose full 8 MiB capacity was obtained with `try_reserve`.
Existing raw config/schema values are borrowed, not copied into a new `Value`
tree; a limit writer refuses the first byte over the wire cap. Thus the encoder
has at most 16 MiB of charged text/wire scratch plus the separately charged
handle vectors, and allocation failure is an ordinary pre-dispatch error. There
is no derived deep clone after a Session event commits. Any operation that
really needs a new raw tree (for example the pruner's replacement) builds it in
its already charged fallible scratch path before installing the new node.

Provider-neutral `ProviderRequest::retained_bytes` remains a resident-data
bound; it is not misused as an exact wire bound because JSON escaping can expand
control-heavy text substantially. `ModelProvider` therefore owns a synchronous,
network-free `preflight_request(ProviderRequestDraft)` seam. It receives borrowed
system/tools/virtual-surface/session/purpose facts plus the proposed config and
returns either a `PreparedRequestPreflight { prepared_call, encoded_bytes }`, a
wire-too-large result, or the same bounded preparation failure the ordinary
attempt would have produced. DeepSeek factors one pure config resolver and one
encoder traversal between this seam and dispatch; preflight uses a counting/
limit sink, while the real stream creates the actual bytes exactly once. It
does not read credentials, open a socket, consume an ID/clock, or publish a
Session fact. Fake providers must implement the same explicit contract rather
than inheriting a falsely generic byte estimate.
For every committed surface-message event, `AppendReceipt` carries
`committed_message: Some(Message)` as the shallow leased handle; every other
event carries `None`. Agent uses that exact handle for `TurnOutcome` rather than
retaining an unleased pre-append copy. The lease is crate-private and excluded
from serialization and semantic equality, so sharing changes ownership and
accounting without changing the event or provider wire shape.

Approval and retry IDs remain globally unique facts even after their events
leave resident history. Durable mode therefore retains exact ownership sets;
it does not use a lossy cache or silently forget an old ID. The combined sets
admit at most 65,536 IDs, 16 MiB of copied UTF-8 ID bytes, and 32 MiB of charged
allocation (entry storage plus actual string capacity). The scanner rebuilds
the same sets on resume. Crossing any bound is `CLI_SESSION_LIMIT` before the
new event is appended, and cross-turn or post-resume reuse is still rejected.

Durable validation applies the Agent's existing model-call identity bounds at
the Session boundary, not merely in the live Agent. Every assistant tool-call
ID, top-level `tool/call` ID, tool-result source/block call ID, and approval call
ID is nonempty, at most 1,024 UTF-8 bytes, and contains no control character;
every corresponding tool name is nonempty, at most 256 UTF-8 bytes, and contains
no control character. Approval request IDs, which a synthetic decision repeats,
use the existing same 1,024-byte/non-control bound. Synthetic recovery message
IDs are deterministic bounded values: a domain-separated SHA-256 over the
session ID, last pre-repair real-event seq, call ID/call seq, and assigned
synthetic result seq, rendered with a fixed ASCII prefix and lower-case hex.
They use no process-local entropy or recovery clock, so a new process can
recognize an already-written repair prefix exactly.
One durable step may declare at most 64 model tool calls,
and durable state may contain at most one unmatched approval request. Live
durable append rejects a second pending approval, a 65th declared call, or an
identity violation before consuming sequence/time; the cold scanner reports an
existing violating row as corruption. These limits make the permanent maximal
repair templates real even for a manually edited journal or another caller of
the Session API.

The current-step fold retains at most 64 compact declared-call entries:
`(id, name, arguments, top_level_seen, approval_state, result_seen)`. A durable
assistant message must declare step-local unique call IDs; duplicate IDs inside
one message or across assistant declarations in the same step are rejected
before the message commits and are cold corruption. A durable
top-level `tool/call` must promote the next unpromoted assistant declaration in
model order and match its ID, name, and arguments exactly; isolated, reordered,
duplicate, or mismatched intent is rejected. Durable `approval/asked`
additionally requires `callId=Some(current_call.id)` and an exact `toolName`
match to that promoted unresolved call, and each call may be asked at most once
even after its first decision. The same checks run in the live append delta and
cold `ValidationIndex`; missing, stale, cross-call, or wrong-tool associations
are rejected before they can affect repair classification. A later
`approval/decided` is paired through that already validated request ID.

No `tool/result` may resolve a call while its approval is pending. After an
allowed-once decision, or when no approval was asked, the normal correlated
result is admitted. After rejected, cancelled, or unavailable, only the exact
canonical `isError=true` `APPROVAL_REJECTED`, `APPROVAL_CANCELLED`, or
`APPROVAL_UNAVAILABLE` result for that call is legal; a success or mismatched
failure is corruption. These rules make “unmatched asked means body was not
dispatched” a property of the durable format rather than an assumption about
the live Agent.

Durable validation also enforces one attempt's existing provider envelope:
at most 4,000 stream chunks and 10 MiB across the compact-JSON encodings of the
provider-neutral chunks, with every chunk/final source in the same turn/step
and a final source span of at most
4,096 unique sequences. Live append rejects one-over before growing the anchor;
cold scan treats an artifact that could not have come through this durable
producer contract as corrupt. This is what bounds the hot and cold
`AttemptAnchor` even for an interrupted final attempt.

Before an attempt's chunks retire, the projection reconstructs the exact
assistant content named by the final message's source sequences and records the
same token-meter anchor that cold replay would derive. Hot execution and cold
resume must therefore choose the same pressure and compaction range even though
the raw chunks are no longer resident.

The Phase 1 `Session::new`, snapshot codec, and deterministic unit tests remain
an explicitly finite in-memory mode with the existing 4,096/16 MiB limits.
`SessionStore::prepare_new` creates a deferred durable Session and
`Session::materialize_if_needed` activates its journal on first use;
`begin_resume` followed by its owned wait/finish always creates the already-
active durable mode used by the real CLI. The public complete-history accessor changes to a fallible
`Session::events()` that succeeds only in memory mode and directs durable
callers to the bounded range reader; `to_json()` has the same mode check.
Resident-tail introspection, where needed internally, is named explicitly and
never presented as complete history. This source-level change is acceptable in
the pre-release Rust API and prevents a durable tail slice from silently
masquerading as the journal.

Append validation no longer receives an all-history slice. A bounded
`ValidationIndex` owns the active surface messages, the current attempt's chunk
sequence/type/turn/step facts, unresolved call sequences, pending approval and
retry ownership, and any live compaction claim. Final assistant validation
occurs before its attempt index is retired; tool results resolve a retained
current-step call entry; replacements resolve owned surface nodes. The recovery
scanner rebuilds the same index sequentially. An internal, closed
`ValidationPolicy` makes the compatibility boundary explicit:
`MemoryCompatible` preserves the released Phase 1 in-memory transition rules,
while `DurableStrict` enables the recoverability rules below for deferred,
active, and cold-scanned journals. Callers cannot select a weaker policy for a
durable Session. `Projection::with_event`, Agent unresolved-call checks, script
final-output selection, and CLI resource-endpoint classification all use this
bounded state or `TurnOutcome`, never `seq -> events[seq]`.

`TurnOutcome` becomes the owned summary of one completed durable turn. In
addition to counters and the exact `TurnEndReason`, it carries the committed
`turn/end` sequence and an optional owned copy of the latest assistant message
in that turn containing nonempty text. The Agent updates that field only when
the corresponding assistant event commits. Script output therefore needs no
complete-history scan, and interactive code verifies the observer's end event
against the outcome sequence/reason rather than searching a resident event
vector.

A tool-result surface node does not retain up to 9 MiB of opaque metadata for
the lifetime of the surface. Before pruning, an async bounded row read uses the
node's `JournalLocator { offset, length, seq }`, verifies that this exact seq is
still a current node with the same node revision/token and full-row digest,
requires the decoded complete raw message to be
semantically equal to the active node's message, and recomputes its masked
SHA-256 identity fingerprint. It then constructs the replacement directly from
the raw JSON by changing only text content. Unknown
extension fields, error/meta data, message identity, and rich blocks therefore
survive exactly. A locator/fingerprint mismatch is corruption and commits no
marker. The cold scanner builds the same locator and fingerprint, while the
pruner holds at most one 9 MiB source-row scratch buffer. This read is complete
before the adjacent pair's synchronous critical section begins.
The pruner does not compare a global surface generation for each candidate:
replacing an earlier, unrelated result must not stale later candidates from the
same surface-order pass. Basic summary, whose whole selected range crosses an
async model call, still revalidates the whole-surface generation.

This path uses a crate-private `ValidatedRawReplacement`, not the ordinary
typed `EventKind::ToolResult` serializer, because that serializer cannot retain
unknown extension fields inside the old event's `data`. The constructor accepts
only the bounded raw row read above, decodes and validates its known shape,
masks/rechecks immutable identity, and changes only the nested prunable text
inside a copied `data` object. Session owns and freshly encodes the replacement
envelope: type, new seq/time, `surfaceOp=replace`, and singleton
`sourceEventSeqs` are canonical new values. Unknown event-envelope fields are
not inherited, and any raw value that tries to supply or conflict with those
Session-owned fields is rejected. Unknown fields inside `data`, including
message/error/meta extensions, survive unchanged. The constructor returns this
validated data plus its typed projection delta; there is no public arbitrary-raw
append escape hatch.

### UI observation above global sequence 4,096

The live observer remains post-commit and non-blocking. On resume it attaches at
the current global `next_seq` and receives only events created by this process;
historical events are not replayed as live output.

Final-answer deduplication no longer treats a global sequence as a fixed bitmap
index. `SourceSeqBitmap` stores a base sequence plus 64 words and maps at most a
4,096-sequence attempt span relative to that base. A source outside the span is
a projection fault after the Session commit, never a reason to roll history
back. The provider already limits one stream to 4,000 chunks; tests cover a
session whose global base is far above 4,096.

Item capacity alone is not a memory bound. The observer therefore also owns
RAII byte credits: at most 16 MiB of copied strings/bitmap payload and 32 MiB of
charged envelope/vector/string allocation across all queued or currently
rendered events. A producer reserves a conservative charge before copying; the
credit travels inside the event and is released when the receiver finishes or
drops it. Failure to reserve, projection allocation failure, or the existing
4,096-item Full condition occurs after the Session commit, sets the observer
fault, detaches the sender, and makes the CLI cancel/await the turn and exit
rather than grow memory or continue approving work without a truthful UI.
The owned `AppendReceipt` exposes the post-commit poison bit so the Agent latches
stop immediately. In particular, a fault while publishing `approval/asked`
causes a durable `approval/decided(unavailable)` and `APPROVAL_UNAVAILABLE`
result without calling the
approval provider. The Session also retains this condition as a sticky observer poison. Every
mandatory pre-Provider and pre-tool durability barrier checks it again after
successful storage sync and immediately before dispatch; a poisoned observer
returns the stable Agent-unavailable stop instead of sending a request,
accepting an approval, or running a tool. CLI polling still owns presentation
and cleanup, but safety does not depend on the CLI winning a scheduler race.

## On-disk layout and path safety

The default root is:

```text
macOS: $HOME/Library/Application Support/dsh/sessions
Linux: $XDG_STATE_HOME/dsh/sessions
       or $HOME/.local/state/dsh/sessions
```

`DSH_SESSION_ROOT` is an explicit test/operator override and must be absolute.
There is no fallback to the workspace, current directory, or `/tmp`. A missing
or unusable path-policy input (for example no absolute usable `HOME` when the
Linux default is needed) is `CLI_SESSION_ROOT_UNAVAILABLE`; an absent directory
on an otherwise valid path is created lazily by the bootstrap below.
For a new session, opening the store only resolves/validates this policy; it
does not create the directory until first materialization. Listing treats an
absent valid root as an empty store.

First-use root bootstrap is itself durable and capability-relative. Path policy
opens the configured/default state base by walking existing components with
`openat`-style directory descriptors and `NOFOLLOW`; it rejects a symlink,
non-directory, unexpected owner, or unsafe writable ancestor rather than using
`create_dir_all` on a path string. Any missing product-owned suffix component
is made with `mkdirat` at mode `0700`. Because POSIX umask may clear even owner
bits, creation never assumes that mode survived, and it never uses a
symlink-following `chmodat` to repair it. On Linux, path policy first captures
the component with `openat(O_PATH|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)` and verifies
its identity/owner/type, then a tiny isolated `unsafe` wrapper invokes the
Ubuntu 24.04 `fchmodat2(path_fd, "", 0700, AT_EMPTY_PATH)` operation on that
exact object. `ENOSYS`/unsupported flags fail closed; there is no fallback that
follows a path. On macOS, the retained trusted parent uses
`chmodat(..., SYMLINK_NOFOLLOW)`, which the platform supports, followed by an
identity check. Both paths then reopen with
`NOFOLLOW|DIRECTORY|CLOEXEC`, require the same identity/type/effective uid,
apply `fchmod(0700)` on the retained ordinary directory fd, and recheck exact
mode. A concurrent `EEXIST` follows the same path; an
effective-uid-owned directory whose mode is only a bit-cleared subset of
`0700` may be normalized to finish a crashed creator, while a symlink, broader
unsafe mode, identity change, or unexpected owner fails closed.
The Linux syscall wrapper is the only new unsafe path-policy primitive: it is
`cfg(target_os = "linux")`, accepts an already-owned fd, passes a fixed
NUL-terminated empty path and integer mode/flags, retains no pointer, checks the
return/`errno`, and is covered by real Ubuntu tests. All surrounding traversal,
identity, and ownership logic remains safe Rust.
For every required product-owned suffix, the retained directory itself is
first synchronized with its chmod metadata and its containing directory is then
synchronized, whether this process just created it or converged through
`EEXIST`; only then may bootstrap descend to the
next component. This closes the case where creator A crashes between mkdir and
parent sync while creator B observes the unsynchronized entry. The completed
sessions root is also synchronized before its store lock is taken. Thus the
directory entry chain and the header are reopenable after a reported pre-effect
barrier. The path capability, not a second string lookup, owns the later journal
creation. Failure to open, create, validate, or sync any component aborts before
`CREATE|EXCL` for a journal; a component that another process swaps or replaces
is detected by the retained descriptor/identity checks and fails closed.

Existing path ancestors must be owned by root or the effective uid and must not
be group/other-writable, except for a root-owned sticky directory such as an
explicit-override test parent. The selected existing state base itself must be
owned by the effective uid and not group/other-writable. Missing components
below the last trusted existing ancestor are created privately as above; the
product-owned `dsh` and `sessions` directories always require exact `0700` even
when they already existed. This permits ordinary owner-readable home/state
parents without weakening the private store root.

Every phrase "synchronize the file/directory/parent" below means the same
fallible `sync_durable(fd)` primitive: Linux uses `fsync`; macOS uses
`rustix::fs::fcntl_fullfsync` for both regular-file and directory descriptors.
No macOS namespace claim is based on ordinary `fsync` alone. This primitive is
used on each newly normalized directory and then its parent, after journal
`CREATE|EXCL`/header write, after conditional unlink, after truncate rollback,
and at every journal barrier. Any unsupported/error result is a storage failure,
not a silently weaker durability mode.

Each CLI ID must first match the literal `session-` prefix. Only its suffix is
parsed as an RFC 4122 UUID with variant RFC 4122 and version 4, then the complete
identifier is re-rendered as `session-<lower-case-uuid>` before any path lookup.
A bare UUID, doubled prefix, non-v4 UUID, malformed suffix, or noncanonical
upper-case spelling is a usage error. The first format uses one flat file per
session; there is no separately managed PID or lock file:

```text
<root>/
  session-550e8400-e29b-41d4-a716-446655440000.jsonl
```

The ID never contains a slash, `..`, NUL, or an arbitrary user string. Directory
names, header IDs, and the requested ID must agree exactly.

The store root is exactly mode `0700`; journals are exactly mode `0600`, set
explicitly on retained descriptors rather than inherited from the umask. A new
journal's `CREATE|EXCL` fd is immediately `fchmod(0600)` and revalidated before
any header byte is written, then the file and parent are synchronized in that
order. Existing objects must be owned by the current effective uid and match
those exact permission bits. The
implementation refuses symlink components, symlink files, non-regular journals,
or a journal with link count other than one. Opens below the retained root
capability use relative paths, `NOFOLLOW`, `CLOEXEC`, `NONBLOCK`, and post-open
identity checks. A newly published file is created with no-clobber semantics and
both file and parent directory are synchronized.

New-session materialization is lazy like the fixed upstream artifact: choosing a
fallible session ID and showing the first banner does not create a file. The
first `AgentLoop::run_turn` performs the complete proposal, message-shape, and
intrinsic per-input limit preflight, then calls
`Session::materialize_if_needed` before its
first input claim or `turn/start`; only then may the Agent append. An
immediate `/exit`, EOF, terminating signal, or startup failure therefore leaves
no empty journal and consumes no canonical slot. Once the first event is
admitted, the header and every fact follow the ordinary durable rules.
Here, "invalid before materialization" means input the CLI rejects without ever
calling `AgentLoop::run_turn`. Every well-formed `TurnProposal` passed to the
Agent, including `Reject` and `Enter([])`, materializes first and preserves the
existing paired `turn/start`/`turn/end` blocked-or-completed facts; lazy creation
does not weaken the Phase 3 empty/rejected-turn invariant.
Cross-session surface/message-count encoding checks are intentionally later:
they require the durable active projection and follow the materialize →
turn/start → claimed-input virtual-preflight order defined under context
measurement.
The pending Session is still fresh and may attach its observer, but append or
reservation entry reports `NeedsMaterialization` with no sequence/time/state
change. Durable Session state is explicit:
`DeferredNew(NewJournalPlan) | Active(Journal)`; resume is always `Active`, and
shutdown of `DeferredNew` is a no-op. Once the materialization command is
enqueued its ownership is stored in the Session and signal cleanup settles it;
dropping an outer future cannot detach a root/journal lock or writer.
Materialization failure is a stable Session-store failure and never falls back
to an in-memory conversation.

To make the 128-journal store cap real under concurrent first materialization,
`materialize`
opens a fresh descriptor for the retained store directory and takes a short
exclusive `flock`. While holding it, creation performs bounded enumeration,
counts every canonical filename slot, uses `CREATE|EXCL` for the final name,
then takes the new journal's own **non-blocking** exclusive lock before writing
any header byte. A surprising busy result never waits while holding the root
lock, never writes a header, and never unlinks an object it does not own under
the journal lock; it releases the root lock, leaves the private canonical name
as a counted crash-style residue, and reports `CLI_SESSION_BUSY`. After a
successful journal lock, materialization writes and fully synchronizes the
header and synchronizes the parent directory.
Only then does it release the root lock; the journal lock transfers to the
writer and remains held. The final filename is namespace-visible after
`CREATE|EXCL`, not falsely described as atomically published; listing ignores it
until the complete valid header exists. A second creator retries the root lock
only within a small fixed startup deadline and otherwise gets
`CLI_SESSION_STORE_BUSY`.

If setup fails, cleanup may unlink the new path only while it still holds that
journal lock and a fresh path `stat` matches the originally created device and
inode; it then synchronizes the parent directory. A crash can leave an empty or
torn private slot, which remains counted. Resume still locks only its journal,
and read-only listing needs no catalog lock because it reads bounded headers and
sorts a collected bounded result.

The workspace's canonical absolute path is stored in the header. Because JSON
stores it as a string, a canonical path that is not valid UTF-8 is rejected as
`CLI_WORKSPACE_UNAVAILABLE` before a journal is created. Resume without
`--workspace` reopens the stored path. An explicit workspace is an assertion:
its opened directory identity must match the stored workspace, so history cannot
be moved to a different project accidentally. A missing or different workspace
is rejected before tail truncation, repair, Provider construction, or network
access.

Path text is never reopened after that check. A shared crate-private
`WorkspaceAuthority` owns the already-open capability directory, canonical and
startup display paths, and device/inode identity. New mode opens it once before
forming the header. Resume preparation opens and verifies it once, then retains
the same authority across the warning gate and repair commit. The recovered
owner returns both `Session` and `WorkspaceAuthority`, and
`LocalToolRegistry::from_authority` consumes/clones that capability rather than
calling its old path-based `open` again. This shared type lives outside
`session` and `tools`, so neither layer depends backwards on the other. A path
rename/replacement after authority acquisition may change the namespace, but it
cannot redirect the held directory or later tool access to a different object.

## JSONL format

The first complete line is a tagged header:

```json
{"type":"session","version":0,"id":"session-...","createdAt":0,"cwd":"/absolute/workspace","delegationDepth":0,"rustWorkspaceIdentity":{"device":"1a","inode":"2b"}}
```

It is followed by existing event envelopes, one compact JSON object and one LF
per logical event:

```json
{"type":"turn/start","seq":0,"time":1,"data":{"turn":1}}
```

The Rust v0 reader accepts exactly one logical Session event per physical line.
An upstream Zstandard/storage `*-chunks` packed row is a foreign physical format
and is refused without expansion or mutation; Phase 8 does not import an
upstream artifact. The oracle decodes packed upstream rows only to compare their
logical facts. A Rust `EventReader<'_>` privately owns the continuation
`{ session_identity, durable_revision, snapshot_end, physical_offset,
next_seq }` at a line boundary; it never needs an unbounded logical sub-index or
accepts a forged public cursor. Creating the reader takes `&mut Session`, settles
the pending/flight state, performs a barrier, and fixes the current
`durable_offset/next_seq` as its immutable snapshot end. The first page streams
to the requested sequence; subsequent pages continue from the retained offset,
each respecting the page event/byte caps. The mutable borrow prevents local
append while it is open. Dropping and reopening a reader after an append creates
a new revision; no page reads an unbarriered row that rollback may remove.

The header carries the version and session identity once. A top-level CLI
session writes `delegationDepth: 0`; resume validates it as a nonnegative safe
integer. The Rust workspace device and inode are lower-case hexadecimal strings,
avoiding JSON integer rounding; resume compares them with the newly opened
stored path before any network or tool construction. Event records do not
repeat these fields. Known extension fields remain lossless. Unknown required
events make resume unsupported; unknown events with exactly `ignorable:true`
are preserved and skipped by semantic folds. The Phase 1 `{header, events}`
snapshot is not auto-detected as a durable artifact.

Current format version remains `0`. A reader peeks at `version` before validating
the rest of a foreign header so a newer file reports `CLI_SESSION_UNSUPPORTED`,
not a misleading corruption error. Fixed v0 legacy transformations happen only
while reading and never rewrite old bytes; newly appended records use the
current v0 shape. Retired request-header formats remain rejected as documented
by the upstream fixture.

Before taking a store slot or calling `CREATE|EXCL`, materialization encodes the
complete tagged header plus its terminating LF and proves that line is at most
64 KiB. This is deliberately stricter than the existing 64 KiB bound on the
in-memory header's raw JSON, because the durable wrapper fields and LF also need
to fit listing's fixed candidate-header buffer.

## Resource limits

All numbers below are product policy, not upstream compatibility facts:

| Resource | Limit | Behavior at the boundary |
| --- | ---: | --- |
| Tagged header JSONL line | 64 KiB including its LF | reject before `CREATE|EXCL`; the in-memory raw-header cap remains separate |
| One JSONL record | 9 MiB | reject the event before append |
| Pending write batch | 256 KiB or 256 events; one large record may stand alone | enqueue and await the owned writer before admitting more |
| Compaction body critical section | committed summary/replacement/end rows, at most 19 MiB | reserve once, commit sequentially, send as one owned writer command |
| Compaction close claim | 1 event / 64 KiB from before start until close | guarantees a bounded success/error end after start; OS failure may still orphan |
| One prune pair | marker + replacement, at most 10 MiB | dedicated large command; reserve before marker and commit the two rows synchronously and sequentially |
| Pruner source-row scratch | one row, at most 9 MiB | bounded async positional read; dropped around each candidate |
| Rendered recovery warning | 256 KiB | prepare fails before mutation rather than truncate the warning |
| One journal read page | 256 events or 9 MiB; one legal large record may stand alone | return a continuation cursor instead of retaining more |
| One provider stream | existing 4,000 chunks / 10 MiB emitted | existing provider error and truthful closure |
| Resident attempt + pending journal data | 32 MiB | stop the attempt, retain closure reserve |
| Shared fixed Provider prefix | existing 4 MiB encoded, 16 MiB charged allocation | reject during Agent construction; system/config/tools are immutable shared owners |
| Provider request handles + encoder scratch | 1 MiB charged handles, 8 MiB flattening pool, and 8 MiB encoded wire | fallible reserve before dispatch; serialize a borrowing view once, with no complete `Value` tree |
| Committed UI queue | 4,096 items, 16 MiB copied payload, 32 MiB charged allocation | post-commit observer fault, cancel/await, no further side effect |
| Model-visible surface | 4,096 messages, 24 MiB encoded message bytes, 64 MiB steady charged allocation; the Provider request remains 8 MiB | compact/preflight before dispatch; reject a durable delta before state change or report a cold limit |
| Approval/retry ownership | 65,536 IDs, 16 MiB UTF-8 payload, 32 MiB charged allocation | reject the new identity exactly; never forget an old owner |
| Complete `ValidationIndex` | 96 MiB steady charged allocation including surface and identity sets | reject exact one-over; every internal vector/string capacity is charged |
| One cold-validation clone | one additional 96 MiB, 192 MiB total index high-water | clone only bounded fold state once; never clone journal history |
| One journal | 512 MiB and 1,000,000 logical events | `CLI_SESSION_LIMIT`; emergency closure reservation remains usable |
| Canonical session-name slots | 128, including zero/torn headers | new-session creation fails; existing valid sessions remain listable/resumable |
| One store enumeration | 256 directory entries / 128 canonical slots, 64 KiB per candidate header | `CLI_SESSION_LIMIT`; bounded output and no journal-body scan |
| Compaction transactions | pressure/hard-limit: at most 2 successful transactions per check; overflow: 1 | prevent a compaction loop; a failed summary is not retried |
| Summary output | 8,192 tokens, existing provider byte/chunk limits | incomplete, image/tool-call, max-token, or no-trimmed-text summary fails closed |

The 8 MiB Provider-request limit is a dispatch encoding limit, not a sufficient
resident-memory proof. The 24 MiB steady surface bound deliberately covers one
otherwise legal pre-dispatch surface, one maximum normalized assistant message,
and the turn's bounded tool-result additions before the next compaction point.
Its charged-allocation counter includes message/container storage and actual
string/vector capacities, not only compact JSON bytes. The 64 MiB surface charge
plus the 32 MiB identity/other-index charge forms the 96 MiB
`ValidationIndex` cap. Building a replacement charges both old and candidate
surface until the atomic swap, but the combined index plus candidate scratch
must remain within the explicit 192 MiB transient high-water; a failed
preflight leaves the old surface authoritative.

The durable Agent reserves worst-case assistant-projection headroom before a
Provider dispatch and tool-result projection headroom before
`ToolExecutor::prepare` or any later effect. Canonical output can therefore be
recorded and closed without discovering a surface-cap failure after an external
effect. A direct durable append that would cross either encoded or allocation
bound is rejected before clock/sequence/state change. A cold scanner charges
the same fields incrementally and returns `CLI_SESSION_LIMIT` with zero mutation
at exact one-over, even when every individual row is otherwise legal. This
prevents a near-512 MiB hand-built journal from forcing its current surface into
resident memory.

Every canonical `session-<uuid>.jsonl` directory entry consumes one of the 128
slots even when a crash left it empty or with a torn header. Only entries with a
complete valid header appear in `--list-sessions`; counting invalid canonical
slots prevents repeated failed creation from bypassing the store cap. The
theoretical bound for journals admitted by `dsh` is therefore 64 GiB. Any
noncanonical entry still counts toward the 256-entry scan bound and can make
enumeration fail closed, but is never opened as a session. The CLI never deletes
old sessions automatically. Reaching the limit produces a clear error and
leaves existing journals readable; a future explicit deletion command is out
of scope.

The journal has hard caps of 512 MiB and 1,000,000 events. Ordinary admission
stops at 511 MiB and 999,932 events, leaving a permanent 1 MiB / 68-event repair
gap. That gap covers one result for every call in the maximum 64-call open step,
one pending approval decision, balanced step/turn closure, and
`session/end-seed`, even after a crash has erased all in-memory claims. Tests
serialize the maximal templates using 1,024-byte call IDs and 256-byte tool names
and fail if they no longer fit the stated gap. The durable validator/scanner
enforces those same identity limits before relying on this proof.
While a live turn exists, its exact `EventClaim` bytes and events are reserved
*in addition* to that permanent repair gap; this covers a large real tool result
that must be recorded after an irreversible side effect. Normal append admission
proves:

```text
durable accepted bytes/events
    + acknowledged-but-unsynced bytes/events
    + in-flight command upper-bound bytes/events
    + local pending-batch bytes/events
    + live claims
    + permanent repair reserve
    <= hard journal caps
```

Those categories are disjoint, so an accepted row is charged exactly once.
Synchronous append admission returns `NeedsFlightSettle` while a writer command
is in flight; async Agent code settles it and retries before accepting another
event. Cancellation can leave one owned flight, but cannot erase its quota
charge or admit closure rows past the hard cap.

Only deterministic closure/recovery code may consume the permanent gap. Its
exact event count and encoded byte ceiling are derived from maximal synthetic
templates and tested exact/one-over rather than left as an informal constant.
A predictable quota failure is therefore detected before a tool side effect.
An unexpected operating-system write/fsync failure may still make an outcome
unknown; recovery records that uncertainty and never fabricates success.

The old in-memory limits and the durable journal limits have deliberately
different closure semantics. Exhausting the bounded memory Session keeps the
existing `AGENT_EVENT_BUDGET` turn reason. A durable record, ordinary-event, or
ordinary-byte limit instead latches the first concrete append error, prevents
any not-yet-started tool or approval body from running, uses Agent-owned
`SESSION_LIMIT` tool results to close every already-declared call, then appends
balanced `step/end` and `turn/end`. The durable turn's protected fallback is
`AGENT_SESSION_LIMIT` from the moment its claim is created; it is not the old
memory-budget reason. Therefore, even when a large preferred `turn/end` cannot
fit and claim settlement selects its smaller fallback without returning an
append error, the journal remains truthful. After the final barrier the Agent
returns the original durable append error when one exists, or the stable store
limit classification for that fallback-only case; the process boundary emits
`CLI_SESSION_LIMIT`. A storage/barrier failure while closing remains stronger
because the balanced durable tail was not proven. Tests cover both event-limit
closure and a large terminal Provider failure whose first durable chunk fits
but whose duplicate preferred `turn/end` does not.

## Append, flush, and rollback

An in-process append remains atomic with respect to projection and the live UI.
Before calling the possibly stateful/fallible `Clock`, advancing `next_seq`, or
changing projection state, it validates the event and completes every fallible
allocation/reservation. It uses the current `next_seq` only as a read-only
candidate and computes a conservative complete encoded-row upper bound,
including type, maximum valid seq/time digits, data, surface/source metadata,
and LF. This is separate from the existing payload-only measurement used by
tool/result policy. If a nonempty
pending batch plus that row could cross 256 KiB or 256 events, it returns
`NeedsFlush` with no state change; async Agent code flushes and retries. An empty
batch may admit one record larger than 256 KiB only when that record is at most
9 MiB. This prospective check prevents a 255 KiB batch plus a 2 KiB record from
becoming an invalid ordinary command. After that check:

1. validate the typed payload and serialize/validate its bounded canonical
   `data` and metadata fragments;
2. prove sequence availability from the read-only `next_seq` candidate, build
   a small `ValidatedProjectionDelta`, and `try_reserve` every collection,
   string, row-buffer, lease, journal-quota, and resident-memory resource it
   will need;
3. only after those checks, call the clock once and validate the returned
   nonnegative timestamp. A clock error changes no Session state;
4. encode the envelope into the already reserved row buffer with a small
   infallible canonical encoder. The conservative bound makes buffer growth
   impossible; an internal bound/encoder contradiction poisons the Session
   rather than returning a retryable error with a consumed timestamp;
5. append the row to the already reserved pending batch, infallibly apply the
   delta, advance `next_seq`, and publish the committed UI projection.

No ordinary capacity, serialization, or storage error is possible after the
successful clock call. Live UI publication is deliberately post-commit: its
separately reserved/counted projection may fault and set the sticky observer
poison reported in the receipt, but cannot roll back the Session event. The
pending batch is bounded. `NeedsFlush` and `NeedsFlightSettle` are control
outcomes, not failed appends: callers await the owned storage state and retry the
same prepared event exactly once without inventing a second ID or timestamp.
Durable append returns an owned
`AppendReceipt { seq, time, event_type, observer_faulted, committed_message }`, not a
borrowed `&SessionEvent`. The full event exists transiently for serialization,
projection, and UI publication, but a return reference must not force log-only
chunks or other retired history to remain resident. Reservation settlement
returns the same owned receipt shape; callers needing full history use
`reader_from` and page its frozen durable snapshot.

Real durable callers use the crate-internal async
`append_settled(NewEvent)`: it prepares and validates the input once, retains
that same `PreparedEvent` across `NeedsFlush`/`NeedsFlightSettle`, awaits the
owned writer state, and moves the unchanged prepared value into the retry.
Claim settlement has matching `settle_settled`/`settle_exact_settled` helpers;
the claim remains unsettled across a control outcome. It never regenerates an
ID/time-bearing payload and never deep-clones a large row merely to retry
storage. A durable claim is a closed four-state owner:
`Ready(PreparedEvent)`, `PendingFallback`,
`PendingPreferred(PreparedEvent)`, or `Settled`. Selecting the fallback moves
the exact value into the Session-owned pending operation; selecting a preferred
value leaves the fallback in the claim. A pre-commit rejection moves the same
owner back to `Ready` when fallback remains legal. `PreferredOnly`, used after
an irreversible tool side effect, instead keeps the exact preferred result in
the Session-owned pending operation; it can never substitute the fallback. A
transient durable clock rejection is retried once from that exact owner.
If that retry is also rejected, the Agent returns the original Clock cause
instead of attempting a `step/end` that cannot pass the still-pending truthful
result; shutdown or cold recovery retains responsibility for the open tail.
Cancellation likewise leaves the candidate in Session until the same claim
resumes or shutdown takes over. A panicking injected clock is normalized to the
same pre-commit clock rejection, so it cannot strand a stream candidate in
front of the Agent's reserved failure closure. The public synchronous `append`
remains the finite memory-mode seam;
calling it on durable mode returns an async-required error before state change.
Durable `remaining_budget()` is likewise unavailable/fallible because the old
4,096/16 MiB answer describes only memory mode. Journal quota uses exact
`reserved_row_bytes`; the existing payload-byte helper remains separately named
and cannot be substituted into a durable claim proof.

Ordinary append never clones the complete active projection. A chunk delta is
O(1) and does not touch surface messages or lifetime identity sets; message
events add or replace only their owned/`Arc` surface nodes; identity deltas call
fallible reserve before insertion. A large surface replacement constructs one
new bounded node list and swaps it only after validation. The only whole-fold
clone is the single bounded cold-repair preflight described below. This avoids
turning 4,000 reasoning chunks into tens of gigabytes of repeated copying.

Compaction has two explicit larger critical paths; neither is a general bypass
of ordinary batching. After `compaction/start` is
durable and the summary call succeeds, `append_compaction_body` reserves the
worst-case three-record/19 MiB journal and allocation budget up front, then
commits events synchronously in upstream order: summary first, replacement
second, and success end last. Each event is separately validated, installed,
and published. A later validation/entropy failure does not roll back an earlier
logical event; it instead consumes the protected close reservation to append an
error end when possible. This preserves the upstream-observable start+summary
and start+summary+replacement prefixes. The rows actually committed by that
critical section become one writer command and one barrier; a physical crash can
still leave a whole-line prefix for cold recovery. No cancellation or unrelated
append can interleave between body events, but latched cancellation selects the
error end before the next ordinary action. This is not an all-or-none logical
transaction.

The second specialized path is one already-read, source-verified, and pre-sized
prune pair. It reserves at most 10 MiB, but each event's transition is validated
only at its upstream-ordered append point. It installs marker then replacement
sequentially, and sends the whole logical prefix actually committed as one owned
writer command.
It may therefore preserve a marker-only logical failure, while no ordinary
append, cancellation, or writer settlement can split the pair. A replacement
larger than 256 KiB because of preserved rich blocks, metadata, or extensions
uses this path rather than the ordinary single-large-record exception.

Before appending `compaction/start`, the owner also acquires a one-event/64 KiB
claim sized from the maximal normalized `compaction/end` template. Errors retain
the bounded provider-neutral failure name, code, and sanitized message, never a
raw opaque chain. The claim remains
held across summary, cancellation, and body-reservation failure; success or
error end consumes it. If the claim cannot be admitted, no start or summary
request occurs. A later operating-system write failure can still leave an
orphan, but ordinary quota pressure cannot.

One owned standard thread holds the journal fd and its lock for the Session's
entire lifetime. Async code sends it append, barrier, bounded `read_page`, and
finish commands through a capacity-one Tokio channel. An ordinary write command
owns at most 256 KiB; a single larger record may be sent alone and is still
bounded by the 9 MiB record limit. Only the explicitly pre-reserved compaction
body and prune-pair commands may contain multiple rows above that ordinary
command limit, bounded by 19 MiB and 10 MiB respectively. A read page has
independent event/byte caps
and uses positional reads, so it cannot alter the append cursor. Every command
has a one-shot acknowledgement. Serializing these operations through the same
owner avoids a read descriptor that could outlive shutdown or accidentally keep
the file's open-description lock alive. It also keeps queue memory bounded and,
unlike a moved `spawn_blocking` job, cannot lose the only journal owner when an
outer future is cancelled.

Read-only resume scan and range-read commands carry an owned cancellation flag
which the writer checks between fixed 64 KiB reads and after each complete
record. Cancellation before recovery mutation returns a cancelled
acknowledgement and allows the owner to finish/join promptly. Once a truncate or
repair commit command begins, it is non-cancelable: the signal is latched while
the command reaches a durable success or poisoned failure, after which shutdown
releases the lock. No cancellation check can split one JSONL row or a specialized
compaction/prune command.

The `JournalWriter` permits only one in-flight command. Its exact event/byte
upper bound stays charged after it leaves the local batch. It stores that command's
acknowledgement receiver in its own state before awaiting it; the receiver is
never only a local variable owned by a cancelable future. A later barrier or
shutdown first finishes the stored flight. Before channel capacity is obtained,
the unsent batch remains owned by `Session`; after a permit is obtained the
command is sent synchronously and its receiver is installed before any further
await. Dropping an outer Agent future can therefore stop an unsent command or
leave one owned flight, but cannot detach or duplicate a write. No new append is
admitted until that flight settles.

The writer tracks two positions: `physical_offset`, after bytes fully written,
and `durable_offset`, after the latest successful sync. An append writes the
full batch, advances only `physical_offset`, and acknowledges both cursors. A
barrier calls `sync_durable` as defined above; only success copies
`physical_offset` to `durable_offset`. Any partial write, write error, or failed
barrier is conservatively truncated to the last `durable_offset`, so all later
unbarriered batches are discarded together. Every rollback is then
synchronized. Any write, sync, rollback, rollback-sync, acknowledgement-channel,
or writer-thread failure poisons the journal and no Provider request or tool can
continue. Cancellation may stop a command before it is enqueued; once enqueued,
its stored flight must settle before another command or shutdown can finish.

The current-thread Tokio runtime never performs blocking disk I/O directly.
Clean shutdown settles any owned flight, sends `finish`, waits for the final
sync acknowledgement, takes and drops the sole sender, and joins the writer
thread. `Drop` is only an abnormal fallback: it cannot promise to flush a batch
that was never enqueued, but it takes and drops the sender *before* joining, so
the receiver reaches EOF after queued work and no file owner is detached or
deadlocked. The implementation never clones the command sender outside the
owned writer handle.

No production path calls `std::process::exit` while an `Active(Journal)` or a
prepared-resume writer still exists. Interactive and script drivers return a
typed exit disposition to the owner; the owner takes the Session from the
`AgentLoop`, performs the final barrier and `Session::shutdown`, and only then
returns the process status. Script mode also shuts the journal before spawning
the owned final stdout/stderr writer. Its existing deadline/signal fallback may
still use `process::exit` for a kernel-stuck output thread, but by then Agent,
tool, and journal state are already settled and the sole journal lock/fd is
closed. A recovery-warning output failure first aborts/closes the still
unmodified `PreparedSession`; it cannot force-exit with that writer alive.
`DeferredNew` has no writer and its shutdown is a no-op. Suspend is different
from exit: it keeps the owned writer, but completes the mandatory barrier before
the process self-stops and revalidates it after resume. These ownership rules,
not destructor luck, make clean exit, output timeout, and terminating signals
durable.

Durability barriers are mandatory:

1. after user/header/context facts and before every Provider dispatch;
2. immediately after `tool/call` and before `ToolExecutor::prepare`, because
   preparation itself may read the workspace;
3. after `approval/asked` before calling `ApprovalProvider`, and after an
   allowed decision before any prepared read, patch, Shell commit, or process
   start (read-only tools use the same rule for one uniform audit trail);
4. after tool results and before the next Provider request;
5. after an assistant attempt closes and before its raw chunks are retired;
6. after `turn/end` before a new interactive prompt or script stdout;
7. after repair and `session/end-seed` before a resumed Session is published;
8. after a compaction close/replacement transaction;
9. during every clean exit, terminating-signal path, and suspend path.

## Single-writer ownership

The process opens the journal itself and takes a non-blocking exclusive `flock`
before it inspects or repairs a session. The same fd remains owned by the writer
thread until the final barrier and shutdown complete. A second writer gets
`CLI_SESSION_BUSY`, exits 1 without waiting, makes no network request, and
changes no byte. Kernel release after a crash allows a later resume. Listing
opens a separate read-only fd, reads only the bounded header, and takes no
writer lock.

This contract is for local macOS and Linux filesystems. It does not claim that
every network filesystem implements advisory locks or fsync durability.

## Tail scanning and cold recovery

Resume first opens and locks the journal, validates the header and path ID, then
opens the stored or explicitly asserted workspace and verifies its canonical
path and device/inode identity. A mismatch returns without scanning, truncating,
or repairing the body. Only after workspace authority is established does cold
recovery preparation run, still before Provider, credentials, tools, the live
Session observer, or a new user turn. The existing bounded terminal/script
writer may be opened solely to surface a recovery warning before mutation:

1. read and validate the complete header line;
2. scan records with a fixed line buffer, validating JSON, sequence continuity,
   event shape, and projection transitions;
3. remember the byte offset after every complete valid record;
4. ignore a final fragment with no LF and select the preceding valid offset as
   the recovery truncation target;
5. for a complete bad JSON event or sequence gap, continue a bounded structural
   scan of the suffix. If a later valid
   complete event envelope of any type exists, reject the file as interior
   corruption; a later `tool/call`, approval, result, or provider-visible fact
   is sufficient even without `turn/end`. Only when everything from the first
   bad complete row through EOF is an unverifiable final suffix may recovery
   select the last valid offset as the truncation target;
6. derive the bounded `RecoveryReport` and deterministic repair plan, clone only
   the bounded `ValidationIndex` produced by the streaming scan, and preflight
   every missing repair plus `session/end-seed` against that clone as one
   seed-replay candidate. Record a prepare token containing the opened file's
   device/inode, exact physical length, SHA-256 of all physical bytes, and a
   digest of the normalized plan/fold. Up to this point inspection is read-only;
7. return the prepared plan/report to the CLI. If the report is nonempty, fully
   write its sanitized warning to the interactive terminal or bounded script
   stderr before permitting any journal mutation. Output failure, cancellation,
   or a terminating signal here releases ownership with zero changed bytes;
8. commit first uses the same opened fd to re-stat and stream-rescan/re-hash the
   bounded journal immediately before mutation. Through the retained store-root
   capability it also performs `fstatat`/`NOFOLLOW` on the canonical filename
   and requires that path still names the same regular device/inode, with link
   count one, effective-uid ownership, and exact mode. Identity, namespace
   binding, length, physical hash, and normalized plan/fold must all match the
   prepare token; otherwise return `CLI_SESSION_CHANGED` with zero changed
   bytes. Then synchronize the selected
   truncation, append only the missing repair suffix, remember
   `resume_seed_len` after repair closers but before an optional new end-seed
   marker, and flush the repaired journal;
9. build the bounded active projection and set
   `first_live_seq = resume_seed_len`
   (if the seed already ended in a marker, no new marker is appended and the
   current seed length is used; this matches upstream seed construction);
10. attach the observer at the current post-marker `next_seq`, so it sees only
   later events and does not replay the recovery marker as live output.

This preflight never reloads the complete journal or constructs a historical
`Vec<SessionEvent>`; the words "whole candidate" mean sequential validation
against the cloned fold state, not materializing up to 512 MiB. A crash may leave
any whole-line prefix of a repair batch, including `turn/end` before the final
seed marker. The scanner recognizes only the exact deterministic prefix of the
same repair plan and continues with its missing suffix; arbitrary lookalike
events remain corruption. Whole-candidate validation includes the seed-replay
exception that lets a final `session/end-seed` clear an inherited compaction
orphan. Thus recovery remains valid after a crash between any two repair rows,
while ordinary live append invariants are never weakened. Repeating resume is
idempotent: the second pass adds no duplicate repair and a seed already ending
in `session/end-seed` is not marked again.

Because the complete warning is written before the first truncation, a crash in
the later truncate/repair window cannot make a risk silently disappear: that
process already emitted the report, and a remaining open repair prefix is
handled on the next resume. Repeated warnings are acceptable; mutating first and
trying to warn later is forbidden.

### Open turn and tool repair

If the tail ends after one or more `assistant/chunk` events but before their
attempt was closed by an assistant message, retry, context-overflow
prune/start boundary, or step end, the repair fold
first prepares `AttemptDisposition::Interrupted`. The disposition is consumed
atomically by the synthetic `step/end` later in the repair order. It drops the
partial assembler and installs no assistant surface/token anchor, while usage
chunks already folded into cumulative usage remain authoritative. This same
state transition is used by a live cancelled/failed attempt apart from its
distinct disposition tag, so hot cancellation and cold interruption cannot
derive different next-turn pressure from the same durable prefix.

Within an open step, assistant tool-call blocks determine stable model order.
Recovery classifies each unresolved model-declared call from durable facts:

| Durable facts | Synthetic decision/result | Result sources |
| --- | --- | --- |
| no top-level `tool/call` | `TOOL_NOT_STARTED` | none |
| `tool/call` + unmatched `approval/asked` | first `approval/decided(cancelled)`, then `APPROVAL_CANCELLED` | the call seq |
| `tool/call` + rejected/cancelled/unavailable decision | the corresponding stable `APPROVAL_*` failure | the call seq |
| `tool/call` + allowed-once decision | `TOOL_OUTCOME_UNKNOWN` | the call seq |
| `tool/call` with no approval request | `TOOL_OUTCOME_UNKNOWN` | the call seq |

The unmatched-approval case is definitely not started because the durable allow
barrier never occurred; its `APPROVAL_CANCELLED` text states that no body was
dispatched. Using the same result as an already durable cancelled decision makes
a crash exactly between the synthetic decision and result resume as the missing
suffix of the same plan. An allowed or no-approval call is conservatively unknown
because the body may have started after its durable intent. Recovery then
uses one total order: append every needed synthetic approval decision first
(normal serial dispatch permits at most one), then append all synthetic results
in assistant block order, then `step/end` when needed, then
`turn/end { interrupted }`, then the seed marker. Thus a pending approval on the
second call is decided before the first call's result, while the two results
still preserve model call order. All synthetic decisions, results, and
step/turn boundaries use consecutive sequences and the last pre-repair real
event's timestamp. The final
`session/end-seed` is an ordinary recovery-lifecycle append and uses the current
recovery clock, matching upstream rather than reusing the old timestamp. Tool
result and boundary ordering matches the fixed upstream repair helper; the
approval decision is the intentional Rust difference stated above. An isolated
raw `tool/call`, duplicate intent, or assistant/intent argument mismatch cannot
reach recovery: durable live append rejects it atomically and cold scan reports
corruption.

Recovery never calls `ToolExecutor`, `ApprovalProvider`, or `ModelProvider`.
`TOOL_OUTCOME_UNKNOWN` tells the next model/user to verify an external side
effect before retrying it.

Durable-mode validation never permits `step/end` or `turn/end` while any
model-declared call remains unresolved, whether it lacks top-level intent,
awaits a decision, or has intent/decision but no correlated result. A file containing a closed dangling
call is therefore corrupt, not repaired by inventing a later recovery turn.
Phase 1 snapshots are not automatically imported into the Phase 8 journal, so
there is no released durable Rust artifact requiring such a migration.

### Interrupted compaction

Cold recovery never fabricates `compaction/end`. It validates the only three
legal orphan prefixes: start; start + summary; or start + summary + the valid
adjacent replacement. The first two leave the old surface unchanged; the third
keeps the already-committed replacement effective. Malformed, mismatched, or
non-adjacent bodies are corruption rather than recoverable prefixes. A
provider-overflow compaction may legitimately be inside an open step; ordinary
pressure compaction is before the step. Recovery therefore first closes any
pending approval and model-declared tool results, then the open step, then the
interrupted turn, and only then appends `session/end-seed`. An orphan combined
with a pending tool/approval state that could not arise at either compaction
hook is corruption. The final seed marker clears the unmatched compaction
invariant for the new lifecycle. The orphan remains visible in history, and a
later compaction may start without a synthetic end that upstream would not
produce.

### Recovery report

`PreparingResume::finish` exposes a `PreparedSession` with a bounded
`RecoveryReport` before `begin_commit` can enqueue any mutation. It records only
counts, stable recovery codes, the number of truncated bytes,
the presence/kind of an orphan basic-compaction prefix, the count of orphan
prune markers, and at most 64 repaired-call entries. Each entry uses a tool
label only when it exactly matches a schema name in the durable request header
(otherwise `<unknown-tool>`) plus a short domain-separated SHA-256 fingerprint
of the call ID, never either raw model-supplied identifier. The complete visibly
escaped warning is preflighted at
256 KiB and is never truncated. It never copies
arguments, tool bodies, raw corrupt bytes, environment values, credentials, or
approval challenges. All untrusted identifiers go through the existing visible
control sanitizer.

An interactive resume renders this report as a warning before the first prompt.
Script mode keeps stdout final-answer-only and writes the warning through its
bounded stderr writer before `begin_commit`. In particular, every
`TOOL_OUTCOME_UNKNOWN` warning tells the user to verify the external effect
before retrying. A recovery warning that cannot be rendered is an ordinary CLI
output failure before any journal mutation, new Provider request, or tool side
effect; it is never silently discarded. Only after the complete warning frame
has been written does the CLI begin and settle the owned repair commit. If
the process crashes during later repair, the next read-only preparation derives
and warns about the remaining recoverable state again; the earlier process had
already written the original report before changing a byte.

## Resume and listing CLI

The public syntax becomes:

```text
dsh [OPTIONS]
dsh --resume <SESSION_ID> [OPTIONS]
dsh --list-sessions [-w <PATH>] [--no-color]
```

Default invocation creates and persists a new session. `--resume` may be used
with interactive input, `--prompt`, or piped stdin. It is mutually exclusive
with `--list-sessions`. Listing permits only workspace filtering and plain
rendering; prompt/model options are usage errors.

`--list-sessions` is handled before workspace tools, Provider construction,
credential lookup, terminal opening, or the Tokio runtime. It succeeds on an
empty store without an API key or network and emits only sanitized ID, creation
time, and workspace path. It reads no prompt or event body. Valid headers are
sorted by `createdAt` descending and then canonical session ID ascending, so
filesystem iteration order cannot change output. A busy valid journal remains
listable; an incomplete/invalid header is omitted from output while its
canonical filename still consumes a store slot.

The first CLI format is deliberately plain and machine-checkable: an empty
list writes zero bytes; otherwise each physical line is
`<canonical-id> TAB <createdAt-unix-ms> TAB <workspace> LF`. The ID and decimal
timestamp are validated facts. The workspace field is rendered through a
single-line terminal-safety boundary: LF, TAB, CR, C0/C1 controls, bidi/format
controls, and escape bytes become visible escapes, so a hostile hand-written
header cannot forge another row or terminal command. `-w/--workspace` is opened
once and compared to the durable workspace device/inode, rather than opening
the untrusted stored `cwd` path. A structurally valid safe-integer future
header version may remain visible in the list when it still has the complete
Rust durable workspace metadata; actual resume performs the version check and
returns `CLI_SESSION_UNSUPPORTED`. This keeps discovery useful without treating
an unknown event schema as readable.

On resume:

- no explicit workspace uses the canonical `cwd` in the header;
- an explicit workspace must resolve to the same directory identity;
- argument parsing retains `--model` as `Option<String>` rather than eagerly
  inserting the CLI default, so resume can distinguish a real user override;
- no explicit model reuses the latest durable request-header model, falling
  back to the CLI default only if no prior header exists;
- when a prior durable request header exists, an explicit model may change the
  effective header but the resumed Agent's first new header still records
  `reason: resume`; `change` is only for later reconfiguration in that same
  Agent lifecycle;
- when no prior request header exists because the session stopped before its
  first dispatch, the first real request remains `reason: initial`, matching the
  fixed upstream baseline rule;
- an interactive banner reports the session ID and `new` or `resumed` state;
- script stdout remains exactly the authoritative final answer, so it does not
  gain a session banner.

There is no active-session switch. `/exit`, Ctrl+D, and terminating signals all
go through owned Agent cleanup and the final journal barrier before releasing
the lock. Ctrl+C settles and flushes the current turn, then keeps the same lock
and session for another prompt. Ctrl+Z settles and flushes before self-stop.
SIGKILL is not catchable; next resume uses the recovery rules above.

Stable storage failures use exit 1 and one sanitized code:

```text
CLI_SESSION_ROOT_UNAVAILABLE
CLI_SESSION_STORE_BUSY
CLI_SESSION_NOT_FOUND
CLI_SESSION_BUSY
CLI_SESSION_CHANGED
CLI_SESSION_WORKSPACE_MISMATCH
CLI_SESSION_UNSUPPORTED
CLI_SESSION_CORRUPT
CLI_SESSION_LIMIT
CLI_SESSION_IO
```

Malformed option combinations and noncanonical IDs remain `CLI_USAGE`, exit 2.

## Context measurement and compaction

Automatic pressure compaction runs at the fixed upstream pre-step boundary:
after `turn/start` and the input batch is claimed, but before `step/start`, its
new user messages enter the surface, or the ordinary request header/context is
appended. Like upstream, its 80% token-pressure value measures only the
previously committed Session surface. When a durable ordinary request header
exists, the summary request reuses its system/tool prefix exactly like upstream;
Rust also duplicates the actual prefix in `compaction/start` so the auxiliary
request is self-contained. In the Rust-only first-turn hard-limit path where no
header exists, it uses the current bounded Agent system/tools and records those
inline. The separately prepared compaction config is likewise captured in the
start recipe. Separately, before input admission, Rust
preflights the proposed surface plus claimed input against the hard
4,096-message and 8 MiB encoder limits and may invoke the same compactor as the
intentional safety difference above. Canonical provider-overflow recovery runs
after a failed ordinary request and compacts that request's committed surface
before its one replay. Raw stream retirement solves the event-budget incident;
compaction separately keeps the model-visible surface under provider message,
byte, and token limits.

`CompactionTrigger` has exactly three durable spellings: `pressure`,
`context-overflow`, and `hard-limit`. `pressure` is the compatible 80% path;
`context-overflow` is the post-provider-error path with a zero-token retain
target; `hard-limit` is Rust's pre-admission safety path and may run even when
the committed pressure is below 80%. The hard-limit path first prunes, then uses
the ordinary 16% balanced-tail selection and at most two successful summary
transactions, rechecking the virtual surface plus claimed input after each. It
stops as soon as the 4,096-message and 8 MiB encoder checks fit. The claimed
input is never included in the summary request because it is not yet committed.
No safe range or a still-oversize request after the bounded passes releases the
unstarted step claims and closes the already-open turn with the stable context
failure.

That local failure is exactly
`LlmFailure { code: "AGENT_CONTEXT_LIMIT", message: "the conversation cannot be reduced to fit the model request limits" }`.
Message-count, 8 MiB encoder, provenance-capacity, no-balanced-range, and
bounded-pass exhaustion all use this same non-secret code/message when the
virtual request still cannot be admitted. An ordinary pressure pass whose
original request fits remains advisory and does not produce this turn error.
After an actual Provider `CONTEXT_WINDOW_EXCEEDED`, failure to make any durable
replacement progress preserves the original provider failure instead of
rewriting it as `AGENT_CONTEXT_LIMIT`.

### Frozen compaction event wire

The Rust format keeps the four upstream event tags and all upstream field
names. `compaction/start` adds one optional `dispatch` object. Its absence is
accepted only by the finite in-memory compatibility reader for an upstream
event; every new or resumed Rust durable journal uses `DurableStrict` and
requires it. This prevents a temporary Phase 8 payload from becoming an
accidental permanent format while still allowing the committed upstream oracle
to decode.

```text
compaction/start.data = {
  compactionId,
  sourceCommandId?,
  turn: positive-safe-integer | null,
  dispatch?: {
    trigger: "pressure" | "context-overflow" | "hard-limit",
    sourceSurfaceGeneration: non-negative-safe-integer,
    shadowedRange: { start: EventSeq, end: EventSeq },
    shadowedSeqs: EventSeq[],
    preparedCall: {
      config: LlmCallConfig,
      adapterDefaults: LlmCallConfigAdapterDefaults,
      contextWindow?: non-negative-safe-integer,
      retryPolicy: {
        mode: "normal" | "always",
        maxRetries?: non-negative-safe-integer,
        retryableCodes: string[],
        backoff: {
          initialDelayMs: positive-finite-number,
          maxDelayMs: positive-finite-number,
          jitterRatio: finite-number
        }
      }
    },
    requestHeaderSeq?: EventSeq,
    requestContextSeq?: EventSeq,
    system?: string,
    tools: ToolSchema[],
    sessionId: SessionId,
    purpose: "compaction",
    instruction: Message,
    instructionFormatVersion: 1
  }
}

compaction/summary.data = {
  compactionId,
  sourceCommandId?,
  summary: ContentBlock[],
  rawOutput?,
  llmStreamCall?: true,
  shadowedRange,
  shadowedSeqs,
  shadowedTokenCount,
  provider,
  model,
  maxTokens?,
  usage?
}

compaction/end.data = {
  compactionId,
  sourceCommandId?,
  turn: positive-safe-integer | null,
  error?: LlmFailure | string
}

compaction/prune.data = {
  shadowedRange,
  shadowedSeqs,
  shadowedTokenCount
}
```

The string form of `compaction/end.error` is read only by the in-memory
upstream-compatibility path. That reader accepts arbitrary UTF-8 (including an
empty string and control characters, matching upstream `errorChain`) up to a
32 KiB Rust safety cap. Rust durable production and cold replay reject it;
they write the existing provider-neutral `LlmFailure` object. The earlier draft
incorrectly mentioned a failure `name`; `LlmFailure` has only the durable
message/code plus its optional bounded status, retry-after, and request ID, so
those are the authoritative fields. A durable compaction failure additionally
requires a nonempty, non-control code <= 256 bytes, a nonempty message <= 32 KiB,
a nonempty, non-control request ID <= 1,024 bytes when present, and complete
compact failure JSON <= 48 KiB. Together with identifiers below this
leaves a conservative margin inside the reserved 64 KiB close row; the exact
encoded row is still preflighted before `compaction/start`.

Both `compactionId` and `sourceCommandId`, when present, are 1 to 1,024
non-control UTF-8 bytes. Provider/model are 1 to 1,024 non-control UTF-8 bytes.
`shadowedSeqs` is nonempty, ordered by current-surface position, has distinct
entries, begins/ends at `shadowedRange.start/end`, and contains at most 4,094
entries for a summary or exactly one for a prune. A summary/raw-output list has
at most 4,096 blocks; tools have at most 256 entries; system text has at most
4 MiB. Retry policy has at most 256 distinct nonempty codes of at most 256
bytes; normal mode requires `maxRetries` and a nonempty code list, while always
mode omits `maxRetries` and requires `retryableCodes: []`. Backoff uses the
existing timer ceiling and requires initial <= maximum and jitter in `[0, 1]`.

`instructionFormatVersion` is exactly `1`. Its message ID is 1 to 1,024 visible
ASCII bytes. The message is a user-role plugin
message sourced from `dsh.compaction`; its complete body, ID, extension fields,
and source are facts, not regenerated during replay. The future live producer
owns the exact canonical instruction text and deterministic message ID. The
Session schema validates its bounded role/source/version shape and preserves
the exact message; cold replay compares the complete dispatch snapshot rather
than trusting a recomputed string.

The exact first-turn order is: validate the raw `TurnProposal` shape/bytes;
materialize a deferred journal; claim and append `turn/start`; reserve but do
not settle the prospective `step/start`/user-message/`step/end` batch; measure
ordinary pressure on the committed old surface and hard limits on a virtual
surface that includes the claimed input; run any needed compaction; build a
borrowed `ProviderRequestDraft` for that exact virtual generation and invoke the
provider-specific wire preflight; only then settle `step/start` and the user
messages. A wire-too-large result, or a provider preparation whose materialized
defaults push the provider-neutral request over its retained
message/tool/byte limits, re-enters the bounded `hard-limit` compaction loop and
preflights the new generation again. The final successful result keeps
its exact `PreparedProviderCall`, encoded byte count, and virtual generation;
after claims settle, request-header/context logging and the mandatory barrier,
Agent constructs and dispatches that same request. Any generation/config drift
is an invariant failure before network I/O. No compaction event can be appended
to a deferred Session. If hard preflight still cannot admit the virtual request,
the unstarted step claims are released and only the already-open turn is closed
with the stable context failure. `Reject` and empty `Enter` never enter this
pre-step path but still materialize and close their balanced turn as described
above.

If provider preparation itself is invalid, there is no encodable request whose
size could violate the hard gate. The preflight returns that bounded preparation
failure without side effects; Agent then settles the already reserved step/input,
consumes the same one ordinary-attempt slot as Phase 3, records the same stable
prepare failure, and never calls `stream`. If the turn has no remaining attempt
slot, it skips provider preflight, settles the step/input, and reports the
existing attempt-limit failure. Thus the new size seam moves no ordinary
prepare/attempt/error fact across the visible step boundary, while a genuine
wire-size failure still leaves no partial step.

The default policy follows the fixed upstream values:

- trigger at 80% of the provider's context window;
- for ordinary pressure, retain a newest balanced tail worth 16% of the context
  window;
- request at most 8,192 summary tokens;
- after one successful pressure compaction, allow one more full transaction only
  when committed pressure is still at or above the threshold; a failed summary
  is not retried;
- after a canonical context-window error, permit one forced compaction and one
  replay of the original request only if the durable surface became smaller;
  overflow selection passes a zero-token retain target (while the range
  algorithm still leaves at least one balanced surface node), rather than the
  ordinary 16% tail.

Every compaction summary call shares the ordinary Agent's hard resource
accounting. Before `prepare_call`, it checks cancellation and the remaining turn
deadline, then consumes one slot from the same
`AgentLimits::max_attempts_per_turn` counter; a preparation failure consumes the
slot just as an ordinary attempt does. The prepared summary `maxTokens` must be
nonzero and no greater than `max_output_tokens_per_request`, and the provider
stream uses the same remaining turn deadline and cancellation token. Reported
summary output usage is added to the same
`max_reported_output_tokens_per_turn` counter. If the attempt slot, per-request
token cap, total reported-output cap, deadline, or cancellation stop is reached,
that hard turn stop overrides advisory compaction continuation: no pre-start
bracket is invented, or an already committed start is closed and barriered
before the turn ends. The optional second pressure/hard-limit transaction and a
later overflow replay each require another independent attempt slot. A
compaction Provider failure is handled once by the compaction owner; it never
enters the ordinary network-retry scheduler.

When an otherwise complete summary reports usage that crosses the cumulative
turn cap, its bounded raw output and usage are still appended as the log-only
`compaction/summary` fact, but no surface replacement is installed. The owner
then writes `compaction/end { error: AGENT_TOKEN_BUDGET }`, barriers it, and
closes the turn with that same hard reason. This mirrors ordinary attempts,
whose already committed chunks are not erased when their usage reaches the
limit, while preventing another model dispatch.

An ordinary pressure pass is advisory. Any failure or exhausted pressure pass
(configuration/context resolution, no safe range, start/summary/body/close
failure, or two successful transactions that still leave pressure high) emits
one sanitized interactive notice or script-stderr warning and continues the
normal step when the original virtual request still satisfies the hard encoder
limits and cancellation/observer/I/O poison has not won. A failure before start
has no compaction event; after start, the owner appends the bounded error end
when possible and barriers the committed prefix before continuing. If the
request does not fit, the turn closes with the stable context failure. A
pressure failure is not itself retried and does not write or consume normal
network-retry facts.

Provider-overflow replay is a separate one-shot internal gate, not the normal
retry scheduler. It has no delay, writes neither `llm/retry` nor
`llm/retry-started`, and does not increment the normal retry counter or consume
its policy budget. The replayed dispatch does increment the global
provider-attempt/turn safety counter and requires a remaining attempt slot. It
rebuilds the ordinary request from the now-current durable surface. A completed
`assistant/message` or return to Agent idle resets the overflow gate; a second
context overflow before that reset cannot recurse. Cancellation wins at every
boundary. Durable pruner replacement progress may justify this one replay even
when no basic range exists or the later summary ends in error; marker-only or no
replacement progress preserves the original provider context error.

The deterministic estimator follows upstream's JavaScript vocabulary exactly:
known text/reasoning/name/argument strings are charged by UTF-16 code units
divided by four and rounded up, with fixed role/block overhead; tool results
recurse; only unknown blocks use compact JSON's UTF-16 length. System text and
serialized tool schemas use the same estimator. "Compact JSON" here means
JavaScript `JSON.stringify` text, including JavaScript number formatting; it is
not `serde_json::to_string` byte length. The existing `ryu-js` dependency keeps
small-exponent floats aligned. For the previous request, the
usage candidate is input + cache-read + cache-write + output tokens; reported
reasoning tokens are already part of provider output and are not added again.
The heuristic candidate is header tokens + the surface captured at
`step/start` + the source-reconstructed provider-assistant price. The meter
keeps the larger candidate, then applies signed estimated deltas for later
surface changes. This is the upstream `TokenMeter` rule; the separate
prompt-side context-pressure projection is not substituted for it. Rust tests
include output usage and non-BMP text so neither a prompt-only sum nor UTF-8
bytes can accidentally replace the upstream calculation. Unknown blocks and
tool schemas also cover control escapes and exponent-boundary floats. Pressure
checks 4,096 provider messages, the 8 MiB request encoder, and an active-surface
byte high-water mark, so thousands of tiny messages or one large message do not
bypass compaction.

### Model-free tool-result pruning

The default pruner matches the fixed upstream configuration:

- trigger only when the combined text blocks in one current-surface
  `tool/result` contain strictly more than 8,192 Unicode code points;
- keep the first 4,096 and last 1,024 code points and insert the exact 39-code-
  point marker `\n\n[... tool result middle pruned ...]\n\n` once;
- treat all text blocks as one continuous stream, while preserving every
  non-text block and its relative order; whenever pruning triggers, every text
  block whose rebuilt text is empty disappears, including a block that was
  already empty, unless it owns the marker;
- count code points like JavaScript `Array.from`, so one non-BMP character such
  as an emoji is never split. Grapheme clusters made from multiple code points
  are not promised atomic in either implementation.

The configuration requires a positive integer threshold, nonnegative integer
head/tail, no unknown keys, and `head + marker + tail <= threshold`. Defaults
therefore reduce a triggered text stream to exactly 5,159 code points. An exact
8,192-code-point result is unchanged, and a second pass over a default-pruned
result is a no-op.

When ordinary committed pressure is below 80%, upstream does not prune
opportunistically. At or above pressure, Rust snapshots the current surface,
prunes oversized results in surface order, remeasures, and skips the summary
transaction if pruning alone made pressure safe. Context-overflow and Rust's
hard encoder preflight always try the same pruning pass before selecting a basic
summary range.

Only the context-overflow path can enter this pass with an open ordinary
attempt. Its first marker append is also the atomic
`AttemptDisposition::ContextOverflow` boundary; subsequent candidates see no
old token. The pair is durably settled and that attempt retired before a replay
token can begin. Pressure and hard-limit passes require no open attempt.

For each candidate, the Session first reserves one bounded 10 MiB pair, then in
one synchronous critical section appends:

1. log-only `compaction/prune` with the original singleton range/sequence and
   its pre-prune token price;
2. an adjacent replacement `tool/result` that preserves turn, step, message ID,
   source/call IDs, `isError`, error, metadata, extension fields, and every block
   except the pruned text content. Its `surfaceOp` replaces exactly the original
   seq and `sourceEventSeqs` is exactly that singleton.

The token projection arms a one-event shadow-price claim on the marker; only the
immediately adjacent matching replacement consumes it and applies
`replacement price - shadowed price`. Any intervening event expires the claim.
The pair is sequential, not all-or-none: if replacement validation fails, the
marker remains committed and the surface still points to the full original.
Cold recovery does not invent a replacement; its next closer or end-seed expires
an orphan claim. A later pass may safely retry the still-current original.

The pruner makes no model request. Fixed upstream performs its snapshot loop
synchronously without an inner signal check. As intentional difference 16,
Rust checks cancellation before the pass, between candidates, and every 64 KiB
while authenticating a source row; once a pair starts, it commits both rows or
reaches the specified marker-only failure before observing cancellation. A
writer settlement may occur between complete pairs, never inside one. All
committed pairs reach the ordinary pre-provider/turn-end barrier. A hard crash
may therefore leave neither row, a marker-only prefix, or the full pair,
exactly as the append-only upstream protocol permits.

Selection takes the oldest surface prefix while retaining the newest tail. A
`shadowedRange` is defined by the two endpoints' positions in the current
surface, not by numeric sequence order. After an earlier replacement moved a
new high sequence to the front, a later valid positional range may therefore
have numeric `start > end`; ordered sources follow surface order and no
`start <= end` check is permitted. Selection
may move a boundary outward to keep an assistant tool call and all correlated
tool results together. It never compacts an open tool pair or the current
unsettled step. If no balanced, genuinely shrinking range exists, selection
returns `None` before `compaction/start`; no failed-compaction event is
fabricated. Ordinary pressure continues only if the request still fits. On
provider overflow, a durable pruner replacement that advances the dedicated
validated-replacement generation is sufficient shrinking progress to replay
the request once, even if the basic range is `None` or later summary work
fails. Ordinary surface events never advance this gate. Marker-only/no-
replacement progress preserves the original context error. Cancellation always
wins and prevents replay.

The replacement provenance list is also bounded: start and summary consume two
entries, so one transaction shadows at most
`MAX_SOURCE_EVENT_SEQS - 2` (currently 4,094) surface nodes. Selection enforces
this before starting the summary call. It first computes the ordinary balanced
cut, clamps that prefix to 4,094 nodes, then moves the cut only toward the
surface head until it is balanced again. It never moves toward the tail and
silently exceeds the provenance bound. A larger eligible prefix is split
across at most the two permitted successful pressure transactions;
exact/one-over tests prove the boundary. If that cannot produce an encodable
request, the turn gets a stable context error; provenance is never silently
truncated.

One transaction is:

1. without consuming a provider-attempt slot, finish the bounded prune,
   pressure/range selection, source-list/record/quota preflight, and
   whole-surface-generation validation needed to prove that a summary request
   will actually be sent. A no-range or prune-only outcome returns before the
   attempt counter changes;
2. check cancellation/deadline, claim one ordinary provider-attempt slot, and
   call `prepare_call` before any bracket event. Validate its effective
   `maxTokens` against the per-request Agent limit. Capture a serializable
   `PreparedCompactionCallSnapshot` containing
   the complete effective `LlmCallConfig.raw` (provider, model, reasoning effort,
   temperature, max tokens, stop strings, and extension fields),
   `adapterDefaults`, `contextWindow`, and the bounded prepared retry policy. The
   process-local `ProviderBinding` is excluded; the exact hot prepared value is
   retained for the one dispatch after the barrier;
3. acquire the bounded close claim, then append and flush `compaction/start`
   with a new fallible random ID, current turn owner, exact trigger, source
   surface generation, ordered selected surface sequences plus positional
   endpoints, the prepared-call snapshot, latest durable request-header and
   request-context sequence references when present, the exact canonical bounded
   system text and ordered tool schemas actually sent, `sessionId`,
   `RequestPurpose::Compaction`, and the exact bounded canonical product
   summary-instruction `Message` plus its format version. Its role is `user`,
   its source is exactly `MessageSource::plugin("dsh.compaction")`, and its
   bounded ASCII message ID is domain-separated and deterministically derived
   from the compaction ID and prompt-format version. When an ordinary header
   exists, its referenced system/tools must equal the stored compaction prefix;
   when none exists, the inline values remain the complete durable owner. The
   ordered source sequences reconstruct selected messages; hard-preflight
   claimed input is not included because the summary does not see it. The
   complete start row is prevalidated against the 9 MiB record and journal quota
   before append. For context overflow whose pruner emitted no marker, this
   start append atomically consumes the old ordinary-attempt token as
   `ContextOverflow`; pressure/hard-limit starts require no open token. Cold orphan
   validation checks this recipe;
4. move the same prepared call into one Provider request with the recorded
   system, tools, selected messages, instruction message, purpose, and session
   ID. Aggregate this auxiliary stream inside the bounded compaction owner and
   do not append ordinary `assistant/chunk`/`assistant/message` facts, execute
   tool output, or enter normal network retry;
5. accept only a successful finish with no image/tool-call block and at least
   one text block whose `trim()` is nonempty. Reasoning may coexist and remains
   only in the raw summary event. Preserve every text block, its order, and its
   retained extension fields, and frame those blocks with product-owned leading
   and trailing text blocks; do not flatten them into one joined string;
6. revalidate the selected surface generation and balanced range;
7. verify the framed checkpoint's estimated price is strictly smaller;
8. append `compaction/summary` inside the capacity-reserved critical section;
9. attempt a synthesized user checkpoint with
   `surfaceOp: replace` and sources containing start, summary, and every
   shadowed surface sequence;
10. append `compaction/end` with success, or append an error end if summary/body
   work fails, then send the one-to-three body events actually committed (at
   most 19 MiB) as one writer command and barrier before any ordinary request.

The durable comparison object is a `ModelVisibleDispatchSnapshot`, not the
process-bound `ProviderRequest` value. It includes the serialized prepared-call
snapshot and every provider-visible request field listed above. Hot dispatch
derives it before moving the prepared call; cold replay derives it only from the
start recipe and referenced durable rows, and the two must compare exactly.
The runtime-only binding is re-established by a live Provider instance and is
never claimed crash-reconstructible. An orphaned summary call is not resumed.

The replacement content is visibly framed as an automatically generated
checkpoint and tells the next model to continue from the retained newer tail.
The raw summary and routed provider/model/usage facts remain log-only in
`compaction/summary`. The replacement message is the only new model-visible
node. Old events and chunks remain readable from the journal.

After `compaction/start`, every ordinary failure attempts exactly one
`compaction/end { error }`. Cancellation remains authoritative after that close
and flush. A failure to append the close leaves an intentional orphan that is
handled only by cold recovery.

## Module ownership and interfaces

New modules have one primary responsibility:

```text
src/session/jsonl.rs          header/event rows and bounded streaming scanner
src/session/path_policy.rs    store root, canonical ID, permissions, no-follow paths
src/session/journal.rs        lock, append/fsync/rollback, cursor, quota, range reader
src/session/store.rs          create/list/inspect/prepare-resume
src/session/recovery.rs       pure repair planning and cold repair commit
src/session/attempt_anchor.rs shared bounded hot/cold provider-stream fold
src/session/context_budget.rs surface pricing, pressure, balanced selection
src/session/compaction.rs    durable compaction schema, prepared/retry snapshots
src/session/tool_result_pruner.rs bounded text transform and adjacent pair facts
src/agent/compaction.rs       summary call and compaction transaction
src/workspace_authority.rs    one retained workspace directory capability and identity
src/resident_credit.rs        crate-private atomic pool/RAII leases for shared values
src/model/stream.rs           shared provider-neutral chunk grammar/validator
```

The durable schema must not introduce `session -> provider -> session` or
`session -> agent` cycles. `RequestPurpose` moves from `provider` to the
provider-neutral `model` layer and `provider` re-exports it for source
compatibility. Session owns `PreparedRetryPolicySnapshot`,
`PreparedCompactionCallSnapshot`, and `ModelVisibleDispatchSnapshot`; these use
only model/session primitives. The retry snapshot has its own canonical bounded
mode/max-retries/codes/backoff fields rather than importing provider types.
The neutral crate-private `resident_credit` module owns only an atomic bounded
pool and RAII lease; it imports no model/session/provider type. `model::Message`
may therefore carry a lease handle while Session creates the pool and charges
admission without creating a reverse dependency.
Agent, which already depends on both sides, converts the public getters of one
`PreparedProviderCall` into that durable snapshot before start and retains the
original prepared call for hot dispatch. `PreparedProviderCall` and
`ProviderBinding` remain provider-owned and never enter Session. A compile-time
module-boundary test plus cold-scan construction proves no Session module
imports `crate::provider` or `crate::agent`.

`ProviderRequestDraft`, `PreparedRequestPreflight`, and
`ProviderPreflightError` also remain in the provider boundary: Agent uses them
before input admission, while Session never imports them. The DeepSeek
implementation shares private pure preparation/encoding helpers between this
seam and `stream`; the public trait exposes neither DeepSeek JSON nor
credentials.

Phase 8 promotes the already locked and production-present `aws-lc-rs 1.18.0`
package (currently brought in by rustls/reqwest) to a direct, exact dependency
for audited SHA-256 digests. It does not add a home-grown hash. The pinned Rust
1.85 builds, licenses, and default-feature graph are rechecked in the repository
gates; if the actual dependency graph changes before implementation, this
decision is revisited rather than silently substituting a weak fingerprint.

`Session` continues to own append validation and projection. Its durable mode
owns a journal handle; the CLI never writes event rows directly. `AgentLoop`
calls a small async barrier/retirement seam and does not know file paths. TUI
continues to consume committed projections only and never opens the journal.

The intended internal API is:

```rust
TurnOutcome::turn_end_seq(&self) -> EventSeq
TurnOutcome::final_message(&self) -> Option<&Message>

ModelProvider::preflight_request(
    &self,
    draft: ProviderRequestDraft<'_>,
) -> Result<PreparedRequestPreflight, ProviderPreflightError>

SessionStore::open_default() -> Result<SessionStore, StoreError>
SessionStore::prepare_new(header) -> Result<Session, StoreError>
SessionStore::list(workspace) -> Result<Vec<SessionMeta>, StoreError>
SessionStore::begin_resume(id, workspace) -> Result<PreparingResume, StoreError>

PreparingResume::wait_ready(&mut self) -> impl Future<Output = Result<(), StoreError>>
PreparingResume::finish(self) -> Result<PreparedSession, StoreError>
PreparingResume::cancel_and_shutdown(&mut self) -> impl Future<Output = Result<(), StoreError>>

PreparedSession::recovery_report(&self) -> &RecoveryReport
PreparedSession::begin_commit(&mut self) -> Result<(), StoreError>
PreparedSession::wait_commit(&mut self) -> impl Future<Output = Result<(), StoreError>>
PreparedSession::finish_commit(self) -> Result<RecoveredSession, StoreError>
PreparedSession::cancel_and_shutdown(&mut self) -> impl Future<Output = Result<(), StoreError>>
RecoveredSession::into_parts(self) -> (Session, WorkspaceAuthority)

Session::materialize_if_needed(&mut self) -> impl Future<Output = Result<(), StoreError>>
Session::append(event) -> Result<AppendReceipt, AppendError>
Session::append_settled(event) -> impl Future<Output = Result<AppendReceipt, AppendError>>
SessionReservation::append_settled(&mut self, event) -> impl Future<Output = Result<AppendReceipt, AppendError>>
SessionReservation::settle_exact_settled(&mut self, claim) -> impl Future<Output = Result<AppendReceipt, AppendError>>
SessionReservation::flush_barrier(&mut self, reason) -> impl Future<Output = Result<(), SessionIoError>>
SessionReservation::begin_attempt(&mut self, turn, step) -> Result<AttemptToken, AppendError>
SessionReservation::append_attempt_chunk_settled(&mut self, &token, chunk) -> impl Future<Output = Result<AppendReceipt, AppendError>>
SessionReservation::seal_attempt(&mut self, &token) -> Result<PreparedAttempt, AppendError>
SessionReservation::append_attempt_closure_settled(
    &mut self,
    &token,
    disposition: AttemptDisposition,
    event,
) -> impl Future<Output = Result<AppendReceipt, AppendError>>
SessionReservation::settle_step_end_with_attempt_settled(
    &mut self,
    claim,
    token: Option<&AttemptToken>,
    disposition: Option<AttemptDisposition>,
) -> impl Future<Output = Result<AppendReceipt, AppendError>>
SessionReservation::retire_attempt(&mut self, &token) -> Result<(), AppendError>
SessionReservation::read_validated_surface_row(
    &mut self,
    locator,
    expected_node_revision,
    expected_full_digest,
    expected_identity_fingerprint,
) -> impl Future<Output = Result<ValidatedRawRow, SessionReadError>>
Session::has_unresolved_model_calls(&self) -> bool
Session::reader_from(&mut self, seq) -> impl Future<Output = Result<EventReader<'_>, SessionReadError>>
EventReader::read_page(&mut self, limits) -> impl Future<Output = Result<EventPage, SessionReadError>>
Session::shutdown(&mut self) -> impl Future<Output = Result<(), SessionIoError>>
```

Materialization, resume scan/repair, and the range reader are async because their blocking
file work is serialized through an owned journal thread; they are never blocking
calls on Tokio's current-thread runtime. A long operation is always installed in
a non-future owner before its wait begins. `PreparingResume` owns the new thread,
lock, command/ack flight, and workspace authority; a dropped `wait_ready` future
leaves all of them in `PreparingResume`, which can be polled again or explicitly
cancelled, settled, shut down, and joined. `PreparedSession::begin_commit`
similarly installs the commit flight in `PreparedSession` before
`wait_commit` borrows it. Once a mutating commit is enqueued, cancellation only
latches and `cancel_and_shutdown` settles it; it never detaches it. `finish` and
`finish_commit` are synchronous state extractions permitted only after the owned
operation acknowledged completion. `Session::shutdown` borrows the Session and
marks it closed on success, so dropping its wait future also leaves the writer
owned for another settlement attempt.

The PreparedSession retains the lock, writer, immutable plan, report, and exact
`WorkspaceAuthority` across the warning output gate. A `SessionReservation<'_>`
is an exclusive Rust `&mut Session`
borrow, not a held `Mutex`/`RwLock` guard; its async barrier therefore does not
violate the no-lock-across-`await` rule. The journal `flock` is lifetime
ownership held by its dedicated writer, not an async critical-section guard.

The concrete signatures may be adjusted to satisfy borrowing, but ownership
must remain: no detached writer, no journal access from the renderer, and no
tool effect before its barrier. The specialized surface-row read reborrows the
Session already held by `SessionReservation`; it settles/barriers the relevant
writer prefix, reads at most one 9 MiB row, validates the locator/revision/two
digests, and then returns control to the same Reservation with all turn/step/end
claims intact. No general `session_mut()` escape hatch is exposed, and the
ordinary history `EventReader` remains unavailable while a Reservation owns the
Session.

CLI startup order is fixed. Argument parsing, help/version, and keyless bounded
listing happen first. New mode performs its one synchronous workspace-capability
open before entering the current-thread runtime, forms the deferred
header/Session from that identity, then creates the runtime/signal streams,
attaches the fresh observer, and gives the same authority to the tool registry
while assembling the Agent.
`AgentLoop::run_turn`, not the input parser, owns first-use materialization at
the preflight boundary defined above.

Resume mode creates the runtime/signal streams and synchronously creates a
`PreparingResume` owner, then selects signals against its borrowed
`wait_ready`. That operation opens, verifies, and retains the workspace
authority through the owned journal thread. On success CLI calls `finish`, opens
only the bounded terminal/script diagnostic writer, and renders
`recovery_report()`. It then calls `begin_commit`, selects signals against the
borrowed `wait_commit`, and finally consumes `finish_commit`. A signal or output
failure before commit changes no journal byte and explicitly settles/shuts down
the still-owned operation. Commit returns the live Session and the same retained
authority; only after its barrier and live observer attachment may CLI
construct the tool registry, Provider, or read credentials. No resumed path is
reopened between validation and tool construction. A signal during an enqueued
commit latches the exit intent but still settles the owned writer before
releasing the lock. No 512 MiB scan, truncate, sync, or Drop join runs directly
on Tokio's current-thread executor during a normal production path.

## Failure, cancellation, and side effects

- First-prompt materialization uses `CREATE|EXCL` at the final canonical journal
  name, then takes the
  journal lock and writes and synchronizes the header and parent directory before
  any Provider or tool can run. Namespace visibility precedes durable validity;
  listing ignores a file without one complete valid header. Conditional cleanup
  uses the still-locked path identity rule above; a crash may leave a private
  zero-length or torn slot, which listing omits, counting still includes, and
  resume reports as corrupt rather than treating it as usable.
- Resume validates header and workspace authority before body recovery. Later
  errors occur before Provider construction and do not modify a committed prefix
  unless the operation is the documented locked tail repair.
- A failed pre-provider barrier means no request is sent.
- A failed pre-tool barrier means no tool body, approval-authorized patch, or
  Shell process starts.
- A result-barrier failure after a side effect poisons the Session; repair later
  records `TOOL_OUTCOME_UNKNOWN` instead of retrying.
- Ctrl+C stops new work, waits for Provider/tool/writer cleanup, flushes the
  aborted turn, and then returns to an interactive prompt.
- TERM/HUP/QUIT preserve their exit codes after cleanup and flush. An I/O error
  cannot downgrade a stronger terminating signal, though the diagnostic may be
  suppressed if the terminal is already broken.
- A compaction model call has no tool side effect. Its failure leaves the
  surface that was authoritative on entry to that summary call; already durable
  prune replacements remain authoritative and may still justify the single
  overflow replay through the generation-progress rule.
- Secrets owned by configuration or environment never enter rows or error text.
  User prompt/tool content is intentionally transcript data; Phase 8 cannot
  promise to recognize every secret a user voluntarily pastes.

## Deterministic upstream fixture

The Phase 8 design checkpoint records:

```text
scripts/typecheck-upstream-phase8-fixtures.mjs
scripts/generate-upstream-phase8-fixtures.ts
tests/fixtures/session/upstream_phase8_oracle.json
```

The generator refuses the wrong or dirty upstream checkout, uses fixed time and
stable normalized IDs, performs no network request, and is run twice with
byte-identical output. Default Rust tests consume only the committed fixture.

The fixture records compact facts rather than embedding thousands of events:

- header plus logical/physical JSONL layout and packed-chunk decode equality;
- scanner results for incomplete tail, bad/gapped suffix, committed corruption,
  and newer/older format refusal;
- unknown required refusal and unknown ignorable-envelope preservation;
- open-tail multi-tool repair order and `TOOL_NOT_STARTED` versus
  `TOOL_OUTCOME_UNKNOWN` provenance;
- resume/end-seed behavior;
- one successful basic-compaction bracket, exact surface replacement,
  summarization input, replay equality, token-pressure reduction, and unchanged
  old-event/chunk hash;
- tool-result-pruner Unicode/config boundaries, adjacent marker/replacement
  facts, metadata preservation, no-op replay, and source-derived orphan marker;
- more than 4,096 logical reasoning chunks followed by another successful
  append, plus unchanged chunk count/hash after compaction.

Physical Zstandard and chunk-row packing facts are recorded as upstream facts;
the Rust comparator normalizes them to decoded logical events and explicitly
asserts both the plain-JSONL intentional difference and zero-modification refusal
of a packed physical row as a Rust journal.

## Default-enabled verification plan

### Long-task regression

1. Three fake-provider turns each emit 1,400 reasoning deltas and a short final
   answer. All three complete; durable event count exceeds 4,096; all 4,200
   deltas remain stream-readable; sequences are continuous; resident state
   remains below its limits; resume completes a fourth turn.
2. A snake-like single turn runs four steps of about 850 reasoning deltas plus
   one harmless fake tool call, followed by a fifth long-reasoning final step.
   All four tools execute exactly once, the turn completes, and resume does not
   replay a tool.
3. A real `dsh` loopback SSE test performs several long reasoning turns and
   observes another prompt instead of `AGENT_EVENT_BUDGET`.
4. The same logical answer split into 1, 17, and 1,000 deltas yields the same
   final message, token pressure, and compaction range.

### Journal and recovery

- exact/one-over header, record, batch, active, event, file, and session-count
  limits, including protected closure bytes, identity count/bytes/allocation,
  surface encoded/charged/192 MiB transient bounds, provenance 4,094/4,095, the
  compaction close claim, and the 19 MiB body. Header
  cases distinguish the old raw-header 64 KiB cap from the complete tagged
  JSONL-line-with-LF 64 KiB cap;
- a near-512 MiB journal made from individually legal rows whose current
  surface crosses 24 MiB encoded or 64 MiB charged fails cold with
  `CLI_SESSION_LIMIT` and zero repair/truncation; hot exact/one-over and
  replacement old+candidate high-water use the identical charge function;
- durable recoverability exact/one-over cases for 1,024-byte call/approval IDs,
  256-byte tool names, 64 declared calls, and one pending approval; maximal
  synthetic templates prove the 1 MiB/68-event gap; durable attempt tests cover
  4,000/4,001 chunks, 10 MiB/one-over content, and source-span exact/one-over;
- durable approval asked with missing, stale, cross-call, or wrong-tool
  association is rejected live and reported corrupt cold; only the exact
  unresolved call/name pair can support a not-dispatched repair result;
- duplicate assistant-declared IDs (within one message or across a step),
  isolated/reordered/duplicate top-level intent, mismatched name or arguments,
  a second approval for one call, result-before-decision, and success/wrong-code
  result after reject/cancel/unavailable are atomic live rejection and cold
  corruption; canonical Agent call/approval/result traces remain unchanged;
- a 255 KiB pending batch plus a 2 KiB row returns `NeedsFlush` before clock/seq
  allocation; one large empty-batch row succeeds; an owned in-flight command
  prevents another append and remains charged across future cancellation;
- injected failure at every projection/vector/string/row-buffer/lease reserve
  returns with unchanged Session state and zero calls to a counting Clock;
  after the one successful clock call, canonical row encoding and delta install
  perform no allocation and expose no retryable error path;
- async durable append/claim settlement prepares a generated-ID 9 MiB event
  exactly once across flush/flight controls; payload-only bytes and complete
  JSONL row bytes have distinct exact-bound tests, and durable
  `remaining_budget` never reports the memory-mode lifetime caps;
- short write, write error, fsync error, rollback error, writer-thread panic, and
  shutdown join; no duplicate seq or detached worker;
- on a current-thread runtime, dropping the borrowed wait future for a 512 MiB
  `PreparingResume`, a repair commit, or Session shutdown leaves its non-future
  owner intact, lets an unrelated ticker advance, and permits explicit
  cancel/settle/shutdown plus lock release; no wait future uniquely owns a
  thread, fd, flock, command, or acknowledgement;
- cancelling a near-limit read-only scan is observed between bounded reads,
  changes no journal byte, and joins/releases its lock; cancelling after the
  first repair mutation instead latches the signal and waits for the durable
  commit outcome;
- every script/interactive exit disposition transfers the Session back to its
  owner; forced final-output exit is reachable only after the journal writer is
  joined and lock/fd closed, while recovery-warning output failure closes the
  unmodified prepared writer before exit;
- torn UTF-8/JSON/no-LF tails, zero-modification refusal of an upstream packed
  storage row, damage before a later committed turn end, and
  inspect-versus-load behavior. A bad row before a
  later durable `tool/call` with no `turn/end` is zero-modification corruption,
  not a truncatable suffix;
- open step/turn, every approval-decision/tool-start matrix row, isolated raw
  calls, closed dangling durable corruption, and interrupted compaction repair;
- power loss after every repair row, especially between synthetic cancelled
  decision/result and between `turn/end`/`session/end-seed`, resumes the exact
  missing suffix; a pending approval on the second call proves
  decisions-before-results and results-in-model-order; recovery is idempotent
  and never calls Provider, approval, or tools. Restarting after the first and
  middle result with a different clock and entropy source still recognizes the
  deterministic IDs/timestamps and appends only the missing suffix;
- unknown required versus `ignorable:true`, newer/older schema, current v0
  append after a legacy read;
- complete range reading above global sequence 4,096 with bounded memory.
- paginating a near-limit journal scans total bytes O(file size), not once per
  page; reader creation barriers pending facts, a held reader excludes append,
  and drop→append/rollback→new reader observes a new revision without accepting
  a stale or forged cursor;
- hot attempt retirement and cold scan rebuild the same assistant content,
  usage anchor, pressure, and compaction range. A durable tail containing
  `step/start`, partial reasoning/text chunks, and a usage chunk but no finish
  is repaired with `Interrupted`: the next turn has no fabricated assistant
  node or token anchor, retains the exact cumulative usage, releases the raw
  assembler, and matches a live cancellation of the same prefix. A caught
  `run_step` panic after begin/chunks and after sealed-raw transfer is closed by
  the outer reserved `step/end`, proving the token is Session-owned rather than
  lost with the panicking future;
- scanning/repairing a near-512 MiB synthetic journal stays within the stated
  projection/page limits and never calls `events()` or materializes full history.
- a 4,000-chunk attempt keeps per-chunk projection work O(1); an allocation/copy
  seam proves chunks never clone surface nodes or durable identity sets;
- durable append/claim settlement returns owned receipts, and a compiled
  long-stream test proves no borrowed event forces retired chunks to remain;
- maximum-message commit makes surface, `TurnOutcome`, and Provider request
  share one immutable inner; Arc/charge probes prove no second payload
  allocation occurs. Injected request handle-vector reserve failure happens
  before dispatch, `TurnOutcome` handle installation remains allocation-free,
  and shared owners keep the one physical charge live until their final handle
  drops;
- DeepSeek request encoding at the exact 8 MiB wire boundary uses only the
  charged flattening/output buffers, while one-over and injected reserve failure
  return before network dispatch; an allocation seam proves no complete
  `serde_json::Value` request tree or third payload copy is constructed;
- observer byte-credit exact/one-over and RAII-release tests cover a slow
  consumer plus repeated large retry attempts. A request/tool-call projection
  fault with CLI polling deliberately delayed remains sticky through the
  post-sync dispatch check, executes zero Provider/tool bodies, commits history,
  and converges through the single observer-fault cleanup path. An asked-event
  fault never invokes the approval provider and durably closes as unavailable.

### Durability and external effects

- fake Provider observes a durable cursor covering every model-visible input
  before dispatch;
- a compaction Provider call's complete model-visible dispatch snapshot can be
  reconstructed solely from the durable pre-dispatch start recipe plus
  referenced header/context/surface rows; a crash while the summary stream is
  pending leaves no unrecorded model input. The test deliberately gives the
  ordinary conversation header different temperature/stop/default fields from
  the prepared summary call, then compares hot and cold effective config raw
  JSON, adapter defaults, context window, retry policy, system/tools/messages,
  instruction Message ID/source/content, session ID, and route purpose exactly.
  A first-turn hard-limit case has no prior ordinary header yet reconstructs the
  same inline system/tools snapshot. The comparison explicitly excludes only
  the process-local Provider binding;
- fake read/patch/Shell tools observe durable call and approval facts before
  their bodies start; an injected call-barrier failure invokes neither
  `ToolExecutor::prepare` nor a body, and an asked/allowed barrier failure calls
  neither the approval provider nor the prepared commit/run respectively;
- quota or fsync failure before a side effect executes no body;
- a crash after intent but before result resumes as unknown and never repeats
  the side effect;
- an interrupted approval resumes as cancelled/not-dispatched, while a durable
  allow with no result resumes as unknown;
- assistant/result/turn closure is durable before next request, prompt, script
  stdout, or clean exit.

### Resume, listing, and filesystem safety

- real binary new → two turns → exit → `--resume` → next turn, with old context,
  turn numbering, and global seq preserved;
- new interactive `/exit`/EOF/signal before a prompt and a script rejected before
  prompt admission create no journal or canonical slot; a no-event invalid
  proposal also remains deferred; the first fully preflighted prompt
  materializes/header-syncs before any input claim or `turn/start`, and
  materialization failure leaves zero Session events with no memory-mode
  fallback;
- `TurnProposal::Reject` and `TurnProposal::Enter([])` both materialize and
  append the same balanced turn facts as memory mode; only CLI input rejected
  before `run_turn` remains a no-event deferred Session;
- headerless resume uses request reason `initial`; resume with a prior header
  uses `resume` even when an explicit model changes;
- `first_live_seq` equals the pre-marker seed length while the observer attaches
  at post-marker `next_seq`; reopening a marker-terminated seed adds nothing;
- script-created session appears in keyless `--list-sessions`; script resume
  continues it while stdout remains final-answer-only;
- listing touches no workspace tool, credential, terminal, runtime, or loopback
  endpoint and reads only bounded headers;
- stored workspace default, explicit same-directory identity, missing/different
  workspace with no body modification, non-UTF-8 canonical workspace,
  root/journal symlinks, hard links, owner/mode mismatch, traversal IDs,
  header/path ID mismatch, and serialized/restored process umask 000/077/0777;
- CLI resume accepts one exact canonical lower-case `session-<uuid-v4>` ID and
  rejects a bare UUID, doubled prefix, upper-case spelling, wrong RFC variant,
  non-v4 version, slash, dot segment, NUL, or malformed suffix before any path
  lookup;
- replacing/renaming the workspace path after resume validation but before the
  warning/repair/tool assembly cannot redirect tools: the exact retained
  authority is returned by commit and the registry performs no path reopen;
- at 127 slots, concurrent creators yield exactly slots 128 and one stable
  failure; root-lock timeout, repeated zero/torn `CREATE|EXCL` residues, the
  257th directory entry, and create/list visibility all fail or filter exactly
  as specified without exceeding either bound;
- after `CREATE|EXCL` makes a name visible, a test process that unexpectedly
  locks and stops on that empty inode cannot make the creator block while
  holding the store-root lock: self-lock returns busy, writes/unlinks nothing,
  releases the root lock, and leaves one counted residue;
- with the state base, product directory, or sessions directory initially
  absent, bootstrap creates each component with exact mode and synchronizes its
  parent before any Provider/tool dispatch; symlink/component swaps, two
  concurrent first materializers, and injected mkdir/open/fsync failures either
  converge on one validated root or fail before a journal header is admitted;
  creator A crashing after mkdir but before sync forces creator B's `EEXIST`
  path to synchronize that parent before it can succeed;
- platform syscall-order/error-injection tests prove every mkdir parent,
  newly chmodded directory, journal fd/header parent, rollback target, and
  conditional-unlink parent reaches Linux `fsync` or macOS `F_FULLFSYNC` in the
  required order; a chmod/sync error never permits Provider/tool dispatch;
- with umask `0777`, Linux repairs the captured `O_PATH` object through
  `fchmodat2(AT_EMPTY_PATH)` and macOS uses no-follow `chmodat`; a concurrent
  swap to a symlink targeting a sentinel directory fails without changing the
  sentinel mode. Linux `ENOSYS`/unsupported flags fail closed instead of falling
  back to path-following chmod;
- two writers: second fails quickly as busy; normal exit and SIGKILL release the
  lock; journals never interleave;
- listing order is creation time descending then ID ascending, includes busy
  valid journals, and excludes invalid headers without freeing their slots;
- interactive and script resumes visibly report truncation, orphan state, and
  unknown outcomes without printing corrupt bytes or tool arguments;
- the complete warning is emitted before the first truncate/repair mutation;
  warning-output failure changes zero bytes, and power loss after warning at
  truncate or every repair row remains safely resumable;
- a second fd that ignores the advisory lock and appends or same-length rewrites
  bytes during the warning gate makes commit revalidation fail with
  `CLI_SESSION_CHANGED` and zero repair/truncation;
- renaming the canonical journal away, unlinking it, or replacing its canonical
  name during the warning gate also fails the final root-capability path-identity
  check with `CLI_SESSION_CHANGED` and zero mutation of the held or replacement
  file;
- journal and diagnostics contain no fake API key, authorization header, proxy
  secret, approval challenge, or unrelated environment value.

### Compaction

- upstream oracle event order, source relations, surface replacement, replay,
  original-prefix hash, and event-count `+4`;
- 80% threshold and exact-below/exact/one-over pressure; ordinary 16% balanced
  tail versus overflow retain-target zero (still at least one balanced node);
- pressure excludes claimed input, while Rust's separate hard encoder preflight
  includes it; usage anchoring includes output and selects the max of usage and
  heuristic anchors;
- a control-heavy system prompt below the 4 MiB resident limit (including a
  large run of NUL characters) remains below generic retained-byte accounting
  but exceeds DeepSeek's exact 8 MiB escaped wire. Provider preflight returns
  too-large before `step/start`, runs the bounded hard-limit path or produces
  `AGENT_CONTEXT_LIMIT`, and the fake stream/network counters remain zero.
  Exact wire/one-over, prepared-default retained-size one-over, preflight
  preparation failure, and a final-generation mismatch are also fixed without
  moving ordinary prepare failures outside the eventual step;
- tool call/result boundary adjustment and no safe-range case;
- ranges use current-surface positions rather than numeric seq ordering; two
  successive replacements exercise a valid later range with numeric
  `start > end` and identical hot/replay/cold selection;
- tool-result pruning exact/one-over/default/config/Unicode/rich-block cases;
- raw pruner replacement preserves unknown `data`/message/error/meta fields but
  always receives a fresh canonical envelope; conflicting seq/time/op/source
  input and locator/text tampering commit no marker;
  adjacent prune/replacement provenance and token-price claim; original full
  result retention; multi-result surface order; second-pass no-op; marker-only
  failure and hard-crash recovery without a fabricated replacement. A source
  row with tiny text and large metadata/extensions proves resident nodes stay
  compact; resume-then-prune preserves unknown fields; locator/full-row/masked-
  identity or text-tamper mismatch commits no marker; a >256 KiB replacement
  proves the dedicated 10 MiB pair command and cancellation boundary. Two
  oversized results in one pass prove the first replacement does not stale the
  second candidate's node-local revision. A compile/behavior case holds the
  turn/step/end claims in one `SessionReservation`, awaits the specialized row
  read, and then settles those same claims without releasing or recreating the
  reservation;
- cancellation before a pruning pass performs zero row reads; cancellation in
  a row read stops at a 64 KiB boundary and commits no marker; cancellation
  after the first complete pair prevents the next candidate from starting,
  while a later uncancelled pass converges. The committed first pair remains
  adjacent and a cancellation observed inside its writer wait cannot split it;
- empty, image, tool-call, max-token, provider-error, timeout, cancellation,
  whitespace-only text, multi-text-plus-reasoning block preservation,
  nonshrinking summary, changed surface, and failed
  close; a summary failure is not retried, while one successful but insufficient
  pressure compaction permits exactly one more transaction;
- a pressure summary failure writes/barriers its error close and one sanitized
  warning, then sends the ordinary request and completes the step when hard
  encoding still fits; cancellation/observer/storage poison prevents that
  dispatch, and an unencodable virtual request closes the turn without opening
  the reserved step;
- pre-start config/range failure and post-second-transaction pressure
  exhaustion follow the same advisory-continue rule and event-count contract,
  with no fabricated start/end before a start actually committed;
- canonical context overflow compacts/retries at most once without consuming the
  normal network retry budget, delay, retry counter, or `llm/retry{,-started}`
  events while incrementing the total provider-attempt counter. Success/idle
  resets the one-shot gate. Cases cover prune-only/no-balanced-range,
  prune-progress followed by summary error, cancellation after prune, and
  marker-only/no-replacement; only durable replacement progress permits replay.
  An error-finish/usage chunk followed by prune-only replay produces two
  distinct attempt anchors, one more total provider attempt, unchanged normal
  retries, and identical hot/cold usage. Crash immediately before and after the
  marker/start closure never merges replay chunks or performs more than one
  replay; a non-context failure, unfinished response, or cross-turn/step token
  cannot select `ContextOverflow`;
- the start recipe's three trigger spellings, ordered selected
  sequences/positional endpoints, generation, header/context references,
  complete serialized prepared-call snapshot, exact inline system/tools,
  session ID/purpose, and exact canonical instruction Message
  ID/source/content plus prompt version obey record/quota limits and survive a
  start-only crash; reconstructed hot/cold
  `ModelVisibleDispatchSnapshot` values are exactly equal while the documented
  process binding alone is excluded;
- every summary dispatch consumes the same attempt and reported-output-token
  counters and remaining turn deadline as an ordinary request. Cases cover a
  23-attempt prefix followed by summary and ordinary-attempt exhaustion,
  reported summary token exact/one-over, per-request max-token rejection, and
  cancellation/deadline both before start and after a durable start;
- with 23 of 24 attempt slots used, a pressure check whose bounded selection is
  `None` consumes no slot, writes no bracket, and lets the hard-fitting ordinary
  request use the final slot;
- a committed surface below 80% whose claimed input crosses the message/byte
  encoder boundary records `hard-limit`, retains the 16% balanced tail, and
  performs at most two transactions before either admitting the input or
  closing the unstarted step claims without ever summarizing the claimed input;
- hard-limit message-count, byte, provenance, no-range, and exhausted-pass cases
  close the already-open turn with exact `AGENT_CONTEXT_LIMIT` text and no
  `step/start`; ordinary advisory no-range continues, while overflow without a
  durable replacement preserves the original `CONTEXT_WINDOW_EXCEEDED` fact;
- old chunks remain readable and journal bytes never decrease after compaction;
- crash at start, summary, replacement, end, and every cold-repair suffix,
  followed by deterministic resume.

### Repository gates

Before Phase 8 is complete, the exact tree must pass:

```console
cargo +1.85.0 fmt --all -- --check
cargo +1.85.0 check --all-targets --all-features --locked
cargo +1.85.0 test --all-targets --all-features --locked
cargo +1.85.0 clippy --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo +1.85.0 doc --no-deps --all-features --locked
./scripts/check-whitespace.sh
git diff --check
```

Release build/help/version, keyless list, new/resume script smoke, release
symbol/LLVM-IR test-seam scan, and exact upstream fixture regeneration are also
required. The default suites must run on local macOS and GitHub Ubuntu 24.04.
Only after independent review, validation/compatibility/README updates, a clean
commit, non-force push, and exact-commit green CI may Phase 8 become `complete`.

## Known system limits

- `SIGKILL`, power loss, failing hardware, and a kernel/filesystem that lies
  about `fsync` cannot be made clean by application code.
- Advisory locks and directory durability are claimed only on tested local
  macOS and Linux filesystems, not arbitrary NFS or cloud mounts.
- Linux root bootstrap requires `fchmodat2(AT_EMPTY_PATH)` when an extreme umask
  removed owner permission from a newly created product directory. Ubuntu 24.04
  is verified; an older kernel that lacks the syscall fails closed instead of
  using a symlink-following chmod fallback.
- Advisory locking coordinates cooperating `dsh` processes; it cannot prevent a
  hostile process running as the same user from continually modifying an owned
  file. The prepare-token rescan detects ordinary gate-window changes before
  mutation, but no claim is made against an attacker that races after that
  final validation.
- Persisted device/inode pairs are object identities within the tested mounted
  filesystem lifetime, not portable project IDs. A remount, restore, move, or
  copy may reject an otherwise familiar path rather than weakening authority.
- Session files contain user/model/tool transcript text. Permissions reduce
  local exposure but are not encryption.
- Automatic summaries are model output. The transaction proves event, replay,
  and size invariants; it cannot prove that natural-language summary content is
  factually perfect. Original events remain available for audit and recovery.
- A process killed after a side effect but before its result is durable cannot
  know the external outcome with certainty. Recovery records unknown and asks
  for verification rather than rerunning it.
