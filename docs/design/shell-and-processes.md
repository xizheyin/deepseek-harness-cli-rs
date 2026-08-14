# Phase 6 shell and process-lifecycle design

This document fixes the Phase 6 contract before production implementation.
The goal is one foreground-only `bash` tool that is bounded and approval-gated.
While the documented host ownership and observer contract remains valid, its
normal settled path does not publish a tool result after cancellation or timeout
until no executable descendant is observed in the session/process group created
for the command. If the host steals the wait status, changes the signal contract,
or invalidates the observer, the call instead returns unresolved
`StartedOwnershipLost`; that state does not claim that descendants stopped and
prevents the Session from being reused as if the call had settled normally.

## Scope and non-goals

Phase 6 adds:

- a macOS/Linux-only local registry that retains the Phase 5 workspace tools and
  adds one model-facing `bash` schema;
- one fresh `/bin/bash -c` session and process group per allowed call;
- fixed `allow`, `deny`, and `ask` shell policy, using the existing approval
  provider and durable asked/decided events;
- a capability-held, workspace-rooted working directory and an allowlisted child
  environment;
- independently bounded stdout and stderr tails plus stable exit/timeout markers;
- normal settled-path handling for caller cancellation, command timeout, Agent
  tool timeout, and the turn deadline that terminates the created process group,
  waits for the direct child and pipes, and observes that no executable member
  is known to remain; host-contract failure instead produces the explicit
  unresolved ownership-loss state above;
- real-process tests for direct children and TERM-trapping same-group descendants
  on the Unix platforms the phase claims.

This phase does not add background jobs, job handles, PTYs, persistent shells,
interactive stdin, a command sandbox, network confinement, shell-history state,
other Unix/Windows process trees, or the terminal user interface. Phase 7 will
connect the current-turn cancellation token to Ctrl+C. Phase 6 proves that the
public Agent path propagates that token correctly, but does not claim that today's `dsh`
binary is already interactive.

An approved shell command is arbitrary native code running as the current user.
Restricting its initial working directory is not an operating-system sandbox:
the command can use absolute paths, change directory, access the network, and
modify anything the user account can modify. The safe default is therefore
`Ask`; without a real approval provider the call fails closed and no process is
started.

The absolute executable is the platform's `/bin/bash`. macOS commonly ships
Bash 3.2 while Ubuntu ships a newer version, so only shared shell syntax is used
in cross-platform acceptance tests; the product does not promise that optional
Bash-version extensions behave identically.

## Upstream reference and deliberate differences

The semantic baseline is DeepSeek Harness commit
`47f943859bef60e4160492346772ded9b24f765a`. The inspected source and test paths
are recorded in `docs/upstream.md`.

The shared upstream/Rust foreground shape is:

- a required `command` and `description`, optional timeout and workdir;
- one fresh `bash -c` invocation;
- stdout first, then a marked stderr section;
- silent output rendered as `(no output)`;
- nonzero exit reported as an ordinary result with `[exit code: N]`;
- timeout reported independently of the eventual signal/exit status;
- caller cancellation closes the correlated call/turn at a high level;
- process-group TERM-to-KILL escalation, not direct-child-only signalling.

This is not a broad compatibility claim. The exact cancellation envelope and the
point at which group quiescence is awaited deliberately differ and remain in the
separate planned compatibility rows below.

Rust intentionally differs in these areas:

1. The fixed upstream exposes background jobs by default. Background work is
   deferred beyond this project's v0.1, so Rust advertises no
   `run_in_background` field and rejects injected extra fields.
2. Upstream ordinary shell calls are normally allowed unless another policy or
   sandbox escalation asks. Rust defaults every shell action to `Ask`; explicit
   `Allow` is used for the narrow canonical comparison.
3. Upstream accepts absolute workdirs and does not make the read-side workspace
   boundary a shell sandbox. Rust accepts only an existing directory whose
   capability-relative components remain inside the configured workspace and
   contain no symlink. The command itself remains unsandboxed after startup.
4. Upstream keeps 64,000 bytes per stream and may write a complete private spill
   file up to 64 MiB. Rust keeps byte-bounded in-memory tails only, stops after
   8 MiB of observed combined output, and never writes command output to an
   implicit spill file. Upstream success `value` retains structured stdout and
   stderr text; Rust persists the model-visible `ContentBlock` once and keeps
   only bounded truncation/cleanup facts in metadata, so Session consumers do
   not receive a duplicate per-stream structured transcript.
5. The upstream library defaults to 120 seconds, while its shipped base profile
   overrides that to 60 seconds; both cap at 600 seconds and use a three-second
   TERM-to-KILL grace. Rust uses the smaller limits below and also obeys the
   Agent's independently configured tool and turn deadlines.
6. Upstream starts from most of the parent environment, removes
   credential-shaped/ambient `DSH_*` names, and lets trusted plugins add managed
   values. Rust clears the environment and copies only a fixed small allowlist;
   commands that rely on proxy, loader, agent-socket, or other ambient variables
   behave differently.
7. Upstream's detached POSIX child is already a new session and process-group
   leader, but foreground `run()` waits only for the direct handle result; full
   group waiting is owned by later service disposal. Rust preserves that
   session/group shape and settles every foreground call only after its stricter
   same-group cleanup obligation has finished.
8. Upstream's optional filesystem sandbox/escalation layers do not exist here.
   Rust's capability-held initial CWD is not a command sandbox, and no sandbox
   metadata is invented.
9. Upstream parameter objects are open in this composition. Rust uses a closed
   typed object and fixed byte/count limits before approval or spawn.
10. Upstream resolves the executable name `bash` through `PATH`, passes only
    `-c`, and can therefore inherit startup hooks such as `BASH_ENV`. Rust fixes
    the executable path as `/bin/bash`, clears those hooks, passes
    `--noprofile --norc -c`, and sets argv[0] to `bash` so the ordinary `$0`
    observation remains compatible. A changed `PATH`, startup hook, or shell
    wrapper therefore has no effect on the Rust command.
11. Upstream keeps aborted subprocess facts internally but the model-facing tool
    throws its standard abort error instead of returning the `ShellRunResult`.
    Rust also closes the turn with its standard abort vocabulary. After a
    successful spawn that reaches `StartedAndQuiescent`, its correlated durable
    result retains bounded `started: true` and cleanup/quiescence facts. If
    ownership is lost, there is deliberately no fabricated result: the call
    stays unresolved and poisons reuse while the independently latched outer
    turn stop is preserved. The envelopes are intentionally different even
    though neither path starts another model step.
12. Upstream creates its command deadline immediately before calling the
    subprocess service's synchronous spawn. Rust starts `timeoutMs` only after
    `std::process::Command::spawn` succeeds, because only that boundary proves a
    process exists and yields an owned PID/group guard. The narrow canonical
    comparison uses commands whose spawn finishes far before the same explicit
    timeout; it does not claim that a slow or blocked spawn consumes the same
    timeout budget.

These differences change legal long-running or background workflows, not only
hostile inputs. They must remain separate `intentional-difference` rows rather
than being hidden inside a broad compatibility claim.

## Public ownership and assembly

The intended public shape is:

```rust,ignore
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellPolicy { Allow, Deny, Ask }

pub struct LocalToolRegistry { /* read, search, patch, and bash */ }

impl LocalToolRegistry {
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, ToolRegistryBuildError>;
    pub fn schemas(&self) -> &[ToolSchema];
    pub fn workspace(&self) -> &Path;
}

impl AgentLoopConfig {
    pub fn with_approval_provider(
        self,
        provider: Arc<dyn ApprovalProvider>,
    ) -> Self;
    pub fn with_file_change_policy(self, policy: FileChangePolicy) -> Self;
    pub fn with_shell_policy(self, policy: ShellPolicy) -> Self;
}
```

This type and its re-export are compiled only for macOS and Linux. Existing
read-only types keep their wider platform surface; Phase 6 does not make an
untested process-lifecycle promise on another operating system.
`LocalToolRegistry::open` also validates the platform observer and returns a
redacted `ToolRegistryBuildError::UnsupportedProcessObserver` before exposing a
`bash` schema when `/proc`, `libproc`, or the platform process ceiling cannot
satisfy the contract below. Because the child environment is captured at this
same construction boundary, a non-Unicode allowlisted value or an oversized
snapshot is instead a redacted `ToolRegistryBuildError::InvalidEnvironment` or
`ToolRegistryBuildError::EnvironmentTooLarge`; it is not a later model-facing
tool-call error.

`ReadOnlyToolRegistry` and `WorkspaceToolRegistry` keep their existing smaller
authority and schema sets. Adding `bash` to either existing type would silently
grant process execution to an embedder that selected a file-only registry, so
the broader `LocalToolRegistry` is a separate explicit choice. Its `open` call
opens the workspace capability exactly once, stores one `Arc<Workspace>`, and
constructs the read, search, patch, and shell components from that same object.
It does not compose two independently reopened registries: after a workspace
path rename, every tool must still refer to the same retained root inode.

An embedder must poll a shell Action inside a Tokio runtime with the I/O driver
enabled (`Builder::enable_io` or `enable_all`); enabling the Cargo `net` feature
only makes `AsyncFd` available at compile time. The pre-spawn reactor preflight
fails closed with `SHELL_ASYNC_RUNTIME_UNAVAILABLE` before any command when this
runtime prerequisite is missing. The public API example and Phase 6 validation
show `enable_all` explicitly.

`LocalToolRegistry::open` is deliberately synchronous startup work: it opens the
workspace, snapshots the environment, and may inspect a large platform process
table. It must not be called directly from an async runtime worker. The canonical
assembly performs it before entering Tokio. An already-async embedder may instead
own one `spawn_blocking` JoinHandle and await it to completion; dropping that
handle on cancellation would merely abandon the still-running startup work and is
not supported.

`AgentLoopConfig` owns `ShellPolicy`, just as it owns `FileChangePolicy`, and
uses the same `ApprovalProvider` seam. The canonical future assembly is:

```rust,ignore
let tools = Arc::new(LocalToolRegistry::open(workspace)?);
let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()?;
runtime.block_on(async move {
    let config = AgentLoopConfig::new(call)
        .with_tools(tools.schemas().to_vec())?
        .with_approval_provider(approval_provider)
        .with_file_change_policy(FileChangePolicy::Ask)
        .with_shell_policy(ShellPolicy::Ask);
    let mut agent = AgentLoop::new(session, provider, tools, config)?;
    agent.run_turn(proposal, cancellation).await
})?;
```

There is one approval provider and two independent policies. The existing
`with_file_change_approval(policy, provider)` convenience remains compatible and
sets the shared provider plus the file policy; the new explicit setters avoid a
later shell configuration silently replacing the provider chosen for file
changes. Whichever explicit provider setter is called last is intentionally the
one shared provider used by both domains.

The registry owns parsing, workdir/environment resolution, output normalization,
and the process runner. The Agent owns policy, durable approval events, event
capacity, interruption precedence, and call/result correlation. The terminal UI
will own the human decision and OS signal source later.

`ToolExecutor::execute` remains a public legacy/direct seam, so the local
registry must not let it bypass policy. Direct `execute` of `bash` returns the
bounded `APPROVAL_REQUIRED` result and never spawns, exactly as direct
`apply_patch` does. The only production route to a shell process is
`prepare -> Action -> Agent policy/approval -> sealed run`. A direct-registry
test uses a sentinel command to prove that rejection has no process or file side
effect.

## Why shell needs a distinct prepared action

Ordinary Phase 3 tools receive one second of cooperative cleanup after
cancellation, after which their future is dropped. That rule is appropriate for
bounded in-process helpers, but it cannot safely own a subprocess: dropping a
future does not terminate a process group or wait for a TERM-trapping
descendant.

Phase 6 therefore adds a sealed, fully owned foreground action. The public enum
must name the payload, but only this crate may construct one; a third-party
`ToolExecutor` cannot use the stronger wait contract to hold an Agent open:

```rust,ignore
pub struct ToolClaimProfile { kind: private::ToolClaimKind }

pub(crate) struct ToolDispatchBinding(Arc<()>);

impl ToolClaimProfile {
    pub fn standard() -> Self;
    pub(crate) fn shell_action() -> Self;
}

pub enum ToolPreparation {
    Complete(ToolExecutionResult),
    Mutation(PreparedToolMutation),
    Action(PreparedToolActionSetup),
}

pub(crate) enum ToolActionTurnStop {
    None,
    CallerCancelled,
    TurnTimeout,
}

pub(crate) struct StepResolution {
    outcome: StepOutcome,
    latched_turn_stop: ToolActionTurnStop,
}

pub(crate) enum ActionDeclineReason {
    PolicyDenied,
    ApprovalRejected,
    ApprovalCancelled,
    ApprovalUnavailable,
    AbortedBeforeDispatch,
    OutputBudgetExceeded,
}

pub(crate) enum ToolActionOutcome {
    NotStarted {
        turn_stop: ToolActionTurnStop,
        result: ToolExecutionResult,
    },
    Infrastructure {
        turn_stop: ToolActionTurnStop,
    },
    StartedAndQuiescent {
        turn_stop: ToolActionTurnStop,
        result: ToolExecutionResult,
    },
    StartedOwnershipLost {
        turn_stop: ToolActionTurnStop,
    },
}

pub(crate) enum ToolActionSetupOutcome {
    Ready(PreparedToolAction),
    NotStarted {
        turn_stop: ToolActionTurnStop,
        result: ToolExecutionResult,
    },
    Infrastructure {
        turn_stop: ToolActionTurnStop,
    },
}

type ToolActionSetupFuture =
    Pin<Box<dyn Future<Output = ToolActionSetupOutcome> + Send + 'static>>;
type ToolActionSetupFn = Box<
    dyn FnOnce(ToolActionSetupControl) -> ToolActionSetupFuture + Send + 'static,
>;

pub struct PreparedToolActionSetup {
    dispatch: ToolDispatchBinding,
    resolve: ToolActionSetupFn,
}

impl PreparedToolActionSetup {
    pub(crate) fn matches_dispatch(&self, expected: &ToolDispatchBinding) -> bool;
}

type ToolActionFuture =
    Pin<Box<dyn Future<Output = ToolActionOutcome> + Send + 'static>>;
type ToolActionDeclineFn = Box<
    dyn FnOnce(ActionDeclineReason)
        -> Result<ToolExecutionResult, ToolExecutorError>
        + Send
        + 'static,
>;
type ToolActionRunFn =
    Box<dyn FnOnce(ToolActionControl) -> ToolActionFuture + Send + 'static>;

pub struct PreparedToolAction {
    dispatch: ToolDispatchBinding,
    prompt: ApprovalPrompt,
    maximum_result_event_bytes: usize,
    decline: ToolActionDeclineFn,
    run: ToolActionRunFn,
}

impl PreparedToolAction {
    pub(crate) fn matches_dispatch(&self, expected: &ToolDispatchBinding) -> bool;
}
```

For every planned call, the Agent creates a fresh `ToolDispatchBinding`, retains
one crate-private clone in `PlannedTool`, and puts another private clone in that
call's `ToolExecutionRequest`. The binding has no public constructor, accessor,
serialization, equality value, or `Debug` payload; equality is only an internal
`Arc::ptr_eq`. An external executor can move the opaque request into the real
registry and can cache that whole carrier, but it cannot extract, independently
clone, synthesize, or serialize the binding. Moving call A's cached carrier into
call B still fails B's pointer-identity checks.

`ToolExecutor` gains a prompt, read-only
`claim_profile(&self, tool_name: &str) -> ToolClaimProfile` method whose default
is `standard()`. Only the built-in `LocalToolRegistry` can return the private
shell-action profile for its declared `bash`; merely declaring a custom schema
with that name grants no action capability. An external wrapper may forward a
profile obtained from the real local registry, but it still cannot construct a
different `PreparedToolAction`; the sealed action constructor is the execution
authority. The Agent snapshots this profile while planning claims, before
`prepare` runs. If an `Action` ever arrives for a plan without the matching
crate-controlled profile, it is an infrastructure invariant failure and cannot
spawn. The profile plus the sealed preparation is the reason the Agent may
choose the shell-specific small `started: false` fallback before preparation.
The profile is a bounded planning fact, not a transferable permission to spawn.
The public trait documentation requires `claim_profile` to be pure, prompt, and
stable for the executor's immutable schema snapshot; its redacted `Debug`
reveals only `standard` versus crate-controlled action planning.
The Agent samples it once inside `catch_unwind`, before claim admission or any
assistant/call settlement. A panic is a fixed step-level infrastructure failure
with no tool call, no opaque payload, and no second profile read; it cannot leave
a half-admitted shell call.

The local registry copies the request's private dispatch binding into the sealed
setup, and the setup must carry that same binding into the resolved Action. The
Agent checks pointer identity as soon as an Action setup returns, again when the
setup resolves an Action, and once more before invoking the single-use run
closure. The first check happens before workdir inspection or approval. A wrapper
that caches call A's genuine setup and returns it for call B therefore causes a
pre-spawn unresolved infrastructure failure: no approval provider or process is
invoked, and the mismatched durable call poisons reuse rather than executing the
wrong command. A text `call_id`, command, or arguments comparison is not an
authority check because those values can repeat across turns or Sessions.

`PreparedToolActionSetup::new`, `PreparedToolAction::new`, both control types,
and the outcome constructors are crate-private. Public `Debug` output contains
only redacted bounded facts and the single-use flag. `ToolActionSetupControl`
contains a child cancellation token, the absolute turn deadline, and the
preparation deadline. `ToolActionControl` contains a child cancellation token
plus the absolute turn and Agent-action deadlines. The sealed runner also owns
the command-local deadline; one future therefore observes all four action stop
sources and can retain the first cause while cleanup continues.
Construction rejects a zero/oversized result bound. Every decline and
`NotStarted` outcome must be an error result with durable `started: false`;
every `StartedAndQuiescent` outcome must carry `started: true`. Shell outcomes
must not contain the file-mutation `committed` marker. `turn_stop` carries only
caller cancellation or the turn deadline, because those close the surrounding
turn; an Agent-tool timeout is instead the primary tool result when it wins and
does not by itself erase a completed command or close an otherwise reusable
turn. `StartedOwnershipLost` carries no invented result, but it retains the
independent caller/turn stop already observed. The Agent uses its existing
infrastructure-failure path, leaves the call unresolved, and poisons both the
current instance and any reconstruction until Phase 8 append-only repair. A
caller cancellation still closes the turn as aborted, and a turn deadline still
closes it with `AGENT_TURN_TIMEOUT`; that surrounding reason does not fabricate a
tool result or make the unresolved history reusable.
`ToolActionOutcome::Infrastructure` is the corresponding pre-spawn invariant
path: it is legal only while standard-library spawn has not succeeded, carries no
result, releases the result claim, poisons the unresolved call, and propagates its
already-observed outer stop into `StepResolution`. It must not be mislabeled as
`NotStarted`, because that variant promises a truthful model-facing result, or as
`StartedOwnershipLost`, because no process was started.

Preparation is side-effect free and has two explicit ownership stages. The
public `ToolExecutor::prepare` adapter may parse the already-bounded arguments,
but the built-in shell implementation must promptly return either a fixed
`Complete` rejection or the sealed `PreparedToolActionSetup`; it performs no
filesystem syscall or process creation first. While awaiting that public future,
the Agent keeps the ordinary priority caller cancellation, turn deadline,
configured tool timeout, then future readiness. A stop that wins, including one
that becomes ready while a prompt but nonzero future poll is returning, uses the
action-aware `started: false`
fallback, cancels the child token, gives an unsealed adapter the ordinary
one-second cleanup grace, and then drops it. Because no sealed setup has begun,
the real registry leaves no blocking job or command behind. This also bounds a
transparent wrapper that delays forwarding the real setup. Concretely, after the
future returns or panics the Agent samples caller, turn, and configured tool
deadline again in that order before classifying the returned preparation; a
future-ready branch is not assumed to preserve the readiness snapshot from before
its last poll.

Only the sealed setup may inspect the workdir. It owns one `spawn_blocking`
JoinHandle that performs the capability-relative `openat`/metadata sequence and
checks its cancellation token between components. The setup future observes
caller cancellation, the turn deadline, and its fixed five-second preparation
deadline, but those observations only latch classification and cancel the
worker: it always awaits the owned JoinHandle before returning. A later
caller/turn stop fills the independent `turn_stop` without rewriting an earlier
`TOOL_TIMEOUT` preparation cause. A stuck kernel/FUSE syscall may therefore
delay a `started: false` return beyond five seconds; no blocking job is abandoned
or detached. Join failure or panic overrides any previously latched ordinary
preparation result and becomes unresolved infrastructure, not a fabricated tool
result; the setup outcome still carries the caller/turn stop observed while
ownership drained, so the surrounding turn closes truthfully.
On a stop, any opened directory handle is discarded. On success, the setup
returns a `PreparedToolAction` holding the workdir capability and all fixed
result envelopes. If a stop and the JoinHandle are ready in one poll, caller,
turn, and preparation deadline are sampled in that order before the join result;
a later caller/turn stop remains independently latched while ownership drains.

The configured Agent action timer is a fresh window that starts only immediately
before the returned sealed Action future is first polled. Caller/turn interruption
during either pre-start stage uses `ABORTED_BEFORE_DISPATCH` plus the appropriate
surrounding turn stop; the sealed setup ceiling uses the existing `TOOL_TIMEOUT`
code with a preparation-specific fixed message.

Before asking or starting, the Agent grows the existing result claim to the
action's maximum truthful event size and checks the configured
per-result/per-turn budgets. Failure at that point returns a bounded decline
result without calling the approval provider or action body.

At tool-round planning time, a declared built-in `bash` call receives a minimal,
truthful action-aware fallback containing `started: false`, null status fields,
and no invented parsed `workdir` or `timeoutMs`. Separately, the crate computes a
pre-start claim ceiling from an encoded-size probe of the largest legal
`started: false` shape, including a maximum-length real workdir. The probe is a
number only: it is never an event fallback and none of its placeholder bytes may
enter Session JSON.

After the batch of ordinary event claims is acquired, but before the assistant
message or any `tool/call` is settled, the Agent grows every shell-action result
claim to that pre-start ceiling with
`reserve_claim_retained_json_bytes`. If any grow fails, it releases the whole
uncommitted batch and returns the step-level budget failure; no assistant message
or tool call is appended. This gives all-or-none admission even though the
individual claim sizes are adjusted in sequence.

Every fixed decline result is proven no larger than that ceiling. It covers
argument rejection, pre-dispatch cancellation, cancellation or turn expiry while
preparing, the fixed preparation timeout, a fixed `Deny`, an invalid approval
channel, and failed later growth to the prepared Action's full result bound. When
one of those facts occurs, the Agent rebinds the retained fallback to the actual
minimal truthful event and settles it; it never persists padding or a maximum
workdir that was not parsed for that call. The Agent constructs these
pre-preparation results from the crate-private profile and does not need a
not-yet-returned decline closure. Expected model-facing paths therefore retain
`started: false`. A preparation factory error or panic is still the existing
unresolved infrastructure path: it starts no process, releases the claim, and
does not invent a result merely to add the field. Other tool fallbacks are
unchanged. Exact encoded-size and replay tests protect this ordering.

The profile also constrains legal preparation variants. A shell-action profile
may return only an error `Complete` result with `started: false`, null status
fields, and no file `committed` marker, or a sealed Action setup; returning a
success `Complete` or `Mutation` is infrastructure failure. A standard profile
returning an Action setup, or any profile/result metadata mismatch, is likewise
infrastructure failure and cannot spawn.

The exact action linearization point is a successful
`std::process::Command::spawn`. A
`NotStarted` means no child was successfully created and must contain a bounded
error result. `StartedAndQuiescent` means spawn succeeded, the direct
child was reaped, both local pipe readers finished or were explicitly closed by
the bounded escaped-pipe rule, and the created process group satisfied the
platform-specific quiescence rule below. Only handled failures for which the
runner subsequently proves group quiescence, closes the pipes, and reaps the
child may become a prevalidated `StartedAndQuiescent` error result. A
pipe-setup failure closes both local handles, sets `pipeSetupFailed: true`, and
becomes `SHELL_PIPE_SETUP_FAILED` unless a higher-priority stop was observed at
that return boundary. A later stream-read error closes that local pipe, sets
`pipeReadFailed: true`, becomes
the `SHELL_PIPE_READ_FAILED` primary failure if no earlier forced cause was
latched, and still runs cleanup. A signal-delivery error never proves absence:
`ESRCH`, `EPERM`, and other errors all defer to the platform observer. `EPERM`
or another non-`ESRCH` delivery failure sets the secondary
`signalDeliveryFailed: true` only if later quiescence is independently proved;
it never invents a second primary code.
Interrupted waits are retried. If the host steals the wait status, a final reap
fails, or the observer identity becomes invalid after spawn, the runner cannot
truthfully claim quiescence and returns `StartedOwnershipLost` instead. The run
future has no ordinary `Err` exit, and an internal panic after spawn remains the
separately documented crash-tail. These invariants are available only to the
built-in runner and are tested at its construction boundary.

Immediately after a successful standard-library spawn, before any await or other fallible work,
the runner installs an allocation-free group guard from the returned child PID
and the shared armed flag. An impossible missing PID is post-start ownership
loss, not `NotStarted`; the runner uses only direct-child best effort and never
guesses a process-group number.

The final cancellation check and the synchronous spawn call are not atomic. If
cancellation lands between them and spawn nevertheless succeeds, the outcome is
truthfully `StartedAndQuiescent`: the runner notices the token immediately,
terminates the group, and uses the after-start abort result. It must never call
that race `ABORTED_BEFORE_DISPATCH`.

The final SIGCHLD, procfs/mount, and workdir-identity rechecks run together in one
owned `spawn_blocking` job before the reactor preflight. The Action keeps polling
caller, turn, and Agent-action stops while it owns that JoinHandle. A stop latches
its classification, cancels the job's cooperative token, and still awaits the job
to definite completion; no JoinHandle is dropped or detached. The worker checks
the token between path components and pseudo-file records. A stuck kernel/FUSE
syscall can therefore delay the classified pre-spawn return, just as in sealed
setup. Once the job joins, any latched stop prevents the reactor preflight and
spawn. Join panic/failure is unresolved infrastructure with the already-observed
outer turn stop retained.

Every synchronous pre-spawn operation has a return-boundary rule. Immediately
after the owned recheck job or `std::process::Command::spawn` returns, the runner
samples caller cancellation, turn deadline, and Agent tool deadline in that
priority order before classifying a returned pre-spawn error. If spawn failed
and a stop is ready, the truthful result is `started: false`: caller cancellation
uses `ABORTED_BEFORE_DISPATCH`, a turn deadline uses the same correlated abort
result plus `ToolActionTurnStop::TurnTimeout`, and the Agent tool deadline uses
`TOOL_TIMEOUT`. Otherwise the returned recheck/spawn failure wins. If spawn succeeded, Started
is already true regardless of a newly ready stop, and the runner immediately
enters owned cleanup. The command-local timer has not begun before spawn. This
is a first-observed rule, not a claim about when a token changed during a
blocking syscall.

After an explicit grant, the Agent performs one final cancellation/deadline
check, constructs `ToolActionControl`, and awaits the same action future to its
definite outcome. There is deliberately no one-second drop escape hatch for an
already-started action. The action's state machine locks the first forced
process-stop cause it observes; a later observed cancellation or deadline cannot
rewrite it after TERM or KILL cleanup begins. Cancellation tokens carry no
arrival timestamp, so the design never claims to reconstruct an unobserved
wall-clock ordering. When multiple causes are ready in one poll, the fixed
priority is caller cancellation, turn deadline, Agent tool deadline, then
command deadline.

- caller cancellation becomes `ABORTED` after start (or
  `ABORTED_BEFORE_DISPATCH` before spawn) and closes the turn as aborted;
- turn deadline writes the correlated abort result, then closes with
  `AGENT_TURN_TIMEOUT`;
- Agent tool deadline becomes `TOOL_TIMEOUT`;
- a command's own `timeoutMs` remains a normal shell result with
  `timedOut: true`.

An action becoming `Ready` is not itself the durable settlement point. After
awaiting the outcome, the Agent samples caller cancellation and the turn
deadline again before appending `tool/result`. If neither was previously
latched, this sample records what is ready, with caller cancellation winning
when both are ready in that same sample; it does not claim which token changed
first between polls. The Agent appends the already-determined process result
without rewriting it, then samples those two outer stops once more before
dispatching any later tool call or model step. A stop observed in either sample
closes the turn and prevents later side effects. The ordinary pre-dispatch
cancellation check on the next call is the linearization boundary for a
cancellation that arrives after the second sample.

That first-observed outer stop must survive the existing step/turn layers. The
tool round returns a `StepResolution`, not a bare `StepOutcome`; its
`latched_turn_stop` is filled from every shell pre-start gate, policy/approval
wait, final pre-spawn check, Action setup, Action outcome, or the two post-Ready
samples and is never overwritten once non-`None`. Existing mutation
`ToolStop::Cancelled`/`TurnTimeout` values are mapped into the same step-level
latch as part of this outer-loop refactor, so an approval provider's cooperative
cleanup cannot reintroduce the old overwrite bug. This applies even when the
accompanying outcome is unresolved infrastructure or ownership loss.
After `run_step` returns, `run_entered_turn` first commits `step/end`, then checks
the stored stop before rereading clocks or the cancellation token:

- `CallerCancelled` closes the turn as the existing user abort;
- `TurnTimeout` closes it with `AGENT_TURN_TIMEOUT`;
- only `None` performs the existing fresh sample, with caller cancellation before
  the turn deadline, and then interprets the ordinary `StepOutcome`.

Consequently a turn deadline observed before a long cleanup cannot be rewritten
by a later caller cancellation, while a caller cancellation observed first
remains an abort even if the deadline passes during cleanup. If both are first
seen in one sample, caller cancellation wins. A stop that arrives after the
Action's second post-settlement sample is still caught by the fresh `None` sample;
no later tool call or model step starts. Setup `Infrastructure` and
`StartedOwnershipLost` keep their unresolved/poisoned history while the same
latched value independently selects the truthful outer `turn/end` reason.

Result settlement is independent of file-mutation truth:

```rust,ignore
enum ResultSettlement {
    FallbackAllowed,
    PreferredRequired,
}
```

Only `ToolCommitDisposition::Committed` means a file was committed. A mutation
maps that disposition to `PreferredRequired`; an action maps
`StartedAndQuiescent` to `PreferredRequired`; declines and `NotStarted` use
`FallbackAllowed`. Action `Infrastructure` and `StartedOwnershipLost` settle no
result and enter the existing unresolved/infrastructure guard while preserving
their step-level outer stop. Shell metadata never sets the file field `committed`
merely to protect a result.

The process, first-cause state, and stream readers remain in one owned future;
there are no detached Tokio tasks. The action type is sealed specifically so an
untrusted extension cannot violate that promise. A drop guard shares the same
single signalling-armed flag as the normal state machine. While armed it may
send one final best-effort SIGKILL; quiescence permanently disarms it before
the owned standard child is reaped, so destruction after reap can never signal a
reused PGID. API
callers still must cancel and await `run_turn`.
Dropping a polled `run_turn`, or an internal panic after spawn, is crash-tail
behavior: it may leave an unresolved Session tail and cannot promise asynchronous
reaping. Phase 8 may append repair evidence but must never rerun a shell call or
signal a PID recovered from an old log, because that number may have been reused.

## Approval and durable order

One accepted action follows this order:

```text
claim admission                    reserve truthful fallbacks/ceilings; append nothing
assistant/message
tool/call                         durable intention exists before preparation
prepare                           parse + inspect only; no process
policy
  deny                            no approval call, no process
  allow                           reserve result capacity, then continue
  ask
    reserve result capacity
    approval/asked
    wait for one answer
    approval/decided
final cancellation/deadline check
spawn one process group
capture + wait, or terminate + wait
tool/result                       exactly cites the preceding tool/call
step/end
```

Only an explicit `Allow` policy or an asked decision of `allowed-once` may reach
spawn. Rejected, cancelled, unavailable, pre-spawn panicking, or capacity-failed
paths create no process. The mandatory pre-start ceiling is part of whole-round
admission and can reject a nearly full Session before any policy is evaluated.
After that admission succeeds, a fixed `Deny` is evaluated before the later
growth from the pre-start ceiling to the started-result bound, so no second,
larger capacity request can disguise that policy denial as an output-budget
error.
Approval IDs and call IDs are correlated exactly as in Phase 5. The preview is
the exact command plus its resolved relative workdir and effective requested
timeout; the bounded approval `reason` uses only the at-most-1-KiB description,
so a maximum command or workdir cannot overflow the reason field. Terminal
renderers must escape this untrusted text. `Debug`
implementations expose only lengths, flags, and IDs, never command text, output,
workdir text, or environment values.

Multiple calls in one model response remain serial. Approval events make event
sequence numbers dynamic, so every later call/result claim is rebound from the
actual committed call sequence rather than predicted arithmetically.

## Arguments and workdir

The closed Rust schema is:

```json
{
  "type": "object",
  "properties": {
    "command": {
      "type": "string", "minLength": 1, "maxLength": 32768,
      "description": "Exact bash command; runtime maximum is 32768 UTF-8 bytes"
    },
    "description": {
      "type": "string", "minLength": 1, "maxLength": 1024,
      "description": "Short display description; runtime maximum is 1024 UTF-8 bytes"
    },
    "timeoutMs": {
      "type": "integer", "minimum": 1, "maximum": 295000,
      "description": "Command-local timeout in milliseconds"
    },
    "workdir": {
      "type": "string", "minLength": 1, "maxLength": 4096,
      "description": "Workspace-contained directory; runtime maximum is 4096 UTF-8 bytes"
    }
  },
  "required": ["command", "description"],
  "additionalProperties": false
}
```

Explicit `null`, wrong types, unknown fields, an empty/blank command or
description, disallowed control characters, a zero/negative/fractional timeout,
and an over-limit value are rejected before approval. Newline and tab are
allowed in commands; other C0/C1 controls are rejected. `description` is display
metadata and does not alter execution.

JSON Schema `maxLength` counts characters, while the runtime ceilings below
count UTF-8 bytes. Each schema description states that the byte rule is the
authoritative second-stage semantic check, so a multibyte string can satisfy the
structural schema and still receive bounded `INVALID_ARGS`. Blank/control/path
semantics likewise remain explicit runtime validation rather than pretending a
regular expression completely describes shell or filesystem meaning.

An omitted workdir means the startup workspace. A relative value is resolved
against that root; an absolute value is accepted only when its lexical,
normalized form names a location inside the same root. The target must be an
existing directory, and every component is opened from the retained workspace
capability with `openat(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`. Symlink
components are rejected even when they currently point back inside. Preparation
retains the final `cap_std::fs::Dir`, its workspace-relative display, and its
Unix device/inode identity; it never converts that authority back into an
ambient path.

Immediately before spawn, the registry reopens the same relative directory from
the retained root and compares device/inode identity. The freshly verified
directory handle is moved into the child setup closure. A rename after that
check cannot redirect the child into a replacement path: `fchdir` still targets
the held directory inode. The displayed path may become stale or the directory
may have been moved elsewhere under the user's authority, so this remains an
initial-CWD guarantee rather than a filesystem sandbox or immutable namespace.

## Environment policy

The child receives null stdin and does not inherit the harness controlling
terminal. `Command::env_clear` removes the ambient environment, after which Rust
copies only these exact Unicode variables when present:

- `PATH`, `HOME`, `USER`, `LOGNAME`;
- `TMPDIR`, `TMP`, `TEMP`;
- `LANG`, `LANGUAGE`, `TZ`, `LC_ALL`, `LC_CTYPE`, `LC_MESSAGES`,
  `LC_COLLATE`, `LC_MONETARY`, `LC_NUMERIC`, and `LC_TIME`;
- `CARGO_HOME` and `RUSTUP_HOME`.

An allowlisted name with a non-Unicode value is rejected rather than silently
changed. A missing `PATH` receives the fixed fallback `/usr/bin:/bin`. The copied
set is checked against the count/byte ceilings below. Rust then overwrites
`NO_COLOR=1`, `TERM=dumb`, `PAGER=cat`, `GIT_PAGER=cat`, and
`GIT_TERMINAL_PROMPT=0`.

This effective environment is sampled, validated, and stored as an immutable
snapshot when `LocalToolRegistry::open` is called. Preparation and spawn reuse
that same `Arc` snapshot; neither rereads process-global environment after the
approval preview. The fixed source-name set contains nineteen possible copied
entries and five deterministic overrides, so the 24-entry limit is a
construction invariant rather than a model-reachable count rejection. Total
encoded name/value bytes remain a fallible 32 KiB build-time check. The preview
lists the fixed policy and override names, never their values.
Everything else is absent, including `BASH_ENV`, `ENV`, `SHELLOPTS`, `BASHOPTS`,
`CDPATH`, `GLOBIGNORE`, `LD_*`, `DYLD_*`, proxy variables, credential-shaped
names, `SSH_AUTH_SOCK`, and ambient `DSH_*` values.

There is no model-supplied environment field. Unknown injected fields are
rejected by the closed argument parser. This allowlist prevents an ambient API
key or startup hook from being inherited, but it is not a complete data-loss
prevention system: an allowed command still has the user's filesystem and
network authority and can read credentials from files. Approval and honest
documentation, not the environment filter, are the security boundary.

## Host process prerequisites

The retained-leader protocol needs a waitable direct child. At
`LocalToolRegistry::open` and again immediately before every spawn, a read-only
`sigaction(SIGCHLD, null, old)` query must show neither an explicit `SIG_IGN`
handler nor `SA_NOCLDWAIT`. The registry never installs, resets, or replaces a
process-global signal handler. An invalid disposition fails registry construction
or produces a bounded `SHELL_PROCESS_OBSERVER_UNAVAILABLE` `NotStarted` result
before spawn. The default `SIG_DFL` disposition is accepted even though its
documented default action is “ignore”: unlike an explicitly installed
`SIG_IGN`, it still leaves wait status on the supported systems.

An embedding host must not change that disposition concurrently and must not
call a broad `wait*`/`waitpid(-1, ...)` handler that can reap a child owned by the
runner. The standalone CLI satisfies this ownership rule. There is no portable
atomic operation that combines a process-global signal check, spawn, and later
wait. If a violating host wins that narrow race and `waitid(WNOWAIT)` returns
`ECHILD`, the runner stops using the now-unanchored numeric PGID and returns
`StartedOwnershipLost`; the unresolved durable call makes the Agent fail closed.
It must not signal a possibly reused PGID or fabricate a quiescent result.

Linux has one additional visibility prerequisite. The registry opens and keeps
a descriptor for `/proc`, verifies `fstatfs == PROC_SUPER_MAGIC`, and checks that
`self/stat` identifies `getpid()` in the current PID namespace. The harness PID
must not be namespace PID 1: PID 1 is the namespace's implicit orphan adopter
even when explicit child-subreaper mode is off, while this foreground runner
intentionally reaps only its owned Bash child plus exact-PID same-group children
that the observer can attribute to this Action. Running as namespace init is
therefore a redacted unsupported-observer build error rather than silently
accumulating adopted zombie descendants. It parses the
exact mount record selected by the retained descriptor's
`statx(AT_EMPTY_PATH, STATX_MNT_ID)` value from `self/mountinfo`, requires the
filesystem type `proc`, and accepts only absent,
`hidepid=0`, or `hidepid=off`; `hidepid=1`, `2`, `4`/`ptraceable`, a `gid`
bypass, an unknown spelling, a different namespace, or an ambiguous mount fails
construction. This is necessary because a same-group process can become
non-dumpable and disappear from a `hidepid=ptraceable` directory view.

Linux must also report `rustix::process::child_subreaper() == None` when
the registry opens and in the owned final pre-spawn recheck. A harness that is a
child subreaper can adopt a same-group grandchild after Bash exits; ignoring its
zombie or an escaped/unrelated adopted child would give the runner a broader
reaping duty it never agreed to own. A nonzero setting is therefore a
redacted unsupported-observer build error or pre-spawn
`SHELL_PROCESS_OBSERVER_UNAVAILABLE`, with no command started. The embedding
host must not change the process-global subreaper setting concurrently, just as
it must not replace `SIGCHLD` handling or steal wait status. Phase 6 does not
silently take ownership of unrelated adopted children.

Neither prerequisite parser reads an unbounded pseudo-file. `self/stat` has a
4 KiB payload ceiling and is read with one additional detection byte.
`self/mountinfo` is processed as an O(1)-memory byte stream with a 64 KiB
per-record ceiling plus one detection byte; numeric tokens and escaped fields
are checked before conversion. A complete pass must find exactly one record for
the retained mount ID and must reach EOF. A missing, duplicate, malformed, or
overlong matching record is unsupported/unknown rather than being accepted from
a partial `read_to_string` or `lines()` result.

The retained procfs identity and mount policy are checked again before spawn and
on every observer pass. A change before spawn is `NotStarted`; a remount,
namespace replacement, or visibility loss after spawn is
`StartedOwnershipLost`, never a false “empty” scan. Tests exercise the parser and
state transitions without needing privileged mounts, while an isolated
namespace test runs when the platform grants the required capability.

`StartedOwnershipLost` still has a direct-child resource obligation. If
`waitid(WNOWAIT)` confirms that the runner retains the leader's wait status but
the platform group observer becomes invalid, the runner closes both pipes,
deterministically attempts one SIGKILL for the still-anchored group, permanently
disarms every signal path, and then moves the standard child into an owned
blocking `wait` job to reap the direct
child. It returns ownership loss because descendants were not proven absent,
not because it abandoned a child it still owns. If the final signal fails, it
still waits for the direct child and may therefore remain blocked on a hostile
or uninterruptible process.

If the observer becomes invalid while non-reaping `waitid` still reports
`si_pid == 0`, the direct leader is running rather than retained as a zombie.
That live owned leader still anchors PID=PGID: while signalling remains armed,
the runner closes the pipes and attempts one final group SIGKILL, then keeps
polling/awaiting the direct child's wait status and reaps it before returning
`StartedOwnershipLost`. An `ESRCH` race is followed by another owned-child
status observation and is never treated as proof that descendants are absent;
`EPERM` or another delivery failure is recorded and the direct-child wait may
remain blocked. Only after a waitable direct status has been obtained is every
group signal permanently disarmed. A deterministic fault test invalidates the
observer while a TERM-trapping leader is still running and proves that the
final anchored KILL happens before the direct wait/reap rather than allowing an
unbounded wait on a leader the runner could still stop.

If `waitid` instead reports `ECHILD`, an embedding host or disposition already
stole/auto-reaped the status. The runner closes pipes, disarms without any
numeric-PGID signal, and records the host-contract violation; there is then no
direct child it can reap. Another indeterminate anchor error also forbids a group
signal, but the runner still owns and awaits a standard-child `wait` job for whatever direct-child
ownership remains. A wait failure is itself ownership loss and never a
quiescence claim. In every case the unresolved Session tail is the durable
recovery barrier; no asynchronous reaping is claimed.

## Process lifecycle

The Unix runner uses `std::process::Command` with fixed `/bin/bash`, Unix
`CommandExt::arg0("bash")`, arguments `--noprofile --norc -c` plus the exact
command, null stdin, and piped stdout/stderr. Fixing the executable avoids a
`PATH` replacement, while the explicit argv[0] preserves the ordinary upstream
`$0 == "bash"` observation. The command does not also call `process_group(0)`.

Immediately before spawn, the runner performs a no-child I/O-driver preflight:
it creates a private `std::os::unix::net::UnixStream::pair`, makes the endpoint
being tested nonblocking, verifies both standard-library descriptors are
close-on-exec, and attempts one temporary `AsyncFd` registration inside
`catch_unwind`. This safe standard-library pair is available on both macOS and
Linux; Rust 1.85 applies close-on-exec before returning it, whereas rustix
1.1.4's flagged `pipe_with` API is not exposed on Apple targets. Tokio 1.53 may
panic rather than
return `Err` when the current runtime has no I/O reactor, so an AsyncFd panic or
registration error becomes the bounded `started: false`
`SHELL_ASYNC_RUNTIME_UNAVAILABLE` result. Pair creation, nonblocking setup, or a
missing close-on-exec flag (including `EMFILE`/`ENFILE`) is instead
`SHELL_PIPE_PREFLIGHT_FAILED`; it does not falsely blame the reactor. Both
endpoints are dropped on every branch. The fixed panic payload and raw OS error
are not persisted. Stop sources are resampled at this return boundary before
either error is classified.

The design deliberately does not use `tokio::process::Command::spawn` as the
Started boundary. Tokio first creates an OS child through `std`, then performs
fallible pipe/reactor and Linux pidfd setup; a later adapter error can therefore
be returned after the command already executed and before Tokio installs its
drop guard. Rust instead takes ownership of the returned `std::process::Child`
and PID directly. The allocation-free group guard is installed immediately.
Only then are the two pipe handles set `O_NONBLOCK` and registered separately as
`tokio::io::unix::AsyncFd` values. Failure to take a pipe, set nonblocking mode,
or register either fd is a `started: true` `SHELL_PIPE_SETUP_FAILED` primary
failure (unless a higher-priority stop is ready at that same return boundary),
followed by the normal owned group cleanup. It can never become
`SHELL_SPAWN_FAILED` or `started: false`.

Each real post-spawn `AsyncFd` construction is also locally wrapped in
`catch_unwind`; an unexpected no-reactor panic or ordinary registration error is
converted to the same started setup failure while the already-installed group
guard retains ownership. It is not allowed to escape to the weaker crash-tail
panic path.

The command deadline begins at spawn success, before pipe adaptation. After
each take/nonblocking/registration operation returns, the runner samples caller,
turn, Agent-action, and command deadlines in that order before a pipe-setup
failure. A ready stop keeps its primary classification, but Started remains true
and `pipeSetupFailed` plus the visible warning still record the secondary setup
fact. Tests inject failure at every adaptation step, including the second pipe.

The group guard owns the only best-effort Drop signal; it first checks whether
the leader is still waitable/alive and never signals after `ECHILD` or permanent
disarm. No opaque library guard can later signal the direct PID. Readiness uses
the AsyncFd clear-and-retry contract for `EAGAIN`; bytes are read through the
owned nonblocking descriptors, not through detached reader tasks.

One deliberately tiny Unix spawn module contains the process-creation `unsafe`
site. Its `CommandExt::pre_exec` closure performs only the async-signal-safe
`rustix::process::setsid()` and `rustix::process::fchdir(held_directory_fd)`
syscalls, returns their `io::Error` directly, and does not allocate, lock, log,
or format after fork. `setsid` makes the child PID both the new session ID and
process-group ID, and prevents inheritance of the harness controlling terminal.
The crate changes its broad lint from `forbid(unsafe_code)` to
`deny(unsafe_code)` so only the spawn, Unix signal-prerequisite, and isolated
macOS process-observer modules can each carry a narrowly documented
`allow(unsafe_code)`; all other modules remain denied. Tests prove the child
PID, session, group, CWD inode, null stdin, and lack of inherited `/dev/tty`.

After `spawn` succeeds, the command-local timer begins. Stdout, stderr,
non-reaping child-exit observation, all deadlines, and cancellation are polled
in one future. A 10 ms leader tick remains armed while direct status is unknown;
on every tick it calls nonblocking `waitid(WNOHANG | WNOWAIT)`. This wake source
is independent of pipe readiness, so a shell that exits while a silent
background descendant keeps both pipe writers open is detected promptly and
enters the unsupported-background scan instead of waiting for EOF or the command
deadline. After direct exit is observed, the same interval separates complete
group-observer passes. Each stream reader consumes 8 KiB chunks, yields between ready
chunks, alternates which stream is polled first after every ready chunk, and
always checks stop sources before more I/O. Thus a continuously ready stdout
cannot starve stderr or timers. Each stream retains only its byte tail, including
during a TERM grace; it does not silently switch to discard mode before a cap or
explicit close. Checked aggregate accounting reads at most the remaining
allowance plus one detection byte. Before group quiescence, crossing the observed
byte ceiling sends SIGKILL and closes both local read ends immediately. After
group signalling has been permanently disarmed, the same crossing can only close
the pipes and record the limit; it must never signal the old numeric PGID. There
is no unbounded `read_to_end`, channel, or detached reader task.

The state machine owns four facts rather than overloading one cancellation
token:

- `primary_result_cause` is the first non-natural reason that determines the
  result: unsupported background, output limit, pipe-setup/read failure,
  pipe-drain timeout, caller cancellation, turn deadline, Agent tool deadline,
  or command deadline. Later causes never rewrite that durable classification.
- `natural_status` is the provisional direct-child exit or signal status. A
  natural leader exit does not lock `primary_result_cause`, because a live
  background member, an output limit, or a bounded pipe-drain failure may still
  be discovered before the action settles. It does, however, permanently disarm
  the command-local deadline as soon as `WNOWAIT` first observes that status:
  later group observation, direct reap, or pipe drain cannot relabel an already
  exited command as `timedOut`. Caller, turn, and Agent-action stops remain live
  during that cleanup.
- `turn_stop` independently records the first caller cancellation or turn
  deadline observed at any time before settlement. Thus a command timeout can
  remain `timedOut: true` even if cleanup crosses the turn deadline, while the
  turn still closes with `AGENT_TURN_TIMEOUT` instead of starting another model
  step.
- `output_limit_exceeded`, `pipe_setup_failed`, `pipe_read_failed`,
  `signal_delivery_failed`, and `pipe_drain_timed_out` record secondary bounded
  cleanup facts. They are set even when an earlier primary cause retains result
  precedence.

When several observations become ready in one poll, the order is caller
cancellation, turn deadline, Agent tool deadline, command deadline, pipe-read
failure, output limit, then natural process completion. Caller cancellation wins
when it and the turn deadline are ready in the same sample. A later caller/turn
stop is recorded without changing an earlier primary result cause. Thus a
command deadline and previously unobserved natural status first seen in the same
poll choose the deadline, but once natural status wins an earlier poll the command
timer is never sampled again.

Termination is one monotonic state machine:

```text
running
  -> cancellation, timeout, pipe-setup/read failure, output cap, or observed unsupported background
  -> SIGTERM for cancellation/timeout/pipe-setup/read/background
  -> wait fixed grace while continuing bounded tail capture
  -> SIGKILL if any executable group member remains
  -> crossing the output cap before quiescence skips the remaining TERM grace,
     sends SIGKILL, and closes both pipes
  -> observe same-group quiescence while the leader identity is still anchored
  -> permanently disarm group signals, then reap the direct child
  -> finish both pipes or close them at the bounded drain deadline
  -> return one classified outcome
```

Output-cap escalation is a safety action, not necessarily a second result cause.
If an earlier cancellation or timeout already locked `primary_result_cause`,
crossing the cap during its TERM grace still forces immediate KILL and pipe
closure, while the earlier durable classification remains unchanged. Once group
signalling is disarmed, no output observation may re-arm it; a later cap crossing
only closes the local pipes and records the secondary flag.

The direct child is deliberately not reaped as soon as it exits. The runner uses
`waitid(P_PID, WEXITED | WNOHANG | WNOWAIT)` to observe its status while keeping
the zombie leader waitable; that retained PID anchors the numeric process-group
identity and prevents reuse while probes or signals are still possible. A
numeric `killpg(pgid, 0)` result is only auxiliary evidence: in particular,
macOS may return `EPERM` for a retained zombie leader, and `EPERM` must never be
treated as absence.

On macOS, the observer uses `proc_listpgrppids` followed by
`proc_pidinfo(PROC_PIDT_SHORTBSDINFO, arg = 1)` for every returned PID. The
nonzero argument is required by XNU's process-info implementation to retain a
zombie reference; with the ordinary zero argument, a retained zombie can instead
produce `ESRCH`. Every record must be exactly `size_of::<proc_bsdshortinfo>()`
and repeat the requested PID and expected PGID. `SZOMB` is non-executable; every
other status is live. The retained direct leader must appear as
`pid == pgid` with `SZOMB` before a pass can be complete.

`proc_listpgrppids` receives a byte-sized buffer but returns a PID-slot count,
not a byte count; its null-buffer query is only the system-level `nprocs + 20`
sizing hint, not the target group's size. The registry reads both that hint and
positive `kern.maxproc`, requires each to be at most 4,194,304, and uses their
checked maximum plus one as the initial slot capacity. Slot count, byte count,
and conversion to the API's signed `c_int` are all checked; allocation uses a
fallible reservation. If a later pass fills the buffer, the observer rereads
both values and grows monotonically within the same hard ceiling. A still-filled
buffer proves only truncation, so it is conservatively `live` and
triggers/continues cleanup rather than ever becoming false absence.

Apple's wrapper can collapse a lower-level failure to zero while leaving
`errno`, so each call clears and then checks `errno`. A negative or inconsistent
list return, zero/duplicate PID, zero/short process-info return (including
`ESRCH` or `EPERM`), missing anchored leader, or identity mismatch is `unknown`.
Two independently complete all-zombie passes, separated by the observer retry
interval, are required before the group is quiescent. Each pass sorts its one PID
buffer in place only to reject duplicates, then drops it before the next pass;
the two exact member sets need not match because zombies may be reaped by their
actual parents between observations. This keeps peak table allocation to one
bounded buffer and avoids the old `killpg == ESRCH` shortcut.

On Linux, the observer streams every numeric `/proc` directory entry without
retaining a process-table collection. Each `/proc/<pid>/stat` has a 4 KiB payload
ceiling and is read into at most 4,097 bytes so exact-limit acceptance and
one-over rejection are distinguishable. It validates field 1 against the
directory PID and parses the state, parent PID (field 4), process group (field
5), session ID (field 6), and `num_threads` (field 20). Every matching group
member must also name the expected session. A matching non-`Z`/`X` process is
live. For every matching member, not only the leader, `Z`/`X` with
`num_threads > 1` is live because another thread may still execute after its
main thread called `pthread_exit`; zero, a missing value, or an overflow is
unknown. Only `Z`/`X` with exactly one thread reaches the zombie classification
or exact-reap rules below. The retained direct leader must be observed at
`pid == pgid`, in `Z`/`X`, with the expected group and exactly one thread before
a pass can be complete.

One Linux-only ownership case needs more than classification. Native code may
use `clone`/`clone3(CLONE_PARENT)` so a same-session/group nonleader becomes a
direct child of the harness rather than of Bash. If a streamed stat record is
`Z`/`X`, has the expected group/session, is not the retained leader, and has
`ppid == harness_pid`, the runner owns that exact zombie. While the retained
leader still anchors the group, it calls exact-PID
`rustix::process::waitpid(Some(pid), NOHANG | __WALL)` using an external-bit
`WaitOptions::from_bits_retain(libc::__WALL as u32)` value; this catches ordinary `SIGCHLD`
children and clone children with another or no exit signal. `EINTR` is retried.
The expected result must return that same PID; `None`, `ECHILD`, a different
identity, or another contradiction becomes `StartedOwnershipLost`. The stream
may reap multiple owned zombies in one O(1)-memory pass, but marks that pass
mutated and never uses it as absence evidence; after EOF it starts a fresh full
pass, so deletion-induced directory-offset effects cannot create a false empty
result without quadratic restart-after-every-child work. A continuously cloning
approved command may prevent settlement, but cannot grow an ignored same-group
zombie list.

The parser does not split the command name on whitespace or the first `)`:
Linux permits both characters there. It validates the numeric prefix, splits at
the final `") "`, then reads relative tokens `state = 0`, `ppid = 1`,
`pgrp = 2`, `session = 3`, and `num_threads = 17` (the documented absolute
fields 3, 4, 5, 6, and 20).

`ENOENT` after a numeric directory entry was observed means that process already
vanished and is skipped; the required second pass catches any surviving child.
Zero, a 4,097-byte detection read, malformed, or otherwise unreadable records make the pass
`unknown`. There is no artificial global entry/byte cap that could make every
future pass fail on a busy but supported host: memory is O(1), the kernel's
`pid_max` bounds the number of entries, and the per-record ceiling bounds
allocation. Linux support requires a complete probe when the registry opens.
That probe also reads a positive `/proc/sys/kernel/pid_max` no greater than
4,194,304; the value bounds work, not retained memory, and is refreshed if a
later scan observes more numeric entries than the snapshot.
The runner requires two consecutive complete no-live passes on both natural and
signalled exits. An unknown post-start observation is retried rather than being
reported as quiescence.

Each macOS list or Linux full scan runs as one owned blocking job because the
platform APIs are synchronous. Its retained memory and per-record input are
bounded, but a very large process table can take time to traverse. The action
future owns and awaits every returned `JoinHandle`; it never abandons a scanner.
A post-spawn scanner is not cancelled by the caller/action token: that token may
trigger TERM/KILL, but the complete observer pass is the evidence needed to
finish cleanup. Only the pre-spawn recheck worker uses cooperative cancellation
to stop work before a process exists.
A scan may delay noticing a newer deadline, but all already-observed caller/turn
facts remain latched and cleanup still completes before settlement. Non-reaping
leader checks and consecutive observer passes are separated by the fixed retry
interval below rather than busy-looping.

On the normal `StartedAndQuiescent` path, only after the platform observer
reports no live same-group member does the runner permanently disarm all group
signalling and move the retained
`std::process::Child` into one owned `spawn_blocking` job that calls `wait` to
reap the direct child. `waitid(WNOWAIT)` has already proved the status ready, so
the syscall should return immediately, but the async worker still does not make
a blocking call. No normal quiescent path may start this final wait job before
that point. This
ordering prevents a freed PGID from being reused by an unrelated process group
between reap and a late signal. The final `ExitStatus` must agree with the
provisional waitid exit/signal fact; a missing or contradictory status is
ownership loss, never a partly invented result. The exceptional
`StartedOwnershipLost` branches above have a separate direct-child reap job:
they first use the still-valid anchor for their one final bounded KILL attempt,
permanently disarm signalling, and then wait/reap without making any group
quiescence claim. Rust reaps its direct Bash child and the precisely attributable
same-group Linux `CLONE_PARENT` children above; other descendants are reaped by
their actual parent or the operating system, and unrelated/adopted children are
never consumed by a broad wait.

Every final direct-child `spawn_blocking` wait—normal or exceptional—remains
inside the Action's stop-selection loop. While the owned JoinHandle is pending,
the Action samples caller cancellation, turn deadline, and Agent-action deadline
in that order, latches the applicable process/turn facts, and cancels only its
cooperative child token; it never drops or detaches the non-cancellable wait
job. Natural status has already disarmed the command-local timer. A later stop
cannot overwrite an earlier latched turn stop merely because the final wait was
slow. On an ownership-loss path there is no result in which to fabricate an
Agent-timeout fact, but caller/turn truth still propagates through
`StartedOwnershipLost`.

If the direct shell exits while an executable descendant remains, Rust does not
turn that descendant into an undeclared background job. It terminates the group
and returns a bounded `BACKGROUND_PROCESS_NOT_SUPPORTED` tool failure. This
closes the foreground-only ownership promise even when the descendant redirected
its inherited pipes.

After SIGKILL, the runner waits for a definite quiescent observation rather than
lying that cleanup finished. A process stuck in an uninterruptible kernel or
filesystem operation can therefore delay return; portable Rust cannot forcibly
complete that kernel call. This limitation is preferable to dropping ownership
and orphaning known work.

Once the original group is quiescent, both pipe readers receive one final
bounded drain window. EOF is the normal result. A descendant that deliberately
escaped with `setsid` may still hold an inherited pipe; after the drain window
the runner closes its read ends and records `pipeDrainTimedOut: true`. If no
earlier primary cause exists, the result is `SHELL_PIPE_DRAIN_TIMEOUT`; otherwise
the earlier result remains primary. The runner does not claim that the escaped
process was killed. Tests that create this hostile case record its PID and
start identity only for observation; the helper also inherits a private test-only
stop channel and has a hard self-exit watchdog. Cleanup closes that channel and
waits for the same identity to disappear. It never sends a signal to a bare
recorded PID that could have been reused.

The promise is intentionally process-group scoped. A descendant can deliberately
call `setsid`, change process group, double-fork, or otherwise escape before a
signal; the runner is not a sandbox and cannot safely chase arbitrary system
PIDs. PID reuse is avoided only while the retained leader anchors the original
group. The harness itself being killed, or a kernel task stuck indefinitely, is
crash behavior rather than a bounded cooperative cancellation path. A
privilege-changing executable that makes a same-group member unsignalable or
unobservable likewise prevents a truthful quiescent result; Phase 6 does not
pretend that approval is a process sandbox or privilege boundary.
Likewise, a hostile Linux program can combine `CLONE_PARENT` with an immediate
session/group escape. Once it has deliberately removed every attributable group
identity, a shared library cannot safely distinguish that direct child from an
unrelated child owned by its embedding host; Phase 6 neither guesses nor reaps
it. This is an explicit arbitrary-native-code limitation, not a property of the
normal same-group path, and reinforces the default approval requirement.

## Output and result contract

The runner keeps stdout and stderr separately. Model text follows the upstream
small-result form:

```text
<stdout>
[stderr]
<stderr>
[timed out after Nms]
[killed by signal: SIGTERM]
```

Only present sections/markers are emitted; a completely silent normal result is
`(no output)`. The exit-code marker is last. Nonzero and command-local timeout
remain `is_error: false`; spawn, policy, unsupported-background, observed-output
limit, and cleanup-contract failures are model-facing failures. Caller/Agent
interruption uses the Agent's existing abort/timeout vocabulary after cleanup.

The durable result metadata contains only bounded process facts:

```json
{
  "kind": "foreground",
  "started": true,
  "exitCode": 0,
  "signal": null,
  "timedOut": false,
  "aborted": false,
  "outputLimitExceeded": false,
  "pipeSetupFailed": false,
  "pipeReadFailed": false,
  "signalDeliveryFailed": false,
  "pipeDrainTimedOut": false,
  "timeoutMs": 25000,
  "workdir": ".",
  "stdoutTruncated": false,
  "stderrTruncated": false
}
```

Metadata is layered so pre-parse failures do not invent effective settings. Every
shell result has `kind`, `started`, `exitCode`, and `signal`; a `started: false`
result has both status fields null. Once timeout/workdir parsing succeeds, those
effective fields may be added. Every `started: true` result has the complete
shape shown above, and after a successful final reap exactly one of `exitCode`
or `signal` is non-null. If that termination fact cannot be obtained, the path is
`StartedOwnershipLost` and there is no result. The maximum-length, fully
populated `started: false` envelope is used only as an encoded-size probe for the
separate pre-start claim ceiling described above. The event retained as the
fallback is always the smaller truthful result for that call; the probe's
placeholder workdir, timeout, or padding is never serialized or replayed.

`workdir` is the normalized workspace-relative display (`.` for the root), so
the durable result records what the approval preview authorized without storing
an ambient absolute path. Output text is not duplicated in metadata. Invalid
UTF-8 is decoded lossily and the final renderer enforces its compact-JSON byte
ceiling, including quote, backslash, control-character, and replacement-character
expansion. It reserves space for the stderr label, per-stream truncation notices,
status markers, and cleanup warnings before selecting bounded suffixes from the
two retained tails; when both streams are present the remaining budget is split
deterministically and unused space is reassigned. The stable notices are
`[stdout truncated; tail only]`, `[stderr truncated; tail only]`,
`[warning: output pipes could not be monitored; output is incomplete]`,
`[warning: a pipe read failed; output is incomplete]`,
`[warning: a process-group signal failed]`, and
`[warning: output pipe remained open; an escaped process may still be running]`.
Only applicable notices are emitted, and their compact-JSON bytes are reserved
before output selection.

Rendering order is fixed: stdout tail and its notice; the optional `[stderr]`
heading, stderr tail, and its notice; secondary pipe-setup, pipe-read, signal,
and pipe-drain warnings in that order; the command-timeout marker; and finally
the signal-or-exit marker. This preserves the upstream rule that the terminal
process-status marker is last while making cleanup degradation model-visible.

`stdoutTruncated` or `stderrTruncated` is true whenever that stream lost bytes
at the raw-tail cap, the final 64 KiB renderer omitted any retained bytes, or the
local end was unavailable/closed before EOF because of pipe setup, an output
cap, read failure, or the drain deadline. A setup failure therefore marks both
unmonitored streams truncated. Thus two individually sub-cap tails such as
40 KiB stdout plus 40 KiB stderr cannot be squeezed into one result while both
flags remain false. The corresponding visible notice is emitted next to that
stream. Invalid UTF-8 lossy replacement alone is not called truncation.

Primary shell-policy/lifecycle failures retain any bounded diagnostic tails and
add one fixed marker before the timeout/status suffix:
`[output pipe setup failed; process group stopped]`,
`[output limit exceeded; process group stopped]`,
`[background process is not supported; process group stopped]`, or
`[pipe read failed; output is incomplete]`. The durable `ToolFailure.code`, not
free-form OS text, remains the machine-readable reason.

For a started, settled process, `exitCode` and `signal` are mutually exclusive
and exactly one is non-null. A pre-spawn result has `started: false` and both are null.
`timedOut` means the command-local deadline won; `aborted` means caller or Agent
interruption won. Secondary `outputLimitExceeded`, `pipeSetupFailed`, `pipeReadFailed`,
`signalDeliveryFailed`, and `pipeDrainTimedOut` flags may coexist with either
primary fact without changing its precedence. Their visible warning markers are
still emitted when an earlier cause remains primary, so a normal timeout cannot
hide evidence that an escaped process may still hold a pipe.

The signal string has one locale-independent canonical form. The runner keeps the
raw positive signal number until the provisional `waitid` fact and final
`ExitStatus` agree. It then maps the target platform's explicit `libc` constants
for `SIGHUP`, `SIGINT`, `SIGQUIT`, `SIGILL`, `SIGTRAP`, `SIGABRT`, `SIGBUS`,
`SIGFPE`, `SIGKILL`, `SIGUSR1`, `SIGSEGV`, `SIGUSR2`, `SIGPIPE`, `SIGALRM`,
`SIGTERM`, `SIGCHLD`, `SIGCONT`, `SIGSTOP`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU`,
`SIGURG`, `SIGXCPU`, `SIGXFSZ`, `SIGVTALRM`, `SIGPROF`, `SIGWINCH`, `SIGIO`, and
`SIGSYS` to those exact `SIG<NAME>` strings. The target-only named constants
macOS `SIGEMT`/`SIGINFO` and Linux `SIGSTKFLT`/`SIGPWR` use the same exact-name
rule. Any other positive signal, including a Linux real-time signal, is `SIG`
followed by its decimal number, for
example `SIG34`; `strsignal` and localized OS text are never used. A non-positive,
overflowing, or numerically contradictory termination fact is ownership loss,
not a fabricated signal string.

Shell output is untrusted terminal text. It is durable and model-visible, but a
Phase 7 renderer must visibly escape ANSI/OSC sequences, bidi formatting
characters, and other terminal controls instead of printing them verbatim.
`Debug` never expands output. The prepared action reserves enough Session
capacity for every allowed result shape; after process start the Agent uses
preferred-only settlement so a real outcome cannot be replaced by a generic
budget fallback.

## Dependency and blocking choices

Phase 6 adds one exactly pinned direct production dependency already present
transitively in the lockfile: `libc = "=0.2.189"`, used for the read-only Unix
`SIGCHLD` query and the macOS `libproc` observer. It enables Tokio 1.53's `net`
feature for Unix `AsyncFd` and rustix 1.1.4's `process` feature in addition to
the existing `fs` feature. The standard-library `UnixStream::pair` supplies the
cross-platform no-child reactor preflight, avoiding both rustix's Linux-only
flagged-pipe API and a new raw-libc pipe unsafe seam. The standard
library owns process creation and the direct child; Tokio supplies only readiness
for the two nonblocking pipes; rustix
supplies typed `setsid`, `fchdir`, signal, and `waitid`; `libc` exposes the
platform's `proc_listpgrppids`/`proc_pidinfo` ABI. The unsafe calls are isolated
behind safe prerequisite and `Live | Quiescent | Unknown` interfaces. The
private Unix prerequisite module contains only the query-form `sigaction`; the
private macOS observer contains checked `sysctl`, `proc_listpgrppids`,
`proc_pidinfo`, `__error` access, and `MaybeUninit::assume_init` after an
exact-size success. Comments and tests fix every pointer lifetime, byte length,
return unit, and errno rule. A
standard-library-only async runner would require blocking two-pipe coordination.
`nix` would duplicate rustix, while
`process-wrap` 9.1 requires Rust 1.87 and the older 8.2 line does not supply this
exact TERM/grace/KILL, WNOWAIT identity, and same-group observer contract.

`std::process::Command::spawn` itself is synchronous. Fork, the two `pre_exec` syscalls, or the
exec-error handshake can theoretically block a Tokio worker in a broken kernel
or filesystem. Moving spawn to an abandonable blocking task would be worse: a
late task could create a child after its owner had stopped polling. The runner
therefore spawns inline using fixed local `/bin/bash` and an already-opened CWD
descriptor, owns every pre-spawn recheck, observer, and final-wait blocking
`JoinHandle` until it finishes, and
discloses that an abnormal kernel call is not cooperatively preemptible.

## Fixed limits

All limits are checked with bounded or checked arithmetic and receive exact and
one-over tests where the value is directly constructible:

| Boundary | Limit |
| --- | ---: |
| command UTF-8 bytes | 32 KiB |
| description UTF-8 bytes | 1 KiB |
| workdir UTF-8 bytes | 4,096 |
| workdir components | 64 |
| command timeout | 1 ms to 295,000 ms |
| default command timeout | 25,000 ms |
| shell-action preparation ceiling | 5,000 ms |
| TERM-to-KILL grace | 3,000 ms |
| post-quiescence pipe-drain grace | 1,000 ms |
| process-observer retry interval | 10 ms |
| one stream-read chunk | 8 KiB |
| observed stdout + stderr before forced stop | 8 MiB |
| retained raw stdout tail | 64,000 bytes |
| retained raw stderr tail | 64,000 bytes |
| final text `ContentBlock` compact JSON | 64 KiB |
| effective child environment entries | at most 24 by the fixed name set |
| effective child environment name/value bytes | 32 KiB |
| one Linux `/proc/*/stat` or `self/stat` payload | 4 KiB + 1 detection byte read |
| one Linux `self/mountinfo` record | 64 KiB + 1 detection byte read |
| Linux `/proc/sys/kernel/pid_max` payload | 32 bytes + 1 detection byte read |
| supported Linux `pid_max` | at most 4,194,304 |
| supported macOS process-count hint/`kern.maxproc` | at most 4,194,304 |
| peak macOS process-ID table | one (`max(hint, maxproc)` + 1) × `sizeof(pid_t)` buffer |
| prepared action result event | at most 128 KiB |

The existing Agent limits remain independently authoritative: at most 256 KiB
for one preferred result, 4 MiB of preferred result components per turn, five
minutes for one action classification, and thirty minutes by default for a turn.
The fixed preparation timer ends when a sealed Action is returned. A fresh Agent
action timer then starts immediately before that sealed future is first polled;
the command timer starts only after successful spawn. The 25-second command
default plus the three-second grace normally fits inside the Agent's 30-second
default tool duration. The 295-second command maximum similarly leaves five
seconds inside the five-minute Agent classification ceiling for ordinary cleanup
and result construction.
A caller that configures a shorter Agent tool/turn duration intentionally gets
that earlier classification. Same-poll-ready and one-tick-before/after tests fix
the observation priority and prevent cleanup from rewriting the first observed
cause; they do not pretend a cancellation token contains an arrival timestamp.
Once a process has started, that five-minute maximum is a termination-cause
classification ceiling, not permission to drop the owner: TERM/KILL, observer,
direct reap, and bounded pipe closure still finish before a normal result, and a
kernel D-state or ownership violation can delay or prevent ordinary return.

These bounds limit harness memory, captured bytes, and cooperative wall time;
they do not limit the approved native command's process count, CPU, address
space, disk writes, or network traffic. A fork bomb or other hostile native code
is outside this non-sandbox runner and is why approval remains mandatory by
default.

## Failure vocabulary

Stable model-facing codes distinguish input, policy, execution, and lifecycle:

- `INVALID_ARGS`: closed/type/null/value/byte-bound failure;
- `SHELL_WORKDIR_OUTSIDE_WORKSPACE`, `SHELL_WORKDIR_NOT_FOUND`,
  `SHELL_WORKDIR_NOT_DIRECTORY`, `SHELL_WORKDIR_CHANGED`;
- `SHELL_POLICY_DENIED`, `APPROVAL_REJECTED`, `APPROVAL_CANCELLED`,
  `APPROVAL_UNAVAILABLE`, `APPROVAL_REQUIRED`;
- `SHELL_PROCESS_OBSERVER_UNAVAILABLE`, `SHELL_ASYNC_RUNTIME_UNAVAILABLE`,
  `SHELL_PIPE_PREFLIGHT_FAILED`, `SHELL_SPAWN_FAILED`;
- `SHELL_OUTPUT_LIMIT`, `SHELL_PIPE_SETUP_FAILED`, `SHELL_PIPE_READ_FAILED`,
  `SHELL_PIPE_DRAIN_TIMEOUT`,
  `BACKGROUND_PROCESS_NOT_SUPPORTED`;
- existing `ABORTED_BEFORE_DISPATCH`, `ABORTED`, `TOOL_TIMEOUT`,
  `AGENT_TURN_TIMEOUT`, and `TOOL_OUTPUT_BUDGET_EXCEEDED`.

Raw OS errors, environment values, approval-provider text, and panic payloads do
not enter the Session. A native panic hook may still write a trusted extension's
panic payload to process stderr before `catch_unwind`, as already documented for
the Agent seam.

Environment admission happens before a usable registry exists, so invalid or
oversized allowlisted values are redacted `ToolRegistryBuildError` variants, not
model-facing `SHELL_*` codes. A later signal-delivery failure is represented by
the bounded `signalDeliveryFailed` metadata flag and visible warning while the
already-latched primary result remains authoritative.

`StartedOwnershipLost` is deliberately not a model-facing success or failure
code. It follows the fixed infrastructure path, leaves the call unresolved, and
causes the existing `Poisoned`/`UnresolvedToolCall` guards to block reuse. This
preserves the only truthful fact when a hostile embedding process stole the
wait status or invalidated the observer after spawn.
Setup/Action `Infrastructure` uses the same unresolved guard without claiming
that a process started; its retained `turn_stop` may still close the surrounding
turn as cancellation/timeout. Neither path invents a `SHELL_*` result merely to
make the log look complete.

The model-supplied command and description are already durable in the preceding
`tool/call`; they are not secrets and cannot be redacted without changing the
requested action. The approval preview is not duplicated into asked/decided
events. Tests prove that a conspicuous ambient fake credential is absent, not
that an approved command is unable to read or deliberately print a secret from
the filesystem.

## Verification plan

### Phase 6 acceptance gates (default-enabled)

Phase 6 is accepted only when default-enabled tests prove the user-observable
normal, failure, rejection, cancellation, timeout, and safety paths below.
Shared Agent, approval, reservation, and Session invariants may cite the
existing Phase 3/5 suites, but Shell-specific process-start and cleanup
boundaries require their own sealed-Action or real-`LocalToolRegistry` tests.

- The closed schema and argument parser, direct-execute fail-closed guard, one
  retained Workspace authority, workdir/environment/output resource limits,
  replay/provenance, and committed upstream fixture are all exercised.
- Action admission proves that a claim-profile panic happens before durable
  assistant/call admission, a dispatch binding cannot move between calls, the
  pre-start claim grows atomically, the complete Action result ceiling fails
  before approval, and cancellation racing a late allow never spawns. Allow,
  Deny, rejection, unavailability, and provider panic remain fail closed; the
  provider-panic fact may reuse the shared approval-path test from Phase 5.
- A real `LocalToolRegistry -> Agent -> bash -> Session` path covers success,
  nonzero exit, self-signal, post-spawn caller cancellation, Agent tool
  timeout, turn timeout, and a silent background process holding inherited
  pipes.
- Process tests cover both sides of the spawn boundary, natural completion
  disarming its command timer, the aggregate-output ceiling, cooperative and
  TERM-trapping direct/same-group processes, redirected and silent background
  work, escaped-pipe drain, ownership loss while the leader is still live, and
  the main runtime/pipe/read/signal/observer/reaper failure classes. A normal
  started result is never published before the group and direct child meet the
  documented settlement proof.
- Host-contract tests isolate `SIGCHLD=SIG_IGN` and `SA_NOCLDWAIT` at open and
  recheck. Linux additionally covers PID-1 identity, child-subreaper open and
  recheck, `Z` plus live threads, and real `CLONE_PARENT` children with both
  `SIGCHLD` and no exit signal through exact `__WALL` reap. macOS exercises its
  real `libproc` observer path.
- Repository-wide format/build/test/Clippy/diff checks, Rustdoc with warnings
  denied, a release binary/help/version check, a release LLVM-IR scan proving
  test hooks are absent, local macOS acceptance, and Ubuntu CI are hard gates.
  Windows remains unclaimed.

### Detailed evidence inventory

The bullets below preserve the full adversarial design inventory. They are not
all separate Phase 6 completion gates: validation must cite the acceptance
items above, while unimplemented combinations belong to the explicit future
hardening section after this inventory.

- schema order, required/optional fields, closed-object behavior, null/type
  parity, command/description/path/time exact limits, and rejected background
  injection;
- direct `LocalToolRegistry::execute("bash", ...)` returns
  `APPROVAL_REQUIRED` and leaves a sentinel untouched, proving the public legacy
  seam cannot bypass the Agent;
- a custom executor that merely declares `bash` receives the standard claim
  profile and cannot produce an Action; a transparent wrapper around the real
  local registry can forward its profile/preparation but cannot fabricate a
  different sealed runner. A malicious wrapper that caches call A's genuine
  setup and returns it for call B fails the private dispatch-binding check before
  workdir inspection, approval, or spawn and leaves B unresolved; same-call
  transparent forwarding still succeeds even when textual call IDs or arguments
  repeat. A profile panic occurs before batch admission, stores no opaque payload,
  and appends no assistant/call event;
- `LocalToolRegistry::open` creates exactly one retained Workspace shared by file
  and shell tools; after the ambient root name is replaced, both still observe
  the original inode rather than split authorities. The canonical assembly opens
  it before entering Tokio; an injected slow async-embedder startup uses an owned
  blocking job that is awaited rather than blocking a runtime worker or being
  detached;
- direct registry and real Agent success, silence, stdout+stderr, nonzero exit,
  self-signal, workdir, next-request replay, event order, and source provenance;
  pure mapping tests lock `SIGTERM`, `SIGKILL`, target-specific named constants,
  and the `SIG<number>` fallback, while a Linux real-time self-signal (and the
  normal macOS named-signal case) proves raw `waitid`/final-wait agreement;
- explicit Allow, Deny, Ask-allow, Ask-reject, unavailable, provider panic,
  capacity failure before ask, and cancellation racing a late allow;
- command-local timeout, Agent tool timeout, turn timeout, and caller
  cancellation with exact turn/result classification, same-poll-ready and one-tick
  boundary cases, plus cleanup that crosses a later caller/turn deadline without
  rewriting the earlier result cause; cancellation/turn expiry at action-Ready,
  before `tool/result`, and after settlement must prevent the next side effect
  without rewriting the completed process fact. Turn-first then caller-during-
  cleanup closes as `AGENT_TURN_TIMEOUT`, caller-first then deadline closes as a
  user abort, and a first same-poll observation chooses caller; setup
  infrastructure and ownership-loss paths preserve the same outer-stop fact;
  delayed normal and exceptional final-wait jobs repeat turn-first/late-caller
  and caller-first/late-deadline cases, proving the JoinHandle is always awaited
  and fresh sampling cannot overwrite the earlier stop;
- a direct natural exit observed before its command deadline permanently disarms
  that timer: an injected slow group observer may cross the old deadline without
  setting `timedOut`, while a surviving same-group member can still become
  `BACKGROUND_PROCESS_NOT_SUPPORTED`; first same-poll deadline/status keeps the
  documented deadline-first classification;
- cancellation immediately before spawn versus immediately after successful
  spawn, proving `started: false`/`ABORTED_BEFORE_DISPATCH` and
  `started: true`/group cleanup respectively;
- synchronous recheck/spawn seams in which caller, turn, or Agent-tool stop
  becomes ready just before return, on the same return, and just after it;
  spawn failure remains `started: false` with the return-boundary priority,
  while spawn success always enters started cleanup. A delayed final recheck
  blocking job crosses a stop, observes its cooperative token, is still joined,
  and proves that neither reactor setup nor spawn begins afterward; an injected
  Join panic returns Action `Infrastructure` with no result/process, releases the
  claim, poisons the call, and retains turn-first/caller-first outer-stop truth;
- a Tokio runtime with time but no I/O driver fails the preflight with
  `started: false` and no process; isolated/fault-seam Unix-pair creation,
  close-on-exec verification, or nonblocking setup failure (including `EMFILE`)
  is instead `SHELL_PIPE_PREFLIGHT_FAILED`. macOS and Linux tests assert the
  real pair's descriptor flags. Injected take-pipe, `O_NONBLOCK`, first/second
  `AsyncFd`, and post-standard-spawn adapter failures are caught as
  `started: true`, close both descriptors, terminate/observe/reap the group, and
  never use a generic fallback;
- shell-profile argument rejection, pre-cancel, preparation cancellation, turn
  expiry, and five-second preparation timeout all fit the initial claim and
  settle `started: false` without polling an Action; an unsealed public adapter
  that never returns the setup observes cancellation and is dropped after the
  ordinary cleanup grace, while an already-started sealed workdir JoinHandle is
  always awaited even when it crosses the five-second/caller/turn boundary. The
  latter proves no detached job, retains the first preparation result plus any
  later outer turn stop, and gives the later Action a fresh configured timer;
- the pre-start encoded-size probe grows every shell result claim before any
  assistant/call event is appended; one failed grow rejects the whole round, and
  successful, rejected, cancelled, and replayed Session JSON never contains a
  padded or placeholder workdir/timeout from that probe;
- TERM-cooperative direct child, TERM-trapping child, and TERM-trapping
  same-group descendant, proving no live member of the anchored group when
  `run_turn` returns;
- a direct shell that exits while a redirected descendant survives, proving the
  undeclared background process is killed and reported;
- a silent long-lived background descendant that holds both inherited pipes,
  proving the independent 10 ms `WNOWAIT` tick detects leader exit and cleans the
  group well before the descendant's natural duration;
- retained-leader `WNOWAIT` ordering; macOS `libproc` normal, zombie-only,
  live-descendant, `arg = 1` zombie lookup, PID-count/fill, and
  `EPERM`/unknown cases; Linux complete streaming `/proc` scans including a real
  descendant whose group leader is `Z` with `num_threads > 1`; Linux isolated
  real helpers using `CLONE_PARENT | SIGCHLD` and `CLONE_PARENT` with no exit
  signal prove the exact-PID `__WALL` path reaps each same-group harness child
  (a later `waitpid(pid, WNOHANG)` is `ECHILD`) before a complete rescan; a
  CLONE_PARENT helper whose main thread exits while another thread runs proves
  the nonleader `Z + num_threads > 1` case stays live and is not prematurely
  waited; and an
  assertion that no signal is sent after quiescence/reap;
- observer fault seams for a vanished unrelated Linux PID, missing retained
  leader, malformed/overlong stat, changed `pid_max`/`maxproc`, duplicate macOS
  PID, allocation failure, two independently complete passes, 4 KiB stat
  exact/one-over, and 64 KiB streaming mountinfo exact/one-over without an
  unbounded string allocation;
- isolated subprocess tests for explicit `SIGCHLD=SIG_IGN` and
  `SA_NOCLDWAIT`, pre-spawn disposition change, stolen wait status mapping to
  unresolved ownership loss, and proof that the registry never changes the
  host's handler; Linux isolated subprocess tests enable child-subreaper mode
  before registry construction and immediately before the final recheck,
  proving both paths reject before spawn and leave no adopted zombie;
- observer invalidation after spawn while the leader remains waitable sends the
  anchored final KILL, disarms, and reaps the direct child before returning
  unresolved ownership loss; a stolen `ECHILD` path sends no numeric signal and
  does not pretend it performed the host's reap;
- Linux retained-procfs identity, mount-ID/mountinfo parsing, accepted
  absent/`off`/`0` hidepid, rejected `1`/`2`/`4`/`ptraceable`/`gid`/unknown
  views, runtime mount change, namespace-PID-1 rejection through the default
  seam, and privileged isolated PID-1/hidepid namespace cases when CI exposes
  the required capabilities;
- an intentionally escaped-session descendant that holds a pipe, proving the
  reader closes at the drain deadline and reports the limitation while test
  cleanup uses its private stop channel plus watchdog and never signals a bare
  recorded PID;
- stdout/stderr tail exact/one-over, hostile chunking, invalid UTF-8, JSON escape
  expansion, fair simultaneous full pipes, output-cap immediate KILL without a
  TERM grace, post-quiescence output-cap pipe close without signalling, primary
  cause plus secondary read/signal/output/drain flags and visible warnings,
  40 KiB + 40 KiB render-stage truncation truth, signal `ESRCH`/`EPERM`/other
  fault handling without treating delivery failure as absence, infinite-output
  timeout, and bounded memory/result size;
- exact allowlist preservation, missing-PATH fallback, startup/loader/proxy/
  credential/`DSH_*` absence, deterministic overrides, non-Unicode and
  byte rejection as redacted registry-build errors, the fixed 24-name
  construction invariant, and absence of a conspicuous secret from Session JSON
  and `Debug`;
- workdir missing/file/outside/symlink/replacement cases and the documented
  non-sandbox limitation;
- pre-spawn prepare panic and spawn failure closure without persisting opaque
  text; a post-spawn internal panic is explicitly crash-tail behavior and is not
  misrepresented as quiescent cleanup;
- two serial shell calls with approval pairs, dynamic sequence rebinding, and no
  process start before the matching durable decision;
- a committed, generated upstream oracle for canonical small foreground results
  and explicit paired assertions for background/default-policy/output-spill and
  schema differences; the pair also asserts that upstream structured
  stdout/stderr text has no Rust metadata duplicate while Rust's rendered text
  and truncation flags remain truthful. An executable/startup pair proves upstream PATH resolution
  or `BASH_ENV` can affect execution while Rust's fixed `/bin/bash` and cleared
  hook cannot, and separately proves both ordinary invocations expose `$0` as
  `bash`;
- macOS local and Ubuntu CI real-process runs. Windows stays unclaimed.
- every scheduler, observer, signal, pipe, and blocking-job fault hook is
  `cfg(test)`-only; a release build plus symbol/LLVM-IR scan proves the hook
  types, phase names, and injected-error strings are absent from production.

### Future hardening (outside the current compatibility claim)

The following combinations improve diagnostic precision and race coverage but
do not block Phase 6 once the acceptance gates above pass. They must not be
described in README or the compatibility table as already verified. Any such
test that exposes a real production defect immediately promotes that defect to
a blocker.

- Exhaustive before/same-poll/after products for caller, turn, and tool
  deadlines at every setup, recheck, spawn, Action-ready, result, and final-wait
  boundary.
- Every stdout/stderr permutation of Unix-pair, close-on-exec, nonblocking,
  take-pipe, first/second `AsyncFd`, post-spawn adapter, and `EMFILE` faults.
- Every individual macOS `libproc` and Linux `/proc` fault permutation,
  including allocation/count churn and every malformed mount/hidepid spelling;
  privileged real PID-1 and hidepid namespaces remain capability-gated
  hardening after the default injected checks.
- Redundant hostile-chunk/fairness, signal-errno, output-cap/cancel/timeout
  same-poll combinations, and all serial two-Shell approval interleavings once
  the representative default paths above remain green.

Repository-wide `fmt`, locked all-target build/test, Clippy with warnings denied,
Rustdoc warnings denied, whitespace checks, deterministic oracle regeneration,
an ordinary non-test `cargo build --release --locked --bin dsh` (so dev-feature
unification cannot hide a missing production Tokio feature), and an independent
process/security review are required before Phase 6 can be marked complete.
