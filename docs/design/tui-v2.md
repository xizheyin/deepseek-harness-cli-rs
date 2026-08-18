# TUI v2 design

## Status and decision

Phase 11 is a user-approved post-v0.1 product-quality extension. The Phase 9
linear renderer remains the tested escape hatch. Production-reachable slices
now add the owned composer, inline dock, one final truth-safe card per settled
tool, and one joined turn receipt. Markdown/diff, Inspect/Review, themes,
Session picker, installed screenshots, and the real-emulator matrix remain
incomplete.
Phase 11 therefore stays `in-progress`. It keeps the accepted Agent, Session,
approval, cancellation, and process semantics and replaces only their
interactive presentation and input ownership.

The primary design is a **hybrid inline TUI**:

```text
native terminal scrollback: committed conversation, completed tool summaries,
                            diffs, errors, and work receipts

small dynamic dock:         composer, current activity, queued prompts,
                            approval choice, and contextual key hints
```

The default Focus path does not use the alternate screen. Native selection,
search, copy, and ordinary terminal history remain available on the verified
primary-screen profiles. Auto mode conservatively avoids tmux, GNU Screen, and
Zellij; users may explicitly request enhanced mode there, but no cross-emulator
scrollback guarantee is claimed yet.
Inspect, Review, and the startup Session picker may enter a temporary
alternate-screen workspace only after an explicit user command; leaving it
restores the inline transcript and draft. A bounded plain renderer remains
authoritative for `--tui linear`, `--no-color`, `NO_COLOR`, `TERM=dumb`,
non-TTY output, and screen-reader use. `--tui auto` selects enhanced only for a
colored `xterm*` profile with no known multiplexer and an initial size of at
least 44 columns by 12 rows; every other profile starts in linear mode.

## Evidence and comparison boundary

The DeepSeek semantic baseline remains
`deepseek-ai/deepseek-harness@47f943859bef60e4160492346772ded9b24f765a`.
The pinned tree has no built-in human TUI. Relevant fixed paths are the same
Phase 7 seams recorded in `docs/upstream.md`:

- ACP multi-turn, cancellation, approval, and owner cleanup;
- Agent chunk/final/tool event ownership;
- approval asked/decided ordering and cancellation precedence;
- Web conversation partial/final projection, approval composer takeover, and
  running-input queue/steer behavior;
- applied-diff presentation models.

Claude Code's dated public
[interactive-mode](https://code.claude.com/docs/en/interactive-mode),
[fullscreen](https://code.claude.com/docs/en/fullscreen),
[accessibility](https://code.claude.com/docs/en/accessibility), and
[permissions](https://code.claude.com/docs/en/permissions) documents are UX
research only. Rust does not copy its brand, layout, strings, feature set, or
wire behavior. The comparison establishes the modern minimum—editable
multiline input, quiet tool summaries, inspectable transcript, responsive
approval, long-session stability, and accessible fallback—then fixes a
Rust-specific design with deterministic tests.

## Goals

1. Make the user's request and the authoritative assistant result visually
   primary; routine internal activity is secondary and collapsible.
2. Render one user-facing lifecycle for each tool call instead of separate
   requested/arguments/approval/result log lines.
3. Provide a bounded Unicode multiline composer with editing, history,
   bracketed paste, preserved drafts, and explicit next-turn queueing.
4. Show exactly what is running, why the user is waiting, what changed, and how
   to intervene without inventing progress or success facts.
5. Present Markdown, code, diffs, errors, and approvals with a consistent,
   responsive semantic hierarchy.
6. Keep all model/tool/path/error text unable to inject ANSI, OSC, cursor
   movement, bidi controls, or terminal clipboard operations.
7. Preserve the existing tested `Ctrl+C`, `Ctrl+Z`, HUP/QUIT/TERM, EOF, output
   deadline, approval fence, and owned process cleanup behavior.
8. Keep memory, pending input, redraw rate, output, and every dynamic region
   explicitly bounded.

## Non-goals

- no Web or desktop GUI;
- no visual or feature-for-feature Claude Code clone;
- no alternate-screen requirement, mouse requirement, or hidden terminal query
  that races ordinary input; explicit Inspect/Review acceleration is optional;
- no new approval bypass, sandbox claim, Provider, Hook, MCP, background agent,
  or arbitrary status-line command;
- no persisted prompt-history file or secret-bearing UI cache;
- no claim that model-generated explanations are security decisions;
- no Session schema rewrite solely for presentation.

## Product principles

### Attention follows consequence

Routine reads and successful searches are quiet. Assistant results are normal
high-contrast text. A saturated accent appears only for one current focus:
running work, approval, or an error. Green confirms a completed action once;
amber means a decision is required; red means a real failure. Color is always
paired with a word or symbol.

### One fact, one component

A `tool/call`, optional approval pair, and `tool/result` project into one
`ToolActivity`. The component changes state in the dock and commits one final
summary to scrollback. Internal call IDs remain available in Inspect data but
are hidden from the default view.

### Progressive disclosure

- **Focus**: user prompts, assistant answers, compact tool groups, decisions,
  errors, compaction notices, and final work receipt.
- **Inspect**: reasoning, complete tool parameters/results, retry facts,
  timings, and correlation IDs.
- **Review**: changed-file summaries, canonical diffs, executed commands,
  failures, and the turn receipt.

No fact is silently deleted. The view changes only the default presentation.

### Truth before animation

The TUI consumes committed Session facts. A local activity may say `Waiting`,
`Preparing`, or `Cleaning up`; it must not say `Running`, `Changed`, `Passed`,
or `Done` until the owning production path proves that fact. A spinner is
delayed for 300 ms so short operations do not flash. Elapsed time appears after
one second. At five seconds, the dock adds the concrete wait/cancel hint that
the host actually knows.

## Visual system

### Semantic roles

```text
text.primary       user and assistant content
text.muted         timing, counts, key hints, secondary metadata
accent             current focus and input cursor
success            completed effect
warning            approval, limit, retry, compaction pressure
danger             error and unresolved outcome
border              composer/dock divider only
diff.add           inserted lines and positive diffstat
diff.remove        deleted lines and negative diffstat
selection          focused menu item, never the only focus signal
```

The terminal background is inherited. Conversation messages have no box.
Borders are reserved for the composer, approval choice, serious errors, and
temporary menus. Bold is used for one heading or selected action, not every
status line. Icons have one stable meaning:

```text
● running     ✓ success     × failure     ? approval
○ denied/skipped            ◇ system/compaction
```

Built-in palettes are Adaptive, Midnight, Paper, Color-blind, High Contrast,
and Mono.
Adaptive uses the terminal's ordinary ANSI palette so it remains usable on
unknown light/dark backgrounds. Themes are semantic token maps, never embedded
escape strings in business events. Reduced Motion disables spinner animation
and uses text changes only.

## Responsive layouts

### 112 columns × 34 rows

```text
 dsh · ds-harness-rs · deepseek-v4 · Manual · context 42%

 YOU
 Fix the authentication timeout and run the related tests.

 DSH
 I will inspect the request path and its timeout tests first.

   ✓ Read      src/auth/session.rs                         12 ms
   ✓ Search    "request_timeout"                    8 matches
   ✓ Updated   src/auth/session.rs                        +12 −3
   ● Testing   cargo test auth                              8.2s
               Running the focused suite             Ctrl+C stop

──────────────────────────────────────────────────────────────────────────────
❯ Continue typing; Enter queues the next turn…
  Enter queue · Ctrl+J newline · @ files · / commands · ? help
```

### 80 columns × 24 rows

```text
 dsh · ds-harness-rs · deepseek-v4 · context 42%

 YOU  Fix the authentication timeout and run its tests.

 DSH  I will inspect the request path first.

   ✓ Read     src/auth/session.rs
   ✓ Search   "request_timeout" · 8 matches
   ● Testing  cargo test auth · 8.2s

────────────────────────────────────────────────────────────
❯ Draft the next message…
  Enter queue · Ctrl+J newline · Ctrl+C stop
```

### 44 columns × 20 rows

```text
 dsh · deepseek-v4 · 42%

 YOU
 Fix the authentication timeout.

 DSH
 I will inspect the request path first.

 ✓ Read src/auth/session.rs
 ● cargo test auth · 8.2s

────────────────────────────────────────────
❯ Next message…
  Enter queue · Ctrl+C stop
```

An initial terminal below 44 columns or 12 rows starts in the linear plain
presentation. Once enhanced mode owns the terminal, a resize down to 12×5 uses
a four-row compact rescue dock so drafts and approvals remain visible. A resize
below 12×5 clears uncertain geometry, restores the terminal, and fails closed;
switching a live cbreak session into canonical linear input is not guessed.

## User-facing state

The UI does not copy the Agent state machine. It owns how committed facts and
local input are presented:

```rust
enum Interaction {
    Idle,
    Running { turn: TurnId },
    Approving { turn: TurnId, request: ApprovalRequestId },
    Cancelling { turn: TurnId },
    Suspended,
    Exiting,
}

enum ViewMode {
    Focus,
    Inspect,
    Review,
}
```

`UiState` also owns a `Composer`, bounded `PromptQueue`, current
`TurnPresentation`, dock geometry, view mode, theme, scrollback commit marker,
and approval focus. A pure reducer consumes:

```text
keyboard / paste / resize / timer / signal / committed Session fact
                                ↓
                             UiEvent
                                ↓
                         UiState + UiEffect
                                ↓
submit/queue/cancel/approve/write/suspend/exit
```

No lock is held across an async effect. Rendering cannot call Provider, tools,
approval policy, Session mutation, filesystem operations, or subprocesses.

One `TerminalSession` owns the terminal descriptors, the exact original
termios, the derived application termios, bracketed-paste state, and optional
alternate-screen state. One `InteractiveDriver` event loop is the only TTY
reader and writer and the only owner that changes input focus. Decoder,
composer, projector, layout, and renderer are pure or synchronously bounded
components beneath that owner; no second input task may race them.

## Presentation vocabulary

```rust
enum TimelineItem {
    User(UserMessageView),
    Assistant(AssistantView),
    ToolGroup(ToolGroupView),
    Decision(DecisionView),
    Error(ErrorView),
    Notice(NoticeView),
    Compaction(CompactionView),
    Receipt(WorkReceiptView),
}

enum ToolActivityState {
    Preparing,
    AwaitingApproval,
    Executing,
    Settling,
    Succeeded,
    Failed,
    Denied,
    Cancelled,
    OutcomeUnknown,
}
```

`tool/call` proves only committed intent, so it enters `Preparing`, never
`Executing`. `Executing` requires a bounded supplement from the owning runtime
after it has dispatch evidence. Approval Allow proves permission, not dispatch.
If no live supplement exists, the UI stays at `Preparing`/`Settling` until the
committed result provides the final state. A turn closing with an unresolved
call becomes `OutcomeUnknown`, never implicit success.

The projector correlates tool state by `(turn, step, call_id)` and approval by
request ID. It produces bounded, tool-specific summaries without changing the
persisted event schema:

- `list`: relative path and returned/truncated entry count when provable;
- `glob`: pattern and returned/truncated path count;
- `grep`: pattern and match/file count when provable;
- `read`: relative path and visible line range/count;
- `apply_patch`: relative file, create/update class, `+N −N`, and canonical
  diff in Review;
- `bash`: visibly escaped command, relative workdir, elapsed time, and exact
  exit/timeout/signal state;
- plugin: configured public plugin/tool IDs and normalized result state, never
  executable path, argv, stderr, or internal protocol ID.

If a structured count is unavailable, the UI says only what is known. It never
parses arbitrary command text to invent a test count or success claim.

The live presentation contract has two fact sources. `CommittedUiEvent`
projects durable ordering, user/assistant messages, retry, usage, request
context, compaction, approval, and tool correlation IDs from a committed
`SessionEvent`. A bounded `ToolPresentation` or `ApprovalPresentation` may be
attached only by the owning Agent/tool path after the corresponding Session
fact commits. It contains closed, tool-specific display facts such as a
workspace-relative path, canonical diffstat, exit classification, or plugin
public ID. It never carries an executable path, plugin argv/stderr, secret,
unbounded tool body, or an uncommitted success claim. The projector may show a
generic lifecycle when this presentation is unavailable; it must not parse an
arbitrary result string to synthesize one.

The initial event projection must stop reducing these user-facing facts to a
type name: user message content; token usage; provider/model/context window;
safe compaction phase/count fields; retry delay/failure; and assistant source,
usage, provider, and model. Compaction summary/raw-output bodies remain hidden.
Historical receipts are rebuilt only from facts actually retained by the
Session; absent timing or tool-presentation facts remain absent.

## Composer and queue

The enhanced terminal uses a long-lived, owned cbreak mode: `ICANON`, echo,
`ECHONL`, `ICRNL`, `IXON`, and `IXOFF` are disabled; `ISIG`, `IEXTEN`, output
post-processing, and the validated interrupt/suspend/quit characters remain
enabled; and `VMIN=1`/`VTIME=0`. Clearing `ICRNL` is what keeps carriage return
(`Enter`, submit) distinct from line feed (`Ctrl+J`, newline). This is not
Crossterm raw mode. TTY identity/foreground validation is separate from
canonical/application-mode validation. The exact original termios is restored
before suspension, exit, terminal failure, or returning an error.

Approval no longer owns a temporary termios guard. It changes only input focus
inside the long-lived application mode. The existing arming barrier remains:
the trusted preview must finish writing, input must stay quiet for 100 ms, the
kernel input queue is flushed once, and the decoder epoch is reset before an
approval key can count.

The composer supports:

- UTF-8 assembly across reads and grapheme-safe cursor/delete operations;
- Left/Right, Home/End, Ctrl+A/E, Backspace/Delete, Ctrl+W/U/K, `Ctrl+_`
  undo, and bounded yank. `Ctrl+Z` remains the kernel suspend character and is
  never reused for undo;
- Up/Down bounded in-session history when the cursor cannot move vertically;
- Ctrl+R bounded reverse history search;
- Ctrl+J or Shift+Enter where the terminal reports it for a newline; Enter for
  submit/queue;
- bracketed paste whose newlines never submit and whose control bytes that
  actually reach the application are rendered as visible text. `VINTR`,
  `VSUSP`, and `VQUIT` remain kernel signal characters even inside paste
  because preserving `ISIG` is the stronger cleanup and safety contract;
- slash-command and bounded file-suggestion modes with explicit focus;
- a draft that survives running work, approval, resize, cancellation, and
  temporary menus.

While a turn runs, Enter creates a visible **next-turn queue item**. It does
not steer the current model request and does not enter Session until it becomes
the next admitted turn. Up from the first composer row retrieves the newest
queued item for editing. `Esc` closes a temporary menu; during approval it and
`Ctrl+C` stop the active turn and retain queued prompts. Queue capacity is eight prompts,
64 KiB per prompt, and 256 KiB aggregate. Overflow is a visible local error and
does not drop the draft.

Queued prompts are admitted FIFO only after the current turn, required cleanup,
and Session settlement complete. A fatal error, explicit exit, terminal loss,
or shutdown never sends them automatically; they remain visible until the
process can safely offer editing again, otherwise they disappear with process
memory and were never recorded as user messages.

Prompt history currently records committed human prompts observed by this
process and keeps them only in bounded memory. Rebuilding it from a resumed
Session snapshot is still part of the later Session-picker/history slice.
Phase 11 writes no separate history file.

Attaching the UI returns a `UiFeed` with a bounded initial snapshot plus the
live receiver. A new Session has an empty snapshot. A resumed Session rebuilds
only the current model-visible surface, model/context facts, and safe
compaction markers; it marks `history_truncated` when the append-only journal
cannot be reconstructed through the existing public Session view. Live events
start strictly after the snapshot sequence and must not duplicate it. Phase 11
does not claim complete historical timings, tool receipts, or hidden compacted
messages without a separately bounded journal scan.

## Scrollback and dock rendering

Completed transcript blocks are appended exactly once through structured
`PresentedChunk` runs. `InlineScreen` is the sole cursor owner: the primary
screen always uses full-screen scrolling (`CSI r`, never a partial DECSTBM), a
fixed-height dock occupies the bottom rows, and the composer uses a software
cursor. Initial attachment emits `dock_rows + 1` full-screen line feeds so the
pre-existing bottom of the terminal moves into native history instead of being
overwritten. Input-only redraws clear and replace only the owned dock and never
replay transcript text.

Each coordinate batch is staged against one ledger generation and committed
only after the whole write succeeds. A zero-byte resize can be restaged. A
partially written coordinate batch poisons the ledger; the driver then clears
the uncertain visible viewport with ED2, keeps bracketed paste enabled during
in-process recovery, establishes a fresh transcript boundary, and redraws the
dock. Suspend and exit use a separate reset that disables paste and shows the
real cursor before restoring termios. Clearing the viewport is an intentional
failure-path tradeoff: it prevents a partial private draft or approval from
being scrolled into history, while committed facts remain in Session.

`SIGWINCH` or a detected winsize change recomputes Unicode display widths and
starts a fresh transcript boundary; it does not claim that an unfinished
physical line can be portably reflowed after the emulator has already resized.
The small deterministic terminal model covers full-screen-only and
top-anchored history policies, while real xterm/iTerm/Terminal/VS Code shrink,
reflow, and copy behavior remains a release-checkpoint matrix. Resize cannot
decide an approval or submit a prompt.

Ordinary stream refresh is capped at 30 frames/second, spinner animation at
8 frames/second, and idle mode has no periodic wake. Input, signal, approval,
terminal failure, and final committed facts take priority over animation ticks.
Each output batch keeps the existing absolute five-second write deadline.

## Markdown, code, and diff

The first implementation supports a bounded, streaming-safe subset:

- paragraphs and soft wrapping;
- headings levels 1–3;
- bullet and numbered lists;
- block quotes;
- inline code;
- fenced code with language label;
- simple tables that linearize below 80 columns;
- canonical unified diff with file headers, hunks, additions, deletions, and
  context.

Untrusted content is parsed into semantic spans only after terminal-control
sanitization. Raw ANSI is never interpreted. Unterminated streaming syntax is
displayed as ordinary text until the final authoritative message resolves it.
Reasoning is a collapsed `Thinking · elapsed` item in Focus and expanded only
in Inspect. Code and diff retain copyable plain text in scrollback.

## Approval

The committed `approval/asked` fact and the matching trusted preview must join
before the approval UI exists. The preview is appended once to scrollback; the
dock then presents the decision:

```text
? Permission required · Modify 1 file
  src/message.txt · +1 −1
  Writes one workspace file. No shell command.

    Allow once      ● Reject      Stop turn
    ←/→ choose · Enter confirm · Esc stop
```

Shell and plugin approvals visibly state that native execution is not a
sandbox. Risk statements come from closed local action contracts. The model's
reason is displayed separately and never changes the decision policy. Reject
is selected by default. Only a directional navigation key received in the
current armed modal epoch may focus Allow, and a later Enter confirms it.
Printable `y`, stale Enter, paste, unknown CSI, resize, output failure, or a
lost approval owner cannot select Allow. `Esc` and `Ctrl+C` choose Stop turn;
closing the modal without a decision would strand the Agent, so it is not a
state. The remainder of the read batch containing a decision is discarded
rather than becoming composer input. Plain mode uses an explicit
`Approve this action? [y/N/c]:` record with the same default-deny semantics.

## Work receipt, context, and sessions

At turn end, the enhanced renderer appends one compact receipt from projector
facts only after the committed `turn/end` and returned `TurnOutcome` agree on
turn, sequence, and reason. The current Focus slice renders exact
step/tool-request/retry/output-token counters, strict patch effects, strict
foreground-Shell starts, and issue counts:

```text
Turn complete
  5 steps | 4 tool requests | 1 retry | 842 reported output tokens
  2 files changed (+12 -3) | 1 command run | 1 issue
```

It cannot claim test counts or pass status unless a structured, trusted source
provides them. The current slice also does not claim an execution duration.
Review will later expand changed files, canonical diffs, commands, errors,
denials, cancellations, and unknown outcomes.

The header displays the configured model, workspace basename, approval mode,
and bounded context percentage when known. Compaction emits one quiet marker
and an Inspect expansion; it does not expose internal event noise.

`dsh --resume` without an ID may open a bounded picker after the Session root
and workspace policy are validated. The picker shows a safe display name or
last user-message summary, age, and workspace basename; full UUID and path are
details. No history is opened, mutated, or resumed merely by moving selection.

## Commands and suggestions

`/` opens a finite command palette whose entries are product-owned. `@` asks a
bounded read-only suggestion provider supplied by CLI assembly; the TUI itself
does not traverse the filesystem. Suggestions are capped, cancellable, and
visibly relative to the workspace. Selecting a suggestion inserts text only;
it never reads the file into the model request by itself.

Ctrl+O opens Inspect, while `/review` opens Review. Focus/Inspect/Review, theme,
reduced motion, help, status, sessions, exit, and quit are local commands.
Commands that would change Agent or Session semantics must follow the ordinary
audited boundary rather than being hidden UI actions.

## Plain and accessible rendering

Plain output contains zero ESC bytes and complete textual labels. It uses
canonical input, numbered or lettered decisions, and append-only status lines;
there is no cursor animation or dock rewrite. Every enhanced interaction has a
plain semantic path, though mouse and cursor-rich editing are accelerators, not
requirements. Mono and High Contrast never use color as the only distinction.
Reduced Motion removes spinner frames. An optional product-owned bell or title
notice may fire only when approval is ready or a long turn completes; model
text never controls it and the default is off.

## Security and terminal ownership

- `CommittedUiEvent` remains the live fact boundary.
- Model, tool, path, diff, error, queue, and Session text pass through the
  existing visible-control sanitizer before layout.
- Untrusted text becomes sanitized `PresentedChunk` items with closed
  `TextStyle` roles. Only `InlineScreen` serializes fixed cursor/clear/SGR
  commands; no untrusted value becomes a cursor count, SGR parameter, OSC
  payload, or terminal query.
- Entering enhanced mode flushes stale input and enables bracketed paste only
  after exact termios capture. Leaving or suspending disables paste, leaves any
  optional alternate screen, and restores termios in that order, including
  panic/unwind best effort.
- Ctrl+C/Z and terminating signals preserve the existing tools-first cleanup
  and Session shutdown order.
- A partially written coordinate frame poisons the screen ledger and is
  recovered with the bounded ED2 path above; it does not roll back, replay, or
  fabricate a Session fact.
- Output failure, resize, and unknown input fail closed. They cannot authorize
  or start a side effect.

## Resource limits

| Resource | Limit |
| --- | ---: |
| composer prompt | 64 KiB UTF-8 |
| undo history | 128 inverse edits and 1 MiB deleted payload |
| yank buffer | 64 KiB UTF-8 |
| queue items | 8 |
| one queued prompt | 64 KiB UTF-8 |
| queued prompt aggregate | 256 KiB |
| in-memory prompt history | 128 entries and 1 MiB |
| bracketed paste | 64 KiB UTF-8 |
| CSI sequence | 32 bytes |
| projected tool activities / approval links | 256 each |
| projected tool summary / Dock activity source | 4 KiB UTF-8 each |
| final tool-card headline / detail | 256 UTF-8 bytes each |
| receipt headline / counters / effects | 4 KiB UTF-8 each |
| sanitized owned text / presented text | 1 MiB |
| presented items | 128 Ki items |
| screen transaction | 2 MiB |
| retained split grapheme | 1 KiB |
| visible suggestion rows | 12 |
| file suggestion candidates | 256 |
| dynamic dock | 24 rows |
| composer visible height | 8 rows |
| ordinary refresh | 30 FPS |
| animation refresh | 8 FPS |
| enhanced minimum | 44 columns × 12 rows |
| already-enhanced compact rescue | 12 columns × 5 rows |
| terminal write batch | existing 8 KiB chunks and 5-second deadline |
| poisoned visual reset | 250 ms |

These UI text limits apply to source bytes before visible-control sanitizer
expansion. Implemented input, queue, decoder, and screen limits have exact and
one-over tests. The new card, receipt, and Dock text limits are bounded, while
their complete exact/one-over evidence remains a release-checkpoint gate.
Existing Session, Provider, tool, approval-preview, and terminal-output limits
remain in force.

## Failure and cancellation matrix

| Situation | Required UI behavior |
| --- | --- |
| invalid UTF-8 key bytes | visible local input error; draft and Session unchanged |
| incomplete/unknown CSI | cancel menu/approval or insert visible text as specified; never Allow |
| oversized prompt/paste/queue | reject locally without losing the previous draft |
| resize during stream | reflow dock; no repeated transcript or cursor loss |
| resize during approval | preserve Reject/selection; no decision |
| Ctrl+C while running | keep draft/queue, show stopping/cleanup, then next prompt |
| Ctrl+C while idle | clear draft first; second explicit action may exit per documented policy |
| Ctrl+Z | restore terminal, finish required cleanup, suspend, revalidate/redraw on resume |
| HUP/QUIT/TERM | restore terminal and finish owned cleanup before stable exit |
| output deadline/failure | restore termios; preserve primary error; no side effect after failed approval display |
| Provider/tool error | one actionable error component plus raw code in Inspect |
| outcome unknown | prominent unresolved result; never render success or replay |
| Session/compaction error | explain what remains safe and whether continuation is possible |
| panic in trusted UI helper | catch at owned boundary where possible, restore terminal, never persist panic payload |

## Acceptance tests

1. Pure reducer tests cover every `Interaction`, view, tool, approval, queue,
   command, resize, cancellation, and terminal-failure transition.
2. Semantic golden tests render 112×34, 80×24, and 44×20 scenes in Adaptive,
   Paper, High Contrast, Mono, and plain modes, including Chinese, emoji,
   combining characters, long paths, hostile controls, Markdown, diff, errors,
   compaction, and queue overflow.
3. Real PTY tests cover Unicode editing, multiline, history, bracketed paste,
   Agent-working drafts/queues, command/file palettes, approval, resize storms,
   scrollback uniqueness, signals, output backpressure, and exact termios
   restoration on macOS and Ubuntu.
4. A deterministic 100-chunk/second stream keeps input responsive, loses no
   Session fact, and has p95 committed-event-to-frame latency below 50 ms on the
   release acceptance host.
5. Fifty thousand synthetic presentation facts keep dynamic rendering bounded
   by visible/dock state; the implementation does not keep a second unbounded
   transcript.
6. Resize 100 times while streaming and approving preserves draft, queue,
   selection, scrollback count, and side-effect count.
7. The installed journey performs read/search, approved patch, approved shell,
   cancel-and-continue, resume, compaction, plugin approval, Focus/Inspect/Review,
   queueing, and final receipt without a real API key.
8. README screenshots come from the installed candidate's real PTY bytes, not
   mockups; overview, approval, and review scenes are captured at declared
   terminal sizes.
9. `./scripts/verify.sh`, Phase 9/10 acceptance, the new Phase 11 acceptance,
   `git diff --check`, independent safety/UX review, and macOS/Ubuntu CI all
   pass with zero ignored tests.

## Implementation checkpoints

1. **Design checkpoint (green)**: this document, roadmap/compatibility/upstream
   status, state tables, wireframes, and the frozen red-test inventory.
2. **Semantic foundation (green)**: bounded committed UI facts, reducer,
   correlation, truth-safe metadata, and fail-open shadow observation.
3. **Composer + inline Dock (green)**: owned long-lived cbreak, decoder,
   Unicode editing, current-process history, paste fences, FIFO, full-screen
   scroll ledger, compact layouts, enhanced approval, exact restoration, and
   PTY failures. This is the first production enhanced path.
4. **Truthful timeline slice (green)**: at most one final card per projected
   lifecycle, emitted by the first non-replacement result or a turn-end unknown
   fallback; strict patch/Shell/plugin facts; and an exact Session/TurnOutcome
   receipt join.
5. **Remaining product checkpoint**: Markdown/diff, Focus/Inspect/Review,
   commands/suggestions, themes, context/compaction, and session picker.
6. **Release checkpoint**: remove the replaced log renderer, installed-binary
   journeys, screenshots, documentation, full clean-target gates, independent
   review, non-force push, dual-platform CI, and a separate completion-status
   commit.

Each checkpoint must be coherent and green before it is pushed. Phase 11 stays
`in-progress` until the final candidate and status commit both pass the declared
platform matrix.
