# Phase 7 interactive CLI design

This document records the designed and implemented Phase 7 terminal contract.
The goal is a small, real macOS/Linux command-line agent that connects
the existing DeepSeek Provider, Agent Loop, workspace tools, approval seam, and
process cleanup code. It is intentionally a line-oriented terminal program,
not a full-screen UI.

The central rule is that the terminal may display a model or tool **Session
fact** as live state only after that fact has committed to the append-only
`Session`. An approval preview is different: it is temporary capability data
that is never claimed as durable and may be displayed only after its correlated
durable `approval/asked` fact exists. Ctrl+C cancels the current turn, but the
CLI keeps awaiting the same pinned `AgentLoop::run_turn` future until the Agent
and any started tool have settled. Rebuilding, aborting, or dropping that future
is not a supported cancellation mechanism.

## Scope and non-goals

Phase 7 adds:

- a real `dsh` assembly for the existing DeepSeek Provider, `AgentLoop`, and
  six-tool `LocalToolRegistry`;
- a line-oriented multi-turn prompt on a real controlling terminal;
- committed assistant-text streaming plus committed tool-request/result status;
- one terminal approval answerer for file diffs and foreground Shell commands;
- Ctrl+C as current-turn cancellation, followed by a fresh prompt after cleanup;
- Ctrl+D/EOF, `/exit`, process termination signals, and terminal suspension with
  explicit cleanup behavior;
- a one-turn `--prompt` mode and bounded piped-stdin mode for scripts;
- stable, role-framed plain-text rendering without ANSI, OSC, cursor movement,
  or a spinner;
- PTY integration tests that use a loopback fake DeepSeek endpoint and the real
  release-facing binary path.

Phase 7 does not add:

- a full-screen terminal UI, raw mode, mouse support, bracketed paste, terminal
  resize layouts, markdown styling, or syntax highlighting;
- prompt queueing, steering an active turn, or editing input while a turn runs;
- live Shell stdout/stderr. Phase 6 returns those bounded streams only after the
  foreground action settles, so Phase 7 displays Shell lifecycle state rather
  than claiming byte-live process output;
- approval policies such as “always allow”, unattended write/process approval,
  or a dangerous command-line bypass;
- upstream profile/plugin/Web launcher compatibility;
- persistence, resume, compaction, history browsing, or transcript files;
- slash commands for goal, plan, compact, permissions, export, Skills, Hooks,
  subagents, or any other later-phase feature;
- Windows or an operating-system sandbox.

The in-memory Session still has the Phase 1 limits of 4,096 events and 16 MiB
of retained compact JSON. A long terminal conversation can therefore reach a
clean resource error before Phase 8 supplies persistence and compaction.

## Upstream evidence and deliberate differences

The baseline is DeepSeek Harness commit
`47f943859bef60e4160492346772ded9b24f765a`. Exact source and test paths are
recorded in `docs/upstream.md`.

The fixed upstream repository does not ship a human terminal UI. Its `dsh`
binary is a profile launcher for Web and one-shot headless products. The help
tests explicitly reject an old built-in `tui` command, and the reference docs
point users to the separate `deepseek-harness/turtle-ui` repository. That
external UI is not part of the pinned tree and is not a compatibility baseline.
The upstream `packages/terminal` subsystem is a PTY tool used by the model, not
the human conversation UI.

The directly reusable upstream facts are narrower:

- the headless profile joins task arguments, requires nonblank input, waits for
  the Agent to become idle and the Session to flush, prints the final committed
  nonempty assistant text once, and exits zero only for a completed turn;
- ACP supports sequential prompts in one Session, cancels one active Session as
  `turn/end { kind: "aborted", reason: { kind: "user" } }`, waits for cleanup on
  connection EOF, and offers only Allow once or Reject approval choices;
- committed `assistant/chunk`, final `assistant/message`, tool, approval, retry,
  step, and turn events remain the source of truth for Web projections;
- Web shows partial assistant text while running, treats the final message as
  authoritative, retains partial text with a stopped marker on cancellation,
  and never lets a late approval answer perform a cancelled side effect;
- the Web input bar supports queue/steer behavior while running, which this
  smaller terminal intentionally does not implement;
- launcher SIGINT is process-level cleanup and exit 130, while Web Stop is
  turn-level cancellation. Interactive Rust Ctrl+C follows the Web Stop meaning;
  one-turn script Ctrl+C follows the process-level exit meaning.

Rust intentionally differs as follows:

1. No-argument `dsh` is a human line-oriented terminal agent rather than the
   upstream profile launcher. Official profile, plugin, Web, and external TUI
   command lines remain rejected.
2. Rust streams already-committed text deltas to a terminal. Upstream headless
   prints only the final text; ACP deliberately hides raw chunks; Web performs a
   replaceable graphical projection.
3. Rust has local `/help`, `/exit`, and `/quit`. They are UI commands and create
   no Agent turn. Other leading-slash text is an ordinary model prompt; Rust does
   not pretend to implement upstream `/goal`, `/compact`, `/permission`, or
   `/export` handlers.
4. Rust accepts one active prompt at a time. Complete lines typed while a turn is
   running are discarded, not queued or steered. A status line tells the user to
   wait or press Ctrl+C.
5. Rust displays the complete Phase 5 canonical diff or Phase 6 command preview
   inside the approval question. This is a deliberate security enhancement over
   the Web approval panel/card split.
6. Interactive Ctrl+C cancels the current turn and returns to the prompt after
   definite cleanup. It does not terminate the Session. Script Ctrl+C cancels,
   waits, and exits 130.
7. Rust output is visible-control-safe plain text. It does not copy Web styling,
   collapse thresholds, markdown rendering, or ANSI color.
8. Piped stdin is a Rust convenience; upstream headless ignores stdin. Rust does
   not expose an unsupported `--stream-json` protocol.

The compatibility table therefore records the human terminal as an
`intentional-difference`, never as a false compatible claim, after the
PTY/lifecycle evidence landed. The separate narrow one-turn script row is
`compatible` only for the final text and completed/non-completed exit facts
proved by the committed Phase 7 oracle and Rust comparator.

## Public command-line surface

The Phase 7 syntax is deliberately small:

```text
Usage: dsh [OPTIONS]

Options:
  -p, --prompt <TEXT>      Run one prompt and exit
  -m, --model <MODEL>      DeepSeek model (default: deepseek-v4-flash)
  -w, --workspace <PATH>   Workspace root (default: current directory)
      --no-color           Force plain output (plain is also the Phase 7 default)
  -h, --help               Print help
  -V, --version            Print version
```

All option names and string values must be valid Unicode. Parsing is streaming:
at most 16 argv entries and 1 MiB plus 8 KiB of aggregate argv text are admitted;
`--prompt` is at most 1 MiB, `--workspace` is at most 4,096 bytes, and `--model`
retains the Provider's 256-byte limit. Unknown options, positional arguments,
duplicates, missing values, invalid combinations, and oversized values are
usage errors with exit code 2. Diagnostics name only the bounded offending
option/category rather than joining every argument. Offending argv payload is
omitted; any retained diagnostic text still passes through the same
visible-control sanitizer as model output. A hostile argv therefore cannot
inject OSC or erase the terminal.

Long value options accept either `--name VALUE` or `--name=VALUE`; an empty
`--prompt=` is invalid. Short value options accept only a separate value such
as `-p TEXT`: glued values and short-option clusters are rejected rather than
guessed. A bare `--` is accepted only as the final argument because this phase
has no positional operands. Help and version are successful only when their
single option is the whole command; mixing them with any other option is a
usage error. These rules keep parsing deterministic without adding a general
CLI framework.

The mode decision is deterministic:

- `--prompt` always selects one-turn script mode and never opens `/dev/tty`;
- with no `--prompt`, non-terminal stdin is read through a cap-plus-one loop to
  EOF as a bounded script prompt; it is never collected by an unbounded
  `read_to_string`. One owned ordinary reader thread holds only fd 0 and the
  bounded prompt buffer while the main runtime continues polling process
  signals. Success/error joins it. If a signal wins, the top-level CLI keeps the
  `JoinHandle` owned and calls `std::process::exit` immediately; process teardown
  ends that powerless read before any product state can continue;
- otherwise stdin, stdout, and stderr must all be terminals, and `dsh` enters
  interactive mode;
- an empty or all-whitespace script prompt is a usage error;
- a partial terminal combination is rejected with a message that recommends
  `--prompt`, rather than silently opening a hidden approval terminal.

"All are terminals" is not enough by itself. Before interaction, fd 0/1/2 and
the independently reopened concrete input/output devices must have identical
character-device identities and report the process's current session through
`tcgetsid` and its foreground group through `tcgetpgrp`; every value must agree
with `getsid(None)` / `getpgrp()`. This rejects the case where stdout is visibly
attached to one PTY while approvals are secretly read from a different
controlling terminal.

The default workspace is captured before the async runtime starts. The default
model is `deepseek-v4-flash`; `--model` still uses the Provider's normal bounded
validation and exact/fallback model behavior. The fixed Phase 7 system prompt is
small and product-owned: it identifies `dsh` as a workspace coding agent, asks
the model to use available tools when useful, and forbids claiming a side effect
that did not complete. It does not claim Skills, Hooks, persistence, a sandbox,
or another unimplemented feature.

Interactive mode uses `FileChangePolicy::Ask` and `ShellPolicy::Ask` with the
terminal answerer. Script mode uses `Deny` for both side-effecting policies. No
script flag silently grants writes or process execution.

## Assembly and ownership

`src/main.rs` remains a tiny process entry point. Product assembly lives in a
public library `cli` entry module whose internal driver types remain private.
This lets the separate binary crate call one small entry function while the
implementation uses crate-private Session observation without making that seam
a general extension API.

The startup order is:

1. parse and validate argv and determine the mode; help/version/usage errors
   still return here without building a runtime;
2. for piped-stdin script mode, create the current-thread runtime and persistent
   signal streams now, then dispatch the owned bounded reader thread above.
   INT/HUP/QUIT/TERM select the documented signal exit; TSTP stops the whole
   process and exits 148 on resume. EOF/cap/UTF-8 results always join the worker.
   The runtime and still-owned signal streams are retained for the later turn;
3. resolve the workspace path;
4. call `LocalToolRegistry::open` exactly once, outside a Tokio worker;
5. build `DeepSeekConfig::from_process_environment()` and
   `DeepSeekProvider::from_environment()`;
6. in both modes, use the shared fallible entropy helper for one independent
   16-byte fill, construct UUID-v4 bytes without another random lookup, and form
   `session-<uuid>` for `Session::new`. Failure is
   `CLI_ENTROPY_UNAVAILABLE` before a Session exists. For interactive mode only,
   then obtain exactly
   `MAX_SESSION_EVENTS * 16` bytes from the operating system CSPRNG into one
   checked 64 KiB authorization pool; a short/error result fails startup with a
   stable diagnostic and the raw OS error is not printed;
7. construct a fresh `Session`; in interactive mode attach its one live-event
   channel, while script mode keeps no unused duplicate projection; then build
   the `AgentLoop` with the registry's exact schema snapshot;
8. if step 2 did not already do so, create a current-thread Tokio runtime with
   all required I/O, time, and signal drivers; then enter the selected UI
   driver.

Startup never reads or logs the API key. `DEEPSEEK_API_KEY` continues to resolve
inside the Provider for each request. `DEEPSEEK_BASE_URL` retains the Phase 2
trusted-endpoint rules. Help, version, and argv errors run without opening a
workspace, reading credentials, starting Tokio, or installing signal handlers.
Synchronous workspace/Provider construction still runs outside a Tokio worker;
an abnormal blocking kernel/constructor call can delay handling a signal that
arrives in that short assembly interval and remains an explicit platform limit.

The Phase 7 production dependency change is limited to enabling Tokio `signal`
and `sync`, enabling rustix `termios` for safe input flushing and terminal
validation, and directly pinning the already-locked
`getrandom = "=0.4.3"` (Rust 1.85). Its fallible `fill` API obtains the startup
authorization pool without `Uuid::new_v4`'s panic-on-entropy-failure path.
Tokio `net` already supplies `AsyncFd`; pinned `libc` supplies small
cfg-platform `fpathconf` probes
for macOS `_PC_MAX_CANON` and both platforms' `_PC_VDISABLE`, whose unsafe
pointer-free calls are isolated and documented. Linux instead reads rustix's
safe `Termios::line_discipline` field and accepts only the kernel ABI value
`N_TTY == 0`. No full-screen TUI or terminal-parser crate is required. Real PTY
tests use only target-specific dev dependencies: `pty-process = "=0.5.3"` with
default features disabled and rustix's `event` feature for bounded polling.
They merge with the already pinned rustix during tests, while `event` is absent
from the normal release feature graph and `pty-process` does not enter the
release binary.

The same change removes every `Uuid::new_v4` call reachable in production: the
two `SystemAgentRuntime` sites (opaque IDs and retry jitter) and the Workspace
mutation staging name. Each uses fallible `getrandom::fill` plus UUID byte
construction. Agent entropy failure becomes a new stable `AgentRuntimeError`;
staging-name entropy failure becomes a definite not-committed tool error before
the private staging directory exists. Neither exposes raw OS text or unwinds
through an active turn. UUID calls used only by tests remain test helpers. The
tiny system entropy syscall can still be delayed by an abnormal/early-boot
kernel and remains an explicit availability limit, like the existing
filesystem/process syscalls.

The entropy boundary is one crate-private, instance-owned fill function. Normal
CLI, `SystemAgentRuntime`, and Workspace construction receive the production
`getrandom::fill` wrapper; unit tests can inject a failing fill into the exact
owner without changing process-global state. The seam is not public, is not
selected from environment or argv, and is absent as a test override in release
code.

The CLI does not spend entropy for user messages. Immediately before each
`TurnProposal::Enter`, it reads the authoritative `session.state().next_turn()`
and constructs `user-<turn>` plus `MessageSource::user`; after Phase 8 resume,
the same rule naturally continues from the replayed next-turn fact. This makes
multi-turn message IDs unique inside the Session and preserves the original
prompt bytes without adding another hidden random budget.

## Committed-event observation

`AgentLoop::run_turn(&mut self, ...)` exclusively borrows its Session, so polling
`agent.session()` from another branch is impossible. Provider or Tool wrappers
are not an acceptable replacement: they can observe data before Session append
succeeds and would make the display disagree with durable truth.

All live Session writes converge on `Session::append_prepared`. Phase 7 adds one
crate-private, attach-once sender of a deliberately small `CommittedUiEvent`
projection there:

- the CLI attaches it only to a fresh Session before constructing `AgentLoop`;
- the channel capacity is exactly `MAX_SESSION_EVENTS` (4,096);
- after append validation, allocation, projection update, event-vector push, and
  byte accounting all succeed, `append_prepared` fallibly extracts only the UI
  facts needed from the now-committed event and calls nonblocking `try_send`;
- the projection carries sequence/time/type identity, assistant text/reasoning,
  a compact final-source bitmap, bounded tool status, approval audit facts,
  retry state, and turn state. Because every live source sequence is in the
  0–4,095 Session domain, private `SourceSeqBitmap` uses the stable Rust 1.85
  `Vec::try_reserve_exact(64)` path and then fixes its logical length at exactly
  64 `u64` words. A reported capacity above 128 words is rejected as an observer
  allocation-policy fault rather than accepted as unbounded over-allocation.
  The projection enum/header stores only the Vec's three-word owner; the logical
  512-byte bitmap and at-most-1-KiB charged allocation are outside the measured
  fixed header. It never
  clones the event's opaque `originalData`,
  tool-result body, file content, Shell output body, or unrelated JSON tree;
- one accepted live append produces exactly one queued projection, even when its
  type-only variant needs no visible frame. Because the whole
  Session can never exceed 4,096 events, the fresh channel cannot become full
  even if the receiver temporarily consumes nothing;
- aggregate copied string payload cannot exceed the Session's 16 MiB retained
  payload ceiling. Each final-source bitmap has exactly 512 logical bytes and at
  most 1 KiB of charged Vec capacity, so even 4,096 final events add at most
  4 MiB rather than an unbounded value-dependent `Vec<u64>` expansion; and 4,096
  fixed projection/channel headers are bounded separately. A test assertion
  keeps one fixed projection header at or below 512 bytes. This is a bounded
  duplicate projection, not a second JSON log;
- projection allocation uses checked `try_reserve`. Allocation failure or the
  logically impossible `Full` result marks an observer-fault flag and drops the
  sender **after** the Session commit; it never returns `AppendError` and never
  panics to undo an already committed fact;
- `try_send(Closed)` means the UI receiver is already gone and simply detaches.
  Conversely, the dispatcher checks the shared producer-fault flag after the
  higher-priority signal streams and before ordinary event/input work on every
  poll. Once set, it immediately latches `CLI_AGENT_UNAVAILABLE`, drops the
  pending frame, stops producing presentation, drains queued projections only
  by `try_recv`/counting, cancels and awaits any active turn, inspects the
  Session tail, and exits 1 unless a stronger termination signal wins. It does
  not wait for up to 4,095 old frames or for natural channel closure, and never
  fills the missing projection with an uncommitted substitute.

Attach is rejected unless the Session is fresh, has zero events, and has no
previous observer. The observer does not replay seed events, expose mutable
Session access, write a transcript, or become a Phase 8 persistence API. If the
driver calls `run_turn`, it first records the Session's current next sequence as
`start_seq`. On the ordinary no-stop path, if the future returns `Ok(outcome)`
before the UI branch has consumed its final notices, the dispatcher keeps
draining/rendering until it observes the committed `turn/end` whose turn ID
exactly equals `outcome.turn()` and whose sequence is at least `start_seq`; only
then may it redraw a prompt or start another turn. Once a local stop/exit intent
is latched, presentation switches to the bounded discard path below: resource
cleanup and the now-accessible Session tail, not rendering every notice, prove
closure. If `run_turn` instead returns infrastructure/admission
`Err`, no future append is possible through that completed borrow: the driver
drains every notice already in FIFO with `try_recv`, inspects the now-accessible
Session tail and `start_seq`, reports the bounded pre-turn or poisoned-tail
error, and exits 1 **without** waiting for a `turn/end` that may not exist.

## Terminal input without raw mode

Interactive input does not use `tokio::io::stdin`,
`spawn_blocking(read_line)`, crossterm, or an input thread. Tokio stdin can
retain an uncancellable blocking read at shutdown, and generic escape-sequence
parsers can accumulate an unterminated CSI/bracketed-paste sequence without the
product's input bound. The separate piped-script admission worker described
above is permitted only because the process exits rather than continuing if a
signal wins while that powerless thread is blocked.

Interactive startup uses two independent terminal open-file descriptions with
rustix. It obtains fd 0 and fd 1's bounded `ttyname_r` paths and reopens those
exact devices:

- input: `RDONLY | NONBLOCK | CLOEXEC | NOCTTY | NOFOLLOW`;
- output: `WRONLY | NONBLOCK | CLOEXEC | NOCTTY | NOFOLLOW`.

Each reopen must have the same character-device `st_dev`, `st_ino`, and
`st_rdev` as its inherited source; input, output, and fd 2 must then all match
one another. A generic `/dev/tty` open is not assumed because a managed host may
permit the inherited concrete PTY while denying that alias or its ioctls.
Duplicating fd 0/1 is forbidden: `dup` would share `O_NONBLOCK` with the parent
shell's open-file description and could leave the shell altered after an
uncatchable exit.

Both are registered with Tokio `AsyncFd` only after the I/O driver exists. The
input descriptor stays in the user's existing canonical line mode. Startup,
every resume, and every transition to an accepting state verify all of these
facts:

- `ICANON`, `ISIG`, `ICRNL`, the existing human-input `ECHO`, and `ECHOCTL` are
  enabled. `ECHOCTL` prevents ASCII terminal controls from being echoed as raw
  terminal commands before the application can inspect them. `OPOST` and
  `ONLCR` are also required so application line feeds retain the tested
  line-oriented layout. `EXTPROC`, `IGNCR`, `INLCR`, and `ISTRIP` are disabled;
  `EXTPROC` is a hard rejection because it can bypass canonical and
  special-character processing even while `ICANON` remains set;
- target-exposed input/output case conversion (`IUCLC`, `XCASE`, and `OLCUC` on
  Linux) is disabled so selector and prompt bytes are read exactly;
- `VINTR`, `VEOF`, `VSUSP`, and `VQUIT` are exactly Ctrl+C (`0x03`), Ctrl+D
  (`0x04`), Ctrl+Z (`0x1a`), and Ctrl+\\ (`0x1c`);
- custom `VEOL` and `VEOL2` delimiters equal the `_PC_VDISABLE` value reported
  for that tty;
- fd 0/1/2 and both independently reopened descriptors still satisfy the
  same-session and
  same-foreground-group checks above; and
- on macOS, `fpathconf(_PC_MAX_CANON)` reports room for at least the 1,000-byte
  product prompt plus its newline. On Linux, where the POSIX/glibc interface
  conservatively reports `MAX_CANON == 255` even though the stock kernel N_TTY
  canonical buffer is 4,096 bytes including its terminator, the observed
  `Termios::line_discipline` must be exactly `N_TTY == 0`. An indeterminate,
  smaller, failing, or unknown line-discipline result rejects the terminal
  rather than guessing about truncation.

Phase 7 changes no terminal attributes, so there is no raw-mode restoration path
to get wrong. It validates rather than takes ownership of the terminal's
`ECHO`/`ECHOCTL` and output-processing settings. The visible-control guarantee
covers bytes actively written by `dsh`; printable keyboard/paste bytes echoed
directly by the kernel line discipline remain an explicit terminal/user trust
boundary and never count as sanitized application output.

One application task owns every read. It uses `readable().await` followed by
`try_io` and a fixed 8 KiB scratch buffer. It never uses `BufReader`, `LinesCodec`,
or another buffer with an implicit growth policy. A terminal prompt is limited
to 1,000 UTF-8 bytes; an approval record is limited to 64 bytes, enough for the
bounded selector words and internal correlation parser seam. This product chooses a
useful bound for the two claimed platforms rather than shrinking to POSIX's
portable 255-byte minimum: the runtime probe must prove the concrete terminal
can deliver the entire admitted line plus its newline. Exact, one-over, and
multibyte-boundary behavior must pass on macOS and Ubuntu PTYs before the
platform is claimed. A paste larger than the kernel's whole canonical queue is
an OS boundary: macOS can stop accepting bytes before the terminator, so `dsh`
cannot observe or diagnose that record. It must never be submitted, Ctrl+C must
flush it and restore a fresh prompt, and the limitation is documented rather
than pretending the application saw a line it did not. Script input has its own
larger one-megabyte cap and does not depend on terminal canonical buffering.

The reader retains at most one bounded partial record plus records already
returned by one 8 KiB read. More than one complete line in that scratch buffer
is processed in order; after the first idle line starts a turn, remaining lines
from the same read enter busy-state discard and cannot become future prompts.
While a turn is busy and no approval is active, every complete record is
discarded.

At the idle prompt, command recognition first removes the terminating LF and
then trims ASCII whitespace only for deciding what the line means. An empty or
all-whitespace line is ignored and redraws the prompt. `/exit` and the other
Phase 7 slash commands are recognized from that trimmed view. Any other line is
submitted as the user's prompt using its original non-LF bytes, including
leading and trailing whitespace; the UI does not silently rewrite model input.

A nonempty canonical read without LF is the record made available by VEOF. In
idle state it goes through the same classifier as an LF record: ASCII-whitespace
redraws, a trimmed slash command runs locally, and any other record submits its
original bytes. During a busy turn it is discarded; at approval it may
Reject/Cancel but can never authorize an Allow. An empty read is idle EOF or
active/approval cancellation-and-exit as specified below. Once a
record exceeds its state limit, the reader enters `DrainOversizedRecord` and
keeps discarding until an LF or a proven short VEOF record boundary. Any
application-visible capacity/truncation chunk is never reinterpreted as the
beginning of a later prompt or approval. Linux PTY `EIO` and terminal hangup are normalized with EOF,
subject to the stronger already-latched termination signal. Other read errors
use safe summaries, never raw bytes.

### Input fences

Continuous busy-state draining is not sufficient for approval safety: a partial
answer typed before the question could otherwise complete after the question.
An **approval** transition therefore performs this exact fence:

1. discard the local partial buffer while this state is not accepting;
2. write every byte of the complete prompt and approval preview through the
   bounded nonblocking writer;
3. revalidate terminal ownership, call `tcflush(IFlush)` on the input
   descriptor, and reset the local parser;
4. require 100 ms with no input; bytes arriving during this arming period are
   discarded and restart the quiet deadline;
5. flush once more, switch only approval input to cbreak/no-echo with
   `tcsetattr(TCSANOW)`, and write the complete bounded selector;
6. flush bytes typed before the selector finished, then mark it accepting.

The complete selector is the observable human boundary. Bytes read before it
completes are discarded rather than retained as a partial record. The quiet
arming period and Reject-by-default selection prevent a continuous paste or
stale Enter from turning a predictable shortcut into permission.

The ordinary idle prompt has no direct authority to perform a side effect. Its
fence instead resets the local parser and flushes old kernel input **before** it
writes `dsh > `, then accepts input immediately after the complete prompt write.
This makes the displayed prompt a truthful readiness boundary: an immediate
human keystroke or deterministic PTY write after seeing it is not erased by a
later flush. A continuously pretyped ordinary prompt may still become a model
turn, but it cannot pass the separate approval fence.

`tcflush` is not an atomic boundary with a human or another writer: a continuous
pretyped stream can still place bytes after the flush. Therefore the quiet
arming stage is also required before selector shortcuts. Each approval still
receives one new internal UUID-v4 nonce from the startup CSPRNG pool after the
matching committed `approval/asked` fact and request envelope have joined. It is
an internal correlation/parser fact, not a displayed user command. After the
selector is accepting, `y`, `yes`, or `allow` moves to Allowed once; Enter still
has to confirm the visible selection. Prefixes, wildcard-like text, unknown
terminal sequences, and no-LF VEOF records never Allow. A process that can
observe the complete selector and deliberately send later keys controls the same
trusted terminal as the user; dsh does not claim to defend against a hostile
terminal owner.

The input flush after the preview can discard an extremely fast legitimate
keystroke, but it cannot by itself grant authority. `dsh` does not call an
uncancellable `tcdrain`: complete write acceptance by the tty driver is the
linear boundary, while pixels physically appearing on a remote/stalled terminal
remain a device/system limit. If preview output, input flush, terminal
validation, or selector preparation fails, the approval fails closed. The same
fence runs after Ctrl+C and before returning to the idle prompt, which also
handles hosts that set `NOFLSH`.

## Signal, EOF, and job-control state machine

The single UI dispatcher directly owns persistent Tokio Unix streams for
`SIGINT`, `SIGTERM`, `SIGHUP`, `SIGQUIT`, and `SIGTSTP`; there is no forwarding
task or signal queue to fill, block, detach, or leak a signal into a later turn.
Tokio coalesces repetitions of the same pending signal, which is sufficient
because each transition below is idempotent. If different streams are ready in
one poll, the fixed order is QUIT, TERM, HUP, TSTP, then INT. The dispatcher
consumes signals throughout terminal writes, approval, `run_turn`, cleanup, and
the final event drain. A signal observed during cleanup is handled in that same
state and never applied to the next turn's fresh cancellation token.

Every dispatcher poll checks termination/stop signals before approval answers
or ordinary input, so a same-poll Allow cannot beat cancellation. Event intake
and one output write chunk alternate when both remain ready; neither continuous
model events nor a writable tty may starve signals, the pinned Agent future, or
the other side. Before arming a fresh idle input fence, the dispatcher has seen
the prior `turn/end`, cleared that turn's approval halves, and consumed every
currently ready signal. Repeated TSTP received while cleaning/suspended is
coalesced with the same suspension request and drained immediately after resume
before terminal validation; it cannot cause an automatic second stop.

Tokio's Unix registration changes the process-wide disposition for the rest of
this short-lived process even after a stream is dropped. Consequently real
signal tests run only in the PTY child, unit tests inject a fake signal source,
and shutdown proceeds directly to the selected process exit after all owned
async work has settled.

The stable behavior is:

| State | Input | Result |
| --- | --- | --- |
| idle | Ctrl+C/SIGINT | flush input, keep Session, redraw prompt |
| active turn | Ctrl+C/SIGINT | cancel this turn's token once; keep awaiting; return to prompt after cleanup |
| approval | Ctrl+C/SIGINT | cancel the turn; approval resolves `cancelled`; no side effect |
| script turn | SIGINT | cancel, await cleanup, exit 130 |
| idle | Ctrl+D/read zero or `/exit`/`/quit` | clean exit 0 |
| active/approval | Ctrl+D/read zero | cancel, await cleanup, then clean exit 0 |
| any | SIGTERM | cancel active turn, await cleanup, then exit 143 |
| any | SIGHUP | cancel active turn, await cleanup, then exit 129 |
| any | SIGQUIT/Ctrl+\\ | cancel active turn, await cleanup, then exit 131 |
| idle | SIGTSTP | flush, self-send `SIGSTOP`, validate/redraw after resume |
| active/approval | SIGTSTP | cancel and await cleanup, then self-send `SIGSTOP`; validate/redraw after resume |
| script turn/output | SIGTSTP | cancel and await if active, self-send `SIGSTOP`, then exit 148 after resume |

Self-suspension uses uncatchable `SIGSTOP` only after the active Phase 6 process
group has settled, so Ctrl+Z cannot leave an approved Shell running without its
supervisor. `SIGCONT` resumes after the `SIGSTOP` call and the UI then revalidates
the complete terminal contract and performs a fresh input fence before accepting
anything. If `bg`/`SIGCONT` resumes `dsh` while it is not the foreground group,
the driver writes nothing and self-stops again after rechecking whether a
terminating signal won. A TERM/HUP/QUIT observed during TSTP cleanup cancels the
suspension and exits with its stronger code. Uncatchable external `SIGSTOP` and
`SIGKILL`, a machine crash, and a stuck kernel call cannot run async cleanup and
remain explicit system limits.

A further Ctrl+C during cleanup is idempotent and may coalesce; at most it
refreshes the “waiting for cleanup” status. It never drops `run_turn` or forces
an unsafe exit. The same turn token is never reused for the next prompt. When a
terminating signal is observed, its exit intent outranks a later durable
non-completed turn or output error; the first
observed intent is retained, with the fixed same-poll order above. `/exit` and
empty-record EOF are the only interactive shutdown inputs that mean success 0.

## Approval broker

Interactive assembly installs one `TerminalApprovalProvider`. `request` remains
prompt and lazy:

- the trait method itself performs no send or side effect. On the returned
  future's **first poll**, it checks cancellation first; only if still live does
  it `try_send` one bounded `ApprovalRequest` plus a fresh oneshot response to
  the UI. One Agent can have only one active question, so capacity one is enough;
- full/closed delivery resolves `Unavailable`;
- the future uses cancellation-first selection between the supplied child token
  and its correlated oneshot, so a late Allow cannot beat cancellation;
- request IDs are matched exactly. A response for an older question is dropped.

The UI joins two independently arriving bounded facts by exact request ID: the
broker envelope (which owns preview + response sender) and the matching
committed `approval/asked` event. It displays nothing until both exist. FIFO
Session events therefore show the prior `tool/call` before the approval. A
committed `asked` followed by `decided: unavailable` without an envelope is a
closed/full delivery outcome, not a question that may remain on screen; either
half is also cleared by its matching decided/turn-end event.

Cross-channel delivery may be observed in the opposite order from production:
the dispatcher retains exact decided-ID tombstones for the current turn. An
envelope whose response receiver is closed, whose ID is already decided, or
whose turn has ended is dropped immediately and can never reopen a fence. Before
clearing tombstones at `turn/end`, the dispatcher drains the capacity-one broker
queue with `try_recv`; all request futures for that completed turn have already
returned, so no later producer can enqueue another envelope. At most the
Agent's 256 tool calls and 256 KiB of validated ID text are retained for this
join state, then released before the next turn.

After the join, the UI writes the complete role-framed sanitized reason and
preview, runs the quiet fence, then writes the three-choice selector. Arrows,
Tab, `h/j/k/l`, and `y/n/c` move the highlight; Enter confirms; Escape cancels;
unknown or pasted control sequences fail closed and restart the fence. No answer
persists beyond one request. Selection is disabled as soon as a response is
sent; the UI waits for the committed decision and later tool result instead of
implying that the side effect already completed.

## Live projection and rendering

The terminal renderer consumes append-origin `CommittedUiEvent`s in sequence.
Every untrusted line is nested inside an unmistakable product-owned frame such
as `assistant |`, `reasoning |`, `tool |`, or `preview |`; UI prompts and the
approval selector use separate trusted frames. Model text can spell the word
“Approve”, but it cannot create the accepting state. The renderer keeps only
bounded per-attempt text state and at
most the Agent's declared tool-call map: 4 MiB and 128 blocks for one DeepSeek
attempt, 64 calls in one step, and 256 calls in one turn. Retry and step closure
release their comparison/map state before another attempt/step is admitted.

- `assistant/chunk` text deltas stream to the answer area after sanitization.
- reasoning deltas use a clear `[reasoning]` label and never enter script stdout.
- `assistant/message` is authoritative. Deduplication is keyed by
  `(turn, step, successful-attempt source sequence set, text-block index)` and
  uses the final event's `source_event_seqs`; chunks from an earlier failed retry
  never participate. If the committed final text exactly matches its cited
  streamed deltas, it is not printed twice. A prefix extension prints only its
  suffix. A mismatch prints an explicit `[final answer corrected]` section; a
  line-oriented UI never erases earlier evidence with cursor control.
- The 4 MiB / 128-block attempt cache is only a deduplication aid, not a smaller
  Agent message limit. When the next retained byte or block would cross either
  UI bound, that attempt enters `DedupDegraded`: subsequent committed chunks
  continue to stream but are no longer accumulated for comparison. When its
  authoritative `assistant/message` arrives, the renderer prints the complete
  final text once under `[final answer restated; streaming comparison limit
  reached]`. It does not guess a prefix, silently omit text, or turn a valid
  129-block Agent message into an Agent failure. Retry/step closure clears the
  degraded marker with the rest of the attempt state.
- `llm/retry` closes the current partial section with a retry notice. The next
  attempt starts a new section. It cannot graphically replace old terminal text.
- `tool/call` prints `[tool requested]`, the sanitized name, and a bounded
  argument summary only after the durable intent exists. It never says
  “started” or “running”; those facts exist only in the later result metadata.
- `tool/result` prints success or the stable failure name/code. The renderer does
  not duplicate the model-facing file contents or Shell output body.
- `approval/asked` opens the request fence described above;
  `approval/decided` confirms Allowed once, Rejected, Cancelled, or Unavailable.
- an ordinarily rendered `turn/end aborted` leaves already displayed partial
  text in place and prints `[stopped]`. A locally latched stop uses the single
  discard-path summary below instead of printing both. Completed, max-token,
  blocked, and stable provider failures have distinct plain notices.

One dispatcher owns the pinned `run_turn` future, committed-event receiver,
approval-envelope receiver, terminal input, all signal streams, and at most one
incrementally sanitized output frame. It continues polling the run future and
signals even when `/dev/tty` is not writable; it pauses event consumption while
one frame is pending, relying on the bounded Session channel rather than an
unbounded output queue. An 8 KiB output chunk is written per readiness turn so
input/EOF and housekeeping cannot starve. Input-side terminal `EIO` follows the
EOF path; output-side `EIO`/`EPIPE` is `CLI_OUTPUT_FAILED`. An actually observed
HUP/TERM/QUIT remains the stronger latched exit reason in either race, and a
dead fd is removed from future readiness polling to avoid a busy loop.

Every frame has a checked source bound and a fixed five-second **absolute** write
deadline, not a progress-reset timer that a one-byte trickle could extend
forever. When `run_turn` becomes Ready on an ordinary completion, one additional
five-second absolute **final-drain deadline** covers all remaining frames through
the matching `turn/end`; it is never restarted per queued event. Expiry or
output error cancels an active turn. An approval is never answerable until its
entire preview and selector have been written.

Any locally latched INT/TSTP/EOF/termination/output-failure intent immediately
drops the partly written nonessential frame, stops generating presentation
frames, and consumes later committed notices only to a saturating skipped-event
counter while continuing to poll and await `run_turn` cleanup. No event gets a
new write deadline in this state. After cleanup, the driver drains the now-fixed
FIFO with `try_recv` and verifies the correlated Session tail. Interactive INT
may then write exactly one bounded `dsh | stopped; skipped N updates` summary using
one five-second absolute deadline before the fresh idle fence; EOF and
TERM/HUP/QUIT write nothing further; TSTP writes nothing before self-stop and
validates/redraws only after a foreground resume. A terminating signal observed
while preparing the optional INT summary still wins. Thus a 4,096-event backlog
cannot turn cancellation, shutdown, or suspension into thousands of serial
five-second waits.

The dispatcher can be fair only if its child future eventually returns
`Pending`. Phase 7 therefore adds one small cooperative-work budget inside the
existing Agent loops: after at most 32 immediately-ready provider chunks, and
after each immediately-ready retry/step or bounded group of tool resolutions,
the Agent awaits `tokio::task::yield_now()` and rechecks cancellation/deadlines.
This is not a detached task and does not alter Session ordering. It makes the
pinned `run_turn` branch return `Pending`, allowing the same outer dispatcher
poll to service ready signals, input, and the absolute output deadline. An
always-ready fake Provider and always-ready tool sequence must prove that these
branches are observed before the 4,000-chunk/16-step hard ceilings are consumed.

Script mode does not stream chunks. After the turn settles it applies the exact
narrow upstream summary inside that turn interval: scan committed
`assistant/message` events in order, concatenate each message's text blocks with
no separator, retain the latest concatenation that is not empty, and write
`retained_text + "\n"` exactly once. Thus no text still produces one LF, an
already-newline-terminated answer produces an additional LF, and a later empty
assistant message does not erase the last nonempty one. Only a durable
`completed` turn exits 0; other turn reasons exit 1 unless a latched process
signal has the stronger 128+signal exit described above. Stable failure status
goes to stderr only for a durable `error` reason; completed, aborted, blocked,
and max-token outcomes add no status text. Exact provider-error wording remains
outside the narrow compatibility claim. Output order is final stdout frame,
optional stderr failure frame, then process exit, matching the upstream
flush-before-output ordering after the Rust in-memory turn has fully settled.
If `run_turn` returns an infrastructure/admission `Err` without a durable turn
outcome, there is no upstream-compatible summary interval: stdout is empty,
stderr receives one stable bounded CLI diagnostic, and the process exits 1.

Script stdout/stderr do **not** toggle `O_NONBLOCK` on inherited descriptors:
status flags belong to the shared open-file description and changing them could
temporarily alter the parent shell or another process that inherited the same
terminal/pipe. After the Agent and every owned side effect have already settled,
one ordinary, explicitly owned final-output thread writes the two bounded
immutable frames in stdout-then-stderr order with fixed-size `rustix::io::write`
calls on duplicated raw `OwnedFd`s. It never acquires `StdoutLock`/`StderrLock`:
otherwise process shutdown could wait on a standard-library lock held by the
blocked worker and defeat the deadline. This uniform path also supports regular
files, terminals, pipes/sockets, and `/dev/null` without trying to register a
non-pollable character device with `AsyncFd`. The main dispatcher retains its
signal streams and an absolute five-second deadline. On
success it must join that thread before returning. On timeout, broken output, or
a terminating signal it immediately returns the selected process exit; normal
process termination then tears down the sole blocked writer thread. As in the
piped-input admission case, its `JoinHandle` remains owned until the top-level
CLI invokes immediate process exit; it is never dropped while product execution
continues. Neither worker owns a Session, Agent, approval sender, terminal mode,
child process, or mutable product state. An abnormal kernel write that
does not return is therefore bounded by process lifetime rather than by an
unsafe descriptor-flag mutation. A broken/expired stdout or stderr produces no
recursive diagnostic on that broken stream.

## Visible-control sanitizer

Every untrusted string crosses one common renderer, including:

- model text and reasoning;
- tool names, arguments, results, and failures;
- approval reasons, diffs, commands, and descriptions;
- workspace paths and filenames;
- Provider messages and command-line diagnostics.

The renderer preserves ordinary printable Unicode and LF. It renders TAB, CR,
ESC, DEL, all other C0/C1 controls, every Unicode `Cf` format character, U+034F,
the bidi marks U+061C/U+200E/U+200F and embedding/override/isolate ranges,
line/paragraph separators, tags, and other explicitly tabled zero-width
formatting code points as visible `\\t`, `\\r`, or `\\u{...}` text. The internal
range table is exhaustive for the Unicode version shipped by Rust 1.85 and has
boundary tests; unknown future format ranges are not silently added. Thus ANSI,
OSC 52 clipboard writes, title changes, carriage-return rewrites, and bidi
spoofing in bytes authored by `dsh` cannot be interpreted by the terminal.

Interactive role framing is part of sanitization, not cosmetic styling. At the
beginning of every untrusted interactive line and after every preserved LF, the
streaming renderer emits the trusted current-role prefix before any more
untrusted text. Script stdout still escapes unsafe code points but deliberately
omits role prefixes so the narrow final-text comparison remains pipe-friendly.
Rendering uses a fixed scratch buffer rather than allocating the worst-case
escaped expansion in one String.

Phase 7 emits no ANSI even when color is allowed. `NO_COLOR`, `--no-color`,
`TERM=dumb`, and every non-terminal mode are therefore naturally readable. A
later phase may add colors only behind these gates and after equivalent tests.

## Fixed resource limits

| Resource | Phase 7 limit |
| --- | ---: |
| argv entries / aggregate text | 16 / 1 MiB + 8 KiB |
| workspace / model argv value | 4,096 / 256 bytes |
| interactive UTF-8 prompt | 1,000 bytes |
| approval record | 64 bytes |
| script prompt from argv/stdin | 1 MiB |
| live event queue | 4,096 events |
| live copied strings / all final-source bitmap allocations | 16 MiB aggregate / 4 MiB aggregate |
| one live fixed event header | at most 512 bytes |
| terminal read scratch | 8 KiB |
| visible rendering scratch | 8 KiB |
| retained current attempt text / blocks | 4 MiB / 128 |
| displayed tool map per step / turn | 64 / 256 |
| tool argument status preview | 512 visible bytes |
| approval request queue | 1 |
| approval decided-ID tombstones | 256 IDs / 256 KiB per turn |
| startup approval entropy pool | 65,536 bytes (4,096 UUID inputs) |
| one output frame absolute deadline | 5 seconds |
| retained PTY transcript tail / one interactive fake HTTP request | 1 MiB / 8 MiB |
| one script-smoke fake HTTP request | 2 MiB |
| local UI slash commands | 3 (`help`, `exit`, `quit`) |

All byte arithmetic is checked. Exact and one-over tests must use both ASCII and
multibyte UTF-8 where the boundary matters. Session, Provider, Agent, tool,
approval, diff, and Shell limits remain independently enforced; Phase 7 never
raises them to make a terminal test pass. Once the 4,096-event or 16 MiB Session
ceiling prevents admitting another complete turn, the CLI reports a stable
resource failure and exits 1; it does not keep offering prompts that can no
longer be durably recorded.

## Failure and exit vocabulary

User-facing startup failures retain a small stable class and a safe summary:

- `CLI_USAGE` (exit 2);
- `CLI_INPUT_INVALID` / `CLI_INPUT_TOO_LARGE` (exit 2);
- `CLI_TERMINAL_UNAVAILABLE` / `CLI_TERMINAL_UNSUPPORTED` (exit 1);
- `CLI_ENTROPY_UNAVAILABLE` (exit 1);
- `CLI_WORKSPACE_UNAVAILABLE` (exit 1);
- `CLI_PROVIDER_UNAVAILABLE` (exit 1);
- `CLI_AGENT_UNAVAILABLE` (exit 1);
- `CLI_OUTPUT_FAILED` (exit 1).

Opaque tool/extension infrastructure text is not copied into diagnostics. A
normal durable model failure displays its already-scrubbed code and message.

Stable process exits are:

- 0: help/version, clean interactive `/exit`/empty-record EOF, or completed
  script;
- 1: startup/runtime/output failure or a script turn whose durable reason is not
  `completed`;
- 2: argv or prompt admission error;
- 129: SIGHUP after cancellation cleanup;
- 130: script SIGINT after cancellation cleanup;
- 131: SIGQUIT/Ctrl+\\ after cancellation cleanup;
- 143: SIGTERM after cancellation cleanup;
- 148: script SIGTSTP after cancellation cleanup, suspension, and resume.

Interactive turn cancellation is not a process failure and returns to the prompt.
An unresolved Agent infrastructure/ownership-loss tail poisons the in-memory
Session; the CLI reports it and exits because Phase 8 repair does not exist yet.

## Security and privacy limits

- The terminal UI is a projection of already-authorized Session facts, not a
  second source of truth.
- All file and Shell side effects retain Phase 5/6 policy and approval ordering.
- Script mode cannot approve side effects.
- The CLI never intentionally copies an API key, child environment value, raw
  panic payload, or private transport error into Session facts or its own
  structured diagnostics. As already documented for Phase 3, a trusted native
  extension panic still runs Rust's process-global panic hook before the Agent's
  `catch_unwind` and may write its own payload directly to inherited stderr;
  Phase 7 does not claim hostile in-process code isolation or replace that
  global hook.
- The CLI never writes a history, transcript, prompt cache, or approval file.
- The control-safe-output claim covers bytes actively written by `dsh`.
  `ECHOCTL` keeps ASCII controls in kernel echo visible, but printable Unicode
  and its terminal-specific interpretation still belong to the user's
  terminal/input trust boundary; echoed keyboard or paste bytes are not
  mislabeled as sanitized application output.
- A permitted Shell is still unsandboxed native code. Terminal approval does not
  add filesystem, network, CPU, memory, or credential isolation.
- Canonical line mode protects bounded human input only on the tested stock
  macOS and Ubuntu terminal paths. The explicit termios, session, foreground,
  and platform-specific canonical-capacity checks reject known incompatible
  setups; an otherwise deceptive custom kernel line discipline is outside the
  claim.
- Output readiness cannot make an incompletely written approval answerable. A
  tty driver or remote terminal may still stall after accepting bytes; physical
  display is not provable without an unsafe, potentially uncancellable drain.
  Async output is bounded by the documented absolute frame deadline; an
  abnormal regular-file or kernel syscall that never returns remains a system
  limit.
- `SIGSTOP`, `SIGKILL`, process crash, machine loss, and uninterruptible kernel
  waits cannot execute async cleanup.

## Verification plan

### Upstream evidence

- Type-check and run the committed Phase 7 generator twice against the exact
  clean upstream commit.
- Record ACP two-turn, partial-cancel, and approval audit facts; headless final
  output/exit facts; and stable diff newline/render facts.
- A Rust comparator must `include_str!` the committed fixture, assert schema
  version and upstream commit, and exercise the real `AgentLoop`/Session plus
  loopback executable path. It compares: two-turn prior context; ordered partial
  chunk then aborted/user closure then successful continuation; paired
  asked/decided IDs and allow-only side effect; and the exact headless summary
  text/LF and completed-versus-other exit facts.
- Compare Rust only inside that explicitly normalized scope. Human terminal UX,
  Queue/Steer, raw chunk exposure, direct diff-in-approval, startup/profile
  syntax, and local slash commands remain differences. The source-derived React
  diff-card fact is evidence for display intent, not a fake runtime comparator.

### Unit and component tests

- argv count/aggregate/value exact and +1, duplicate, missing, unknown,
  non-Unicode, control-injection, `--name=value`, rejected glued/clustered short
  options, final/non-final bare `--`, empty `--prompt=`, and help/version mixed
  with other options;
- sanitizer ANSI, OSC 52, CR, C0/C1, bidi, line separator, multibyte, and
  scratch-boundary cases;
- terminal line exact 1,000/+1, multibyte, kernel-truncated huge-paste
  non-submission plus Ctrl+C recovery, and application-visible drain-to-boundary,
  empty/all-whitespace redraw, slash-command trimming with ordinary-prompt byte
  preservation, whitespace/slash/ordinary non-LF VEOF, multiple-line read,
  invalid UTF-8, empty EOF, Linux `EIO`, other read error, macOS
  canonical-capacity probe, Linux `N_TTY` exact
  acceptance plus unknown/non-N_TTY rejection, every required termios bit
  and control mapping including disabled VEOL/VEOL2, EXTPROC and target case
  conversion rejection, mismatched controlling PTYs, foreground/session
  ownership, and `tcflush` failure;
- committed-event observer: post-commit ordering, reservation settlement,
  minimal projection (no opaque body), 16 MiB strings / exact 64-word logical
  bitmap / 128-word capacity rejection / 4 MiB aggregate bitmap allocation /
  header bounds, capacity
  theorem, projection allocation failure, `Full` fault-after-commit, closed
  receiver, rejected append emits nothing, exact outcome-turn matching after an
  Ok result, admission failure before `turn/start`, run-ready-before-turn-end
  drain, infrastructure-Err FIFO drain without an invented turn end, and a
  4,095-notice backlog plus producer fault that immediately discards rather than
  creating per-event write deadlines;
- final-message source-sequence/block-index dedup, prefix/mismatch, retry partial,
  exact 4 MiB/128-block cache boundaries and `DedupDegraded` at +1, stopped
  partial, tool-request wording, stable failure, role framing, and script
  final-only rendering;
- session-ID entropy format/uniqueness/failure in both modes, multi-turn
  `user-<next-turn>` uniqueness and exact prompt bytes, injected
  `SystemAgentRuntime` ID/jitter entropy failure, Workspace staging entropy
  failure with definite not-committed/zero-stage/unchanged target, release-source
  proof that production contains no `Uuid::new_v4`, approval entropy exact
  startup fill/failure, distinct unused challenges,
  delivery full/closed, matching asked fence, allow/reject/cancel,
  asked-without-envelope decided-unavailable, invalid-answer re-prompt/re-fence,
  decided/turn-end observed before a late broker envelope,
  internal correlated-record parsing, short `y` selection after the fresh fence,
  complete/partial/continuous stale input, no-LF Allow refusal, late Allow vs
  cancellation, output failure, and EOF;
- fake-source signal state transitions for idle/active/approval/script INT, TERM,
  HUP, QUIT, TSTP, same-poll priority, coalesced repeats, resume validation,
  repeated INT during cleanup, final event drain, fresh turn tokens, and a
  4,096-notice backlog whose INT/TSTP/TERM/observer-fault discard path never
  creates per-event write deadlines;
- an always-ready 4,000-chunk Provider and immediately-ready retry/tool/step
  paths hit the Agent cooperative budget, allowing a ready signal, input event,
  and output deadline to win before the bounded hot loop finishes;
- script headless summary tests for empty text, later empty text, multiple blocks,
  already-final-newline, exact LF, infrastructure Err with empty stdout, exit
  0/1/2/129/130/131/143/148, slow/non-reading piped-input read
  success/cap+1/join and every input-time signal, pipe timeout, inherited
  descriptor flags unchanged, successful writer join, process-exit teardown of
  a blocked reader/writer, stdout/stderr `/dev/null`, broken output, and
  signal-vs-output precedence.

### Real binary and PTY tests

Use the normal `dsh` binary with a loopback HTTP/SSE DeepSeek server and a
conspicuous fake API key. No hidden release flag or in-process fake-provider
back door is added.

Default PTY tests cover:

- startup banner, prompt, `/help`, `/exit`, `/quit`, and Ctrl+D;
- help/version/argv errors use workspace, credential, and loopback sentinels to
  prove that none of those resources is touched; the fact that Tokio and its
  signal streams are not constructed is established separately by the audited
  startup order;
- one prompt whose assistant text arrives in two delayed chunks and is printed
  once in order;
- two turns in one process; the second loopback request contains the first turn's
  committed history;
- read-only tool request/result status;
- `apply_patch` exact diff then Allow once changes a temporary file only after
  the answer; Reject and Ctrl+C leave it unchanged;
- foreground Shell approval and result lifecycle;
- active-turn Ctrl+C preserves displayed partial text, commits stopped/aborted,
  returns to a fresh prompt, and a later prompt succeeds;
- TERM/HUP/QUIT/EOF cleanup and their stable exit codes;
- Ctrl+Z during a TERM-trapping Shell cancels and settles the process group
  before self-suspension, then redraws after `fg`/continue;
- TSTP cleanup interrupted by TERM/HUP/QUIT follows the termination exit rather
  than stopping; `bg` resumes only long enough to self-stop without touching the
  tty; script TSTP exits 148 after foreground resume;
- stale complete, partial, and continuous pretyped input before approval never
  authorizes; only a post-fence selection followed by Enter can allow;
- exact/+1 canonical line behavior, nonempty Ctrl+D, and huge-paste
  non-submission plus Ctrl+C recovery on macOS and Ubuntu;
- the complete `tcgetattr`-derived terminal-state snapshot is unchanged after
  normal exit, cancellation, approval, and suspend/resume; startup refusal
  occurs before any termios mutation, required `ECHOCTL`/`OPOST`/`ONLCR`
  disablement is rejected before the first prompt, and Phase 7 never calls
  `tcsetattr`;
- slow/non-reading terminal output cancels safely; approval EOF and tty EIO/HUP
  settle without a side effect; real SIGQUIT cleans before exit 131;
- a trickle-writing PTY with the maximum committed backlog proves ordinary final
  drain has one total deadline, while INT emits at most one summary and
  TSTP/TERM emit none before their stop/exit transition;
- ANSI/OSC/bidi from projected model/tool/preview/path text is visibly escaped,
  while hostile argv and opaque tool arguments are safely omitted; captured
  application frames contain none of the injected control sequences;
  PTY assertions allow the line discipline's own `OPOST/ONLCR` CRLF and do not
  mislabel trusted keyboard echo as `dsh` output;
- `NO_COLOR`, `TERM=dumb`, script pipe input, script `--prompt`, exact upstream
  blank/final stdout, stderr separation, missing key, Provider error,
  non-completed turn, Shell Deny with no process, and every advertised option;
- the conspicuous fake key is absent from PTY/stdout/stderr and every retained
  Session-derived projection; the PTY reader checks the complete byte stream,
  including a key split across reads and bytes later evicted from rolling
  diagnostic tails.
- a real 4,096-event/16 MiB admission exhaustion reports once and exits 1 rather
  than offering an unusable next prompt.

`tests/interactive_cli.rs` uses `pty-process`'s blocking API; bounded transcript,
loopback server, and child guard helpers live under `tests/support/` so Cargo
does not treat them as separate test binaries. Most cases spawn `dsh` directly
as the PTY session/foreground-group leader. That topology is sufficient for
input, output, cancellation, and simple stop/resume, but it is **not** evidence
for a real shell's `bg`/`fg` behavior. The dedicated job-control cases instead
spawn an interactive `/bin/bash --noprofile --norc` in the PTY. The shell starts
a tiny child wrapper that self-stops before `exec dsh`, records the exact job,
then uses its normal `fg`, `bg`, and `fg` transitions. A separate in-session,
bounded watchdog knows that job's process group and kills it if the test driver
disappears; ordinary cleanup cancels and reaps the watchdog.

PTY test ownership must be exact: each direct child has a bounded deadline, the
test keeps its process handle, closes the PTY, terminates only a still-owned
process group on failure, waits/reaps the child, and joins every reader/server
thread. In the shell-wrapper topology cleanup first uses the owning shell's job
table and trap, then the recorded still-owned job group; it never assumes the
outer shell's process group contains the foreground `dsh` job. Linux PTY-master
`EIO` is EOF. Cleanup never signals a remembered bare PID after ownership is
lost. A possibly stopped owned group receives CONT before bounded TERM then
KILL, so teardown cannot hang on a suspended test. Any approved Shell command
used in a job-control test also has an independent same-group self-expiry
watchdog so a test-harness crash cannot leave native code running.

The fake servers bind only `127.0.0.1:0`, accept a scripted bounded number of
connections (tool turns need more than one), and put deadlines on
accept/read/write. The shared interactive server matches the production request
ceiling at 8 MiB so the retained-session boundary can be exercised; the smaller
script-smoke server caps one request at 2 MiB. Chunk visibility and stalls use
bounded channel gates rather than timing sleeps. Early client disconnect during
cancellation is an expected recorded outcome, never a server-thread panic. The
ordinary PTY reader retains at most 1 MiB and treats overflow as failure; the
resource-ceiling cases use an explicit rolling mode that continues draining but
keeps only the latest 1 MiB. Both modes handle split markers and always join the
owned reader thread.

### Repository and platform gates

- `cargo +1.85.0 fmt --all -- --check`;
- `cargo +1.85.0 check --all-targets --locked`;
- `cargo +1.85.0 test --all-targets --locked`;
- `cargo +1.85.0 clippy --all-targets --locked -- -D warnings`;
- rustdoc with warnings denied, release build, help/version/script smoke, release
  symbol scan for test-only seams, whitespace and diff checks;
- default macOS local PTY suite and default Ubuntu 24.04 CI PTY suite.

Windows, another Unix, an external Turtle UI, public DeepSeek networking, real
credentials, and a sandbox are not claimed by these gates.

## Phase boundary

Phase 7 is complete only when the real binary, terminal approval, committed live
projection, cancel-then-continue behavior, script mode, PTY matrix, oracle
comparison, documentation, and macOS/Ubuntu gates all pass. README must continue
to describe only the behavior proven through that exact executable path.

Phase 8 will own durable JSONL storage, resume, interrupted-tail repair,
compaction production, history discovery, and long-session lifecycle. Phase 7
must not create an ad-hoc transcript that Phase 8 later has to reinterpret.

## Phase 9 inline TUI refinement

Phase 9 keeps the committed-event ownership and bounded writer above, but
replaces the developer-oriented plain presentation with a scrollback-first
inline TUI. It deliberately does not enter an alternate screen: prompts,
assistant text, tool activity, diffs, and errors remain ordinary terminal
history that a user can scroll, select, and copy.

The styled interactive path uses only product-owned ANSI SGR and bounded cursor
movement. Model text, tool arguments, paths, diffs, errors, and every other
untrusted field still pass through `VisibleRenderer`; they can never supply an
escape sequence. `--no-color`, `TERM=dumb`, and non-TTY/script output contain no
product ANSI and retain complete readable labels. This refinement uses the
existing `rustix` terminal capability rather than adding a full-screen TUI
framework.

### Visual hierarchy

- `dsh-rs` owns one compact startup line and a visually distinct input marker;
- reasoning is subdued, final assistant text is primary, and repeated streamed
  chunks keep one stable role rather than printing a new banner per fragment;
- tool request, result, retry, cancellation, and error states have distinct
  semantic tones plus text labels, so color is never the only signal;
- approval keeps the complete sanitized reason and preview above the selector.
  File changes therefore retain the canonical diff, while `bash` retains the
  exact command, workdir, timeout, and disclosed environment policy produced by
  the tool layer;
- the layout avoids terminal-width-dependent boxes, so narrow terminals wrap
  content without corrupting cursor accounting.

### Approval selector and terminal ownership

Ordinary prompts remain canonical line input. Only after the committed
`approval/asked` fact, broker envelope, preview, input flush, and quiet arming
fence have joined does the terminal enter a small **cbreak** selector mode:

1. capture the exact validated canonical `Termios` value;
2. disable `ICANON` and input echo, retain `ISIG`, `ICRNL`, output processing,
   and the canonical signal characters, and set `VMIN=1`/`VTIME=0`;
3. render three vertical choices with **Reject selected by default**;
4. Up/Left/`k`/`h` select the previous item; Down/Right/`j`/`l`/Tab select the
   next item; `y`, `n`, and `c` move to Allow once, Reject, and Cancel;
5. Enter confirms the visible selection. Escape selects Cancel after the short
   escape-sequence disambiguation deadline;
6. restore the exact captured canonical attributes **before** delivering the
   decision, suspending, exiting, or publishing a terminal/output failure.

An arrow escape sequence may be split across kernel reads. The selector owns a
small bounded decoder and a short pending-Escape deadline: a complete CSI arrow
moves selection, while an isolated Escape becomes Cancel. Unknown sequences and
oversized input fail closed and redraw the safe default. A stale
Enter can only confirm Reject, never Allow.

The mode owner is explicit rather than a detached task. Normal completion calls
fallible restore and verifies the original terminal facts; a scoped Drop guard
performs a best-effort restore only as a panic/unwind backstop. Ctrl+C and
Ctrl+Z remain kernel signals because `ISIG` stays enabled. The dispatcher first
restores canonical mode, then follows the existing cancel/cleanup/suspend
ordering. HUP, QUIT, TERM, EOF, output failure, observer failure, and a dropped
approval owner follow the same restore-before-exit rule.

### Phase 9 evidence and README screenshots

Selector unit tests add fragmented arrow input. Default PTY tests add arrow-key
selection, default-Enter Reject, explicit Allow, Escape/Cancel, Ctrl+C,
selector-active Ctrl+Z and termination, EOF, stale paste, unknown sequence,
output failure, zero-width fallback, and a harness-wide exact before/after
`Termios` assertion. Styled tests also prove that only
product-owned bytes contain ANSI and that `--no-color` contains none. The
output-failure fixture enters before cbreak; restore on an active output failure
is additionally reviewed through the owned state machine rather than presented
as a separate PTY observation.

README images are captured from the release-built real `dsh` binary against a
bounded loopback DeepSeek SSE fixture in a temporary workspace. The fixture
drives two deterministic scenes: streamed code exploration/tool status and an
`apply_patch` diff with the keyboard selector. The capture command, terminal
size, fake-key policy, and expected transcript are recorded in the Phase 9
validation document. Generated art is never presented as a product screenshot.
