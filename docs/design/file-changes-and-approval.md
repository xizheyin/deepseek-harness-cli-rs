# Phase 5 file changes and approval design

This document records the implemented Phase 5 contract. The goal is one
auditable, model-callable file-change tool whose exact change can be
previewed, allowed, denied, or sent to a user for one-shot approval. A rejected,
cancelled, malformed, stale, or otherwise uncommitted change must leave the
target file unchanged.

## Scope and non-goals

Phase 5 adds:

- a combined workspace registry that retains the Phase 4 `list`, `glob`, `grep`,
  and `read` tools and adds one `apply_patch` tool;
- a strict, single-file unified-diff input for UTF-8 create or update operations;
- a side-effect-free preparation stage that computes the exact candidate content
  and complete approval diff;
- fixed `allow`, `deny`, and `ask` policy modes plus a cancellable approval-provider
  seam for the future terminal UI;
- paired, log-only `approval/asked` and `approval/decided` audit events;
- conflict detection immediately before an atomic create or replacement;
- bounded result metadata carrying the same canonical diff that was approved.

This phase does not add delete, rename, copy, mode-only or binary patches,
multi-file transactions, directory creation, shell commands, persistent approval
prompts, remembered grants, or a terminal prompt. The `dsh` executable remains a
help/version/argument-error shell; the real Phase 5 production path is the public Rust
Agent Loop assembled with the real workspace registry and an approval provider.
Interactive prompting belongs to Phase 7 and repair of crash tails belongs to
Phase 8.

One patch changes one file. There is no portable all-or-nothing transaction made
from several POSIX `rename` operations, so accepting a multi-file patch would make
the phase's atomic-failure promise false.

## Upstream behavior used as the reference

The semantic baseline is DeepSeek Harness commit
`47f943859bef60e4160492346772ded9b24f765a`.

The fixed upstream does not expose an `apply_patch` or unified-diff input tool.
Its relevant model-facing mutations are `write` (create or full replacement),
`edit` (literal replacement), and the separate `str_replace_editor`. Rust's patch
schema is therefore an intentional product difference. The compatible facts we
can compare are the resulting file content, guarded-create behavior, ordinary
stale/match failures, diff facts, approval audit ordering, and call/result
correlation.

The primary upstream evidence is:

- `packages/fs/tool-fs/src/{write,edit,diff,error,sandbox}.ts` and
  `packages/fs/tool-fs/tests/{tools,integration,diff,error}.spec.ts`;
- `packages/fs/tool-str-replace-editor/src/index.ts` and its tool tests;
- `packages/fs/fs/src/{index,types}.ts` and
  `packages/fs/fs-local/src/{index,fsio,win32}.ts` plus their tests;
- `packages/fs/fs-observation-policy/src/index.ts` and `tests/policy.spec.ts`;
- `packages/core/tools/src/index.ts`, its tool/invariant tests, and
  `packages/core/agent-loop/src/tool-calls.ts`;
- `packages/interaction/user-approval/src/{index,types,invariant}.ts` and its
  approval/invariant tests;
- `packages/interaction/permission-presets/src/index.ts` and
  `packages/sandbox/sandbox/src/escalation.ts`.

Upstream has three distinct decision layers:

```text
pre-tool decision       allow | deny | ask
session ask policy      ask | never
one request outcome     allowed-once | rejected | cancelled | unavailable
```

`ask` does not mean that every normal workspace write prompts. Official ordinary
workspace writes are normally allowed; an explicit pre-tool rule or sandbox
escalation produces the ask. Rust deliberately defaults every `apply_patch`
preparation to ask because the initial terminal product has no mature policy-rule
engine. Embedders may explicitly select allow or deny. This safer default changes
ordinary write behavior and will be recorded as an intentional difference.

Upstream writes a private sibling staging file, synchronizes it, preserves the
ordinary mode of an existing target, and publishes by rename (or the corresponding
Windows operation). Guarded create uses a no-replace hard link. The upstream
version check and final replacement are not one cross-process atomic CAS: an
external writer in that last window can still be overwritten. Rust must not claim
more than portable filesystems can provide.

## Public ownership and assembly

The public concepts are intended to have this shape:

```rust,ignore
pub enum FileChangePolicy { Allow, Deny, Ask }
pub enum ApprovalOutcome { AllowedOnce, Rejected, Cancelled, Unavailable }

pub trait ApprovalProvider: Send + Sync {
    fn request(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalFuture<'_>;
}

pub struct WorkspaceToolRegistry { /* one workspace capability + five schemas */ }

impl WorkspaceToolRegistry {
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, ToolRegistryBuildError>;
    pub fn schemas(&self) -> &[ToolSchema];
    pub fn workspace(&self) -> &Path;
}
```

`AgentLoopConfig` owns the fixed file-change policy and an `Arc<dyn
ApprovalProvider>`. Its default is `Ask` with a fail-closed provider that returns
`Unavailable`; therefore simply exposing `apply_patch` without wiring a UI cannot
write. The existing constructors remain useful for read-only/fake tools.

The canonical assembly takes schemas and execution from the same registry:

```rust,ignore
let tools = Arc::new(WorkspaceToolRegistry::open(workspace)?);
let config = AgentLoopConfig::new(call)
    .with_tools(tools.schemas().to_vec())?
    .with_file_change_approval(FileChangePolicy::Ask, approval_provider);
let agent = AgentLoop::new(session, provider, tools, config)?;
```

The Agent and Session own policy resolution and approval audit events. A file
tool owns patch parsing, the immutable prepared change, and filesystem commit.
The approval provider receives one owned, read-only request value; it cannot
mutate the session or receive filesystem authority. The terminal UI will
implement this provider later.

All these boundaries are trusted in-process Rust traits, not a sandbox for
hostile native plugins. Synchronous factory methods must return promptly; futures
must cooperate with cancellation and own all work they start. `Debug` output may
show IDs, counts, operation kind, path-byte count, and diff-byte count, but never
the patch, file content, approval reason, or diff body.

## Two-stage tool protocol

The current one-stage `ToolExecutor::execute` remains the compatibility method
for ordinary tools. Phase 5 adds a defaulted preparation method whose ordinary
implementation wraps that existing execution as `Complete`. A mutation registry
can instead return an opaque prepared mutation:

```rust,ignore
prepare(request, cancellation)
    -> Complete(ToolExecutionResult)
     | Mutation(PreparedToolMutation)

PreparedToolMutation
    = bounded approval prompt + one owned, single-use commit capability
```

The preparation future may borrow its executor, but `PreparedToolMutation` is
fully owned and `'static`. Its commit capability is a consumed `FnOnce` that
captures only owned baseline/candidate facts and an `Arc` to registry state. The
Agent runs that synchronous closure with `spawn_blocking`; it returns a bounded
`ToolExecutionResult` plus `Committed` or `NotCommitted`. `ApprovalProvider`
returns `Result<ApprovalOutcome, ApprovalProviderError>` so provider failure or
panic can be normalized to durable `unavailable` without retaining extension
error text.

Calling the legacy one-stage method directly for `apply_patch` fails closed. It
cannot bypass the Agent-owned approval stage. Read-only registries and all Phase 3
test executors continue through the default `Complete` path.

One mutation follows this order:

```text
assistant/message
tool/call                         durable intention already committed
prepare                           read-only: parse, read, apply, render
  ├─ malformed/conflict/no-op     bounded ordinary tool result
  └─ prepared mutation
       ├─ allow                   no approval audit pair
       ├─ deny                    no provider call and no commit
       └─ ask
            approval/asked
            wait for provider, policy, or cancellation
            approval/decided
       commit only if allowed
       tool/result                same diff in bounded metadata
step/end
```

`prepare` is part of tool work and remains under the configured tool timeout.
Waiting for a human answer is bounded by the turn deadline, not the ordinary
30-second tool-body timeout. An allowed commit receives a fresh tool-work budget.
This separation prevents a user from losing an approval merely because reading
the prompt took longer than a filesystem operation.

The prepared commit is single-use. It owns the exact baseline and candidate; the
approval is not a reusable path permission and cannot be applied to newly read
content.

## Patch input and exact preview

The model-facing schema is a closed object:

```text
apply_patch { patch: string }
```

The string is one ordinary textual unified diff. It must contain exactly one
`---`/`+++` file header pair and at least one hunk. Supported operations are:

- create: headers are exactly `--- /dev/null` and `+++ b/<relative-path>`;
- update: headers are exactly `--- a/<relative-path>` and
  `+++ b/<the-same-relative-path>`.

Headers with timestamps, quoted paths, omitted `a/` or `b/` prefixes, or extra
header text are rejected. A `\ No newline at end of file` marker is accepted
only where the unified-diff grammar permits it, and that side may not contain a
later explicit hunk line or an implicit unchanged tail. The canonical preview
uses the same headers, three context lines per hunk, the standard
no-final-newline marker when needed, and exactly one final newline. These
deliberately narrow rules keep the model schema and byte-for-byte approval tests
stable rather than silently accepting every dialect implemented by a dependency.

The parser rejects multiple files, deletion, rename, copy, binary data, NUL,
mode-only changes, absolute paths, parent traversal, symlink components, malformed
hunks, trailing second patches, and no-op results. Parent directories must already
exist. The patch and every path are bounded before parsing; hunk count, total line
count, single-line length, baseline/candidate bytes, and generated diff bytes have
fixed ceilings.

Patch application uses the parsed hunks against the complete current UTF-8 text.
All context/removal lines must match at the hunk's declared old-file position;
offset searching and fuzzy application are rejected, and no partially applied
hunk is retained. CRLF files are compared and diffed in an LF-normalized
in-memory form and written back using their detected line-ending style, matching
the relevant upstream edit semantics.

The approval preview is not the untrusted input string. After exact-position
application, a bounded linear canonicalizer rebuilds the complete unified diff
from the verified change, removes redundant unchanged edit runs, and includes
three lines of surrounding context. If the complete diff cannot fit its
approval/session budget, preparation returns
`DIFF_TOO_LARGE`; it never asks a user to approve an ellipsis. The very same diff
value is retained by the commit plan and placed in successful result metadata.
Tests compare approval bytes, metadata bytes, and a fresh post-write diff.

A no-op patch returns `NO_CHANGES`, asks nobody, creates no staging entry, and does
not touch the target.

## Approval service and durable audit

For `Ask`, the Agent creates one fresh opaque approval ID and atomically reserves
space for both audit events before appending either:

```text
approval/asked {
  id,
  toolName,
  callId?,
  reason?
}

approval/decided {
  id,
  outcome: allowed-once | rejected | cancelled | unavailable
}
```

The decision claim reserves the longest allowed outcome representation. After the
provider answers it is rebound and settled with that exact outcome; it may not
silently substitute a different answer. If the pair cannot be reserved, `asked`
is not written, the provider is not called, and the existing tool-result claim
closes the call as `APPROVAL_UNAVAILABLE`.

The arguments and diff are not duplicated into the durable ask event: the prior
`tool/call` contains the arguments, while the correlated `tool/result` for every
prepared mutation outcome contains the canonical diff in bounded metadata. The
live `ApprovalRequest` carries the complete preview to the UI. Both approval
events are log-only and never enter `Session::messages()`.

The optional human-readable reason is omitted when adding its fixed wording to a
maximum-length path would exceed the 4 KiB reason limit. The complete path and
canonical diff remain present in the bounded preview and result metadata, so this
does not hide the proposed change or turn a valid maximum-length path into an
infrastructure failure.

Only `AllowedOnce` commits. `Rejected`, `Cancelled`, `Unavailable`, a missing
provider, provider error, provider panic, invalid resource input, or inability to
reserve/commit the audit pair all fail closed. A late answer after cancellation is
discarded. `Deny` never calls a provider. `Allow` and `Deny` do not manufacture an
ask/decided pair because no question was asked.

An asked event requires an open turn, and a decided event requires one matching
unresolved ID. IDs, tool names, call IDs, reasons, and outcome vocabulary are
bounded and validated on live append and JSON replay. A normal request always
settles the pair. A forced future drop, process crash, or session-clock failure can
leave an unmatched asked event; the same Agent becomes poisoned and reconstruction
fails closed until Phase 8 appends repair evidence. It does not silently reuse the
answer or execute the patch.

The static Phase 5 `FileChangePolicy` is application configuration, not a durable
session switch. Upstream `approval/policy` last-write-wins events and live policy
switch notices remain partial until the Phase 7 command/UI and Phase 8 resume path
can consume them honestly.

## Workspace and conflict rules

Preparation and commit reuse the already-opened Phase 4 workspace capability.
No code converts a display path back into ambient filesystem authority.

The mutation implementation is available on Unix targets in Phase 5. Local
acceptance runs on macOS, and the Phase checkpoint still requires green Ubuntu
CI before completion. The read-only Phase 4 registry remains the portable
fallback; Windows mutation publication is deferred rather than being approximated
with weaker path or replacement rules.

Mutation paths are stricter than reads:

1. only a normalized workspace-relative target is accepted;
2. every existing path component, including the final component, must be a real
   directory/regular file rather than a symlink;
3. the parent directory is opened as a capability and retained by the prepared
   plan, narrowing later rename races;
4. existing targets must be regular UTF-8 files without NUL and have one hard
   link; a target with `nlink > 1` is rejected;
5. create requires absence at preparation and again at no-replace publication;
6. update records the opened file identity, ordinary mode, exact normalized
   content, and raw bytes. Commit rechecks all those facts before staging and
   again immediately before publication.

The registry serializes commits made through that registry. Two prepared updates
from the same baseline cannot both commit: after one replaces the target, the
other recheck returns `FILE_CONFLICT`. External changes detected before either
recheck also preserve the external version.

This is still not an absolute cross-process CAS. On portable macOS/Linux APIs, an
uncooperative writer can change the path after the last check but before `rename`.
Rust narrows this window with a retained parent handle and late checks and never
writes an existing inode in place, but does not claim linearizability against an
adversarial external process. The fixed upstream has the same class of window and
an earlier final check. Tests cover deterministic changes before both Rust checks,
not an impossible universal scheduling proof.

Rejecting all mutation symlinks and multi-link targets is stricter than upstream.
It prevents a repository-controlled alias from redirecting or confusing a write,
at the cost of requiring users to edit the real in-workspace path.

The existing Agent reserves every eventual call/result pair before publishing an
assistant message. Approval events make later event sequences dynamic, so a
result claim reserves a worst-case byte ceiling and is rebound to the real
`tool/call` sequence immediately after that call commits. Rebinding cannot exceed
the protected ceiling. This preserves the serial `call -> optional approval pair
-> result` order and exact result provenance without guessing future sequences.

## Atomic publication and commit point

Commit is one blocking, registry-serialized operation over capability-relative
handles; no synchronous filesystem call runs on an async Tokio worker.

For both create and update:

1. create an owner-only sibling staging directory (`0700`) with a random,
   exclusively created name;
2. create one staging file exclusively at `0600`;
3. write the full candidate, set its final ordinary mode, and `sync_all` it;
4. check cancellation and revalidate the target one last time;
5. publish create with hard-link no-replace, or update with same-directory rename;
6. remove only the known private staging file and directory, never recursively;
7. synchronize the parent directory, persisting both publication and cleanup.

Create mode is `0600`. Update preserves only ordinary `0o777` permission bits and
strips setuid/setgid. Owner, group, ACLs, extended attributes, flags, resource
forks, and hard-link topology are not preserved; update creates a new inode. That
metadata limitation is explicit rather than hidden behind a claim to preserve the
whole file.

The successful link/rename is the commit point. Before it, cancellation or any
error performs best-effort cleanup and returns an uncommitted result; the target
remains absent or retains its exact old content and ordinary mode. If that
cleanup itself fails, the result stays `committed: false` and carries a bounded
cleanup warning instead of hiding the residue. After publication:

- cancellation cannot rewrite the fact as `ABORTED`;
- parent-sync failure is `FILE_COMMITTED_DURABILITY_UNCERTAIN`, not an ordinary
  uncommitted failure;
- cleanup failure is reported as committed with a bounded warning;
- the Agent must persist a committed result even when the turn then closes as
  cancelled.

Before commit begins, the Agent also protects enough result capacity for the
largest bounded committed outcome. Once the file has changed it uses a
preferred-only settlement: it must never substitute the ordinary
`TOOL_OUTPUT_BUDGET_EXCEEDED` fallback that would falsely omit `committed: true`.
Session-clock failure or a process crash after publication can still leave an
unresolved durable tail; the same Agent is poisoned and Phase 8 must repair it.
Capacity reservation cannot make a fallible clock infallible.

The commit future therefore returns both a normalized result and a disposition
(`Committed` or `NotCommitted`). Cancellation is checked before the blocking
commit starts and between its bounded filesystem steps. Once the blocking commit
has started, the Agent keeps awaiting it until it establishes one of those two
facts; it never drops the worker and guesses `ABORTED`. A committed outcome wins
over cancellation, while a proven pre-commit cancellation remains uncommitted.

The configured tool timeout becomes a cancellation request once commit has
started, not permission to abandon the blocking worker. If the worker proves
that publication did not happen, the result is normalized to `TOOL_TIMEOUT`; if
publication did happen, the committed result wins. Caller or turn cancellation
still closes the surrounding turn as aborted/timed out after that file fact is
durably recorded.

A kernel call stuck in a broken FUSE/network filesystem cannot be portably killed.
That can delay `run_turn` beyond its normal timeout once commit has begun. This is
the same disclosed boundary as Phase 4 and is not described as hard cancellation
or process isolation. Phase 5 deliberately chooses truthful file/audit facts over
returning promptly with an outcome it cannot know.

## Errors and model-visible results

Expected input, policy, filesystem, conflict, and approval outcomes are ordinary
correlated tool results. Representative stable codes are:

```text
INVALID_PATCH
UNSUPPORTED_PATCH
PATCH_TOO_LARGE
DIFF_TOO_LARGE
NO_CHANGES
FILE_NOT_FOUND
FILE_ALREADY_EXISTS
FILE_NOT_TEXT
FILE_NOT_REGULAR
FILE_HARDLINK_DENIED
FILE_TOO_LARGE
FILE_CONFLICT
WORKSPACE_PATH_DENIED
FS_NOT_FOUND
FS_PERMISSION_DENIED
FS_IO_ERROR
POLICY_DENIED
APPROVAL_REJECTED
APPROVAL_CANCELLED
APPROVAL_UNAVAILABLE
ABORTED
ABORTED_BEFORE_DISPATCH
TOOL_TIMEOUT
FILE_COMMITTED_DURABILITY_UNCERTAIN
FILE_COMMITTED_CLEANUP_WARNING
```

Messages are stable, bounded, workspace-relative, and never include the raw patch,
file body, absolute root, provider error, panic payload, or raw OS error. A normal
success states whether the file was created or updated. Its bounded metadata
contains the normalized relative path, operation, canonical approved diff, and
`committed: true`. That metadata is durable but not model-visible under the current
Phase 3 surface projection; the text result remains model-visible.

Internal inability to maintain call/result/audit truth remains an infrastructure
error and poisons rather than returning a misleading model result.

## Resource limits

The initial fixed ceilings are intentionally stricter than the unbounded upstream
write/edit inputs:

- patch argument: at most 256 KiB UTF-8;
- exactly one file and at most 1,024 hunks;
- at most 100,000 patch lines and 64 KiB per patch line;
- existing and candidate file: each at most 16 MiB, 100,000 lines, and 1 MiB
  per file line;
- complete canonical approval diff: at most 64 KiB compact JSON/text budget;
- any prepared mutation result event: at most 128 KiB;
- approval reason: at most 4 KiB;
- one approval pair per tool call;
- existing Agent call/result/session/turn budgets remain authoritative.

All count and byte arithmetic is checked or saturating before allocation. Parsing
does not trust hunk counts for capacity. Preparation checks cancellation while
reading and applying bounded data; staging writes in bounded chunks and checks
cancellation before starting the next chunk. No rejection path stores the full
patch in an error or `Debug` output.

These guarantees cover the built-in `WorkspaceToolRegistry`. `ToolExecutor` is a
trusted native extension seam: its default `prepare -> execute -> Complete`
adapter cannot prove that an arbitrary third-party executor has no hidden side
effects. Such implementations must obey the trait contract and are not treated as
an untrusted plugin sandbox.

## Deterministic verification plan

Default tests use fresh temporary directories, fake providers, fixed IDs/clocks,
and in-memory approval providers. They never inspect the user's project, use an
API key, or access the public network.

The minimum matrix is:

- canonical create and update; LF and CRLF; exact preview, result metadata, and
  post-write diff equality;
- no-op, malformed hunk, wrong context, multiple files, delete/rename/binary/mode,
  NUL/invalid UTF-8, missing parent, and every size/count limit at exact/+1;
- `Allow`, `Deny`, `Ask+AllowedOnce`, `Rejected`, `Cancelled`, `Unavailable`, no
  provider, provider error/panic, late answer, and distinct IDs for two asks;
- exact durable order `assistant/message → tool/call → asked → decided → result →
  step/end`, pair correlation, source-event provenance, and next-request replay;
- capacity reservation failure before ask, cancellation while waiting, before
  staging, during staging, immediately before publication, and after commit;
- external change before approval, during approval, and immediately before the
  late recheck; two plans from one baseline; create publication race;
- parent/sibling/absolute paths; intermediate/final internal/external/broken/cycle
  symlinks; directory/special file; hard-link denial with the other alias intact;
- create mode `0600`, update preservation of `0640`/`0755`, stripping special
  bits, full old-or-new visibility to concurrent readers, file/dir sync failure,
  cleanup residue handling, and zero target writes for every precommit failure;
- reconstruction with a paired approval log, rejection of malformed/unpaired
  decisions, and fail-closed reconstruction of an unmatched asked tail.

A tracked, type-checked upstream oracle covers official write create,
read-then-write, unique edit, replace-all diff, not-observed/stale conflicts,
approval outcomes, and core event ordering. Rust tests consume that fixture and
compare normalized final content plus the approval audit sequence. Paired
intentional-difference assertions retain the facts that upstream has no patch
schema, normally allows workspace writes without asking, can overwrite a
last-window external update, follows different path/link rules, and has no
equivalent hard resource ceilings.

Phase 5 is complete only after the oracle is reproducible, the public real-registry
Agent path and all failure/cancellation/safety tests pass on Rust 1.85, repository
verification is green, compatibility and validation records are accurate, and an
independent review finds no data-loss or audit blocker.
