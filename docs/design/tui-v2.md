# TUI v2 design

## Status and decision

Phase 11 is a user-approved post-v0.1 product-quality extension. The Phase 9
linear renderer remains the tested escape hatch. Production-reachable slices
now add the owned composer, inline dock, one final truth-safe card per settled
tool, one joined turn receipt, bounded assistant-only presentation, and a
generator-provenanced semantic preview for real `apply_patch` approvals. A
bounded current-turn Inspect and one-summary Review now use the same primary-
screen ledger. Tables, commands/suggestions, themes, Session picker, installed
screenshots, and the real-emulator matrix remain incomplete.
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
Inspect and Review use a temporary, read-only panel inside the
existing primary-screen Dock. The panel is owned by the same `InlineScreen`
ledger as the composer, so committed output continues into native scrollback
above it and no second transcript is retained or replayed. The panel is never
written into scrollback as a report merely because the user opens it. A later
Session picker may use an alternate screen only after a separate terminal
ownership checkpoint proves every partial-write, signal, suspend, resize, and
Drop path; Phase 11 does not rely on that acceleration. A bounded plain
renderer remains
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
  that races ordinary input; the first Inspect/Review path stays in the owned
  primary-screen Dock;
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
summary to scrollback. Internal call IDs remain correlation state, but the
first Inspect view reports only their retained/original byte availability.

### Progressive disclosure

- **Focus**: user prompts, assistant answers, compact tool groups, decisions,
  errors, compaction notices, and final work receipt.
- **Inspect**: retained reasoning, payload/identity availability, retry facts,
  and commit times. Turn, step, and committed sequence are its visible
  correlation keys. It says `omitted` when a bounded source is not retained;
  it does not call bounded data complete.
- **Review**: changed-file summaries, proven process outcomes, failures, and
  the joined turn receipt. Canonical diffs and full command records require a
  later closed presentation supplement and are not reconstructed from prose.

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
`TurnPresentation`, dock geometry, a transactional view mode, theme,
scrollback commit marker, and approval focus. `ViewMode::Inspect` and
`ViewMode::Review` hide the composer visually but never move or clear its
draft, undo state, history navigation, or queued prompts. A pure reducer
consumes:

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
termios, the derived application termios, and bracketed-paste state. The first
Inspect/Review implementation does not add alternate-screen state. One
`InteractiveDriver` event loop is the only TTY
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

The production path supports a bounded, streaming-safe subset only for
assistant text:

- paragraphs and terminal soft wrapping;
- headings made from one to three `#` characters followed by a space;
- `- `, `* `, and `+ ` bullets, plus one- to three-digit numbered markers
  followed by `. `;
- `> ` block quotes;
- paired single-backtick inline code;
- line-leading triple-backtick fences whose optional language label is at most
  32 ASCII bytes and contains only alphanumerics or `_+.-`;
- case-insensitive `diff` or `patch` fences with visual file-header, hunk,
  addition, deletion, and context styles.

The parser does not decode HTML entities or interpret raw ANSI. Tables,
emphasis, links, images, and HTML rendering remain unimplemented. Assistant
fenced `diff`/`patch` remains presentation-only and never proves a file effect.
Real `apply_patch` approval uses a separate provenance path described below;
it never promotes assistant text or a generic diff-looking prompt.

Untrusted content is converted to visible text before parsing, and the closed
presentation builder rejects terminal controls again. Only a matching
authoritative assistant final may recognize a closing fence at EOF without a
trailing line feed. A stream-key change, retry/correction boundary, `StepEnd`,
`TurnEnd`, or cancellation aborts pending syntax and flushes it as ordinary
assistant text; an abort never promotes incomplete output to code. Code and
diff retain copyable source text in native scrollback. Linear mode retains
reasoning in its established plain record; enhanced Focus suppresses it and the
bounded Inspect archive owns its current-turn presentation.

## Approval

The committed `approval/asked` fact and the matching trusted preview must join
before the approval UI exists. The preview is appended once to scrollback; the
dock then presents the decision:

```text
Approval required
Proposed update · not applied
  src/message.txt · +1 -1 · 1 hunk
  One workspace file · no shell command

--- a/src/message.txt
+++ b/src/message.txt
@@ -1 +1 @@
-old
+new

Approval required | proposed action above
  Allow once | apply exact preview
> Reject | make no change
  Stop turn | cancel work
Not sandboxed | Reject is the safe default
Arrow keys move | Enter confirms | Esc stops
```

The patch tool attaches closed provenance while it builds the canonical
single-file diff. This is a Rust type boundary, not a cryptographic signature.
One immutable preview string remains the source for the approval card and the
eventual tool-result `meta.diff`; a compact, bounded row-kind vector records
file headers, hunks, context, additions, removals, and no-newline markers.
Operation, workspace-relative path, hunk count, and `+N/-N` are produced at the
same boundary. This is process-local presentation provenance and is not added
to the Session schema.

`ApprovalPrompt::new` always creates an opaque preview. A tool name, model
reason, raw tool arguments, result prose, or text that merely resembles a diff
can never acquire patch styling. Only the patch preparation path can construct
the closed canonical-patch presentation. The row vector is aligned with every
physical preview line and is validated before the prompt can exist. This also
keeps hunk content such as `--- a/decoy` classified as a deletion rather than a
file header. The enhanced presenter first makes every variable character
terminal-visible, then applies the provenance-tagged row styles without
changing the copyable text. The linear renderer ignores the metadata and
preserves its complete zero-ESC record.

The card says **proposed** / **not applied** until a real tool result arrives.
It shows the closed operation, path, hunk and line counts, the fact that this is
one workspace file with no Shell command, and the complete diff. A malformed
or resource-invalid semantic preview fails before the selector can accept
input; it is never partially trusted or silently truncated. Generic Shell and
plugin previews remain opaque Warning text.

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
The current Review expands trusted changed-file and process/plugin outcome
summaries, errors, denials, cancellations, and unknown outcomes. Canonical
diffs and full command records still require a later closed supplement.

Focus does not claim a live context percentage from the latest Provider usage.
The first truthful status line uses the Session projection's own bounded
surface estimate, sampled by the CLI before and after a turn, and labels it as
an estimate. Inspect separately shows the latest reported input/output/cache
usage and the configured context window. A route change never combines a new
model with an older window or usage record. If the safe join is unavailable,
the percentage is omitted rather than guessed.

Compaction emits one quiet marker and an Inspect expansion; it does not expose
the summary or raw Provider output. A successful marker says that earlier
context was summarized and, when known, that the shadowed nodes were estimated
at a given token count. It never says that exactly that many tokens were
deleted or freed. A prune marker is not called complete until its matching
surface replacement commits.

### Bounded view archive and primary-screen detail panels

`LiveRenderer` remains the sole owner of `UiProjector` for the first detail
slice. Beside it, one `ViewArchive` retains only the current turn and one
frozen last-successfully-joined review. It observes complete
`CommittedUiEvent` values so sequence and commit time are not discarded, but
it does not reimplement tool outcome correlation. Inspect and Review builders
read this archive and the projector through immutable snapshots.

The archive is presentation state, not a second Session log:

- Inspect retains bounded reasoning text, retry facts, approval outcomes,
  compaction phases, and payload/identity availability
  (`retained/original bytes`, omitted parts). Raw tool arguments/results/meta
  and literal internal correlation IDs are not shown by default and never
  enter Debug output. Turn, step, and committed sequence are the visible
  correlation keys in this first slice.
- Review is frozen only after the committed `turn/end` and returned
  `TurnOutcome` agree on turn, sequence, and the receipt-relevant reason key.
  It contains trusted changed-file summaries, strict foreground process
  outcomes, plugin settlement facts, issues, and the same receipt used by
  Focus.
- The first Review slice is deliberately summary-only. It does not call Git,
  reread the workspace, parse model/tool prose, or claim a complete historical
  diff/command record. A canonical diff may appear later only through a closed
  post-commit presentation supplement from the owning patch path.
- A resumed observer begins at the live seam. Inspect therefore says that
  earlier details are unavailable, and Review does not synthesize historical
  counters or timings without a retained `TurnOutcome` join.

`ViewMode` has a desired state and a screen-committed state. A key may request
Inspect or Review, but the transition becomes visible only after the matching
`InlineScreen` transaction commits. Changing panel height uses the same-size
re-anchor path: clear every old owned bottom row, scroll only the additional
height needed when a panel grows, retain the existing transcript cursor when a
panel shrinks, then draw the new fixed-height panel. It does not reuse the more
conservative physical-resize path, which intentionally creates a completely
new boundary. Closing restores the unchanged Composer Dock.
Program-driven transitions therefore do not append panel snapshots to native
history or replay transcript facts.

The terminal emulator resizes before `SIGWINCH` reaches the process. A hostile
or unusual emulator may therefore move old primary-screen rows into its own
history during shrink before dsh can clear them. Phase 11 can prove that dsh
does not actively append or duplicate panel text; it does not claim universal
control over emulator-owned resize history.

While a detail panel is open, the event loop continues draining and reducing
the bounded observer. Assistant/tool output still commits once above the
panel, while the panel coalesces redraws. `Ctrl+C` keeps its existing turn
cancellation meaning. Paste, Enter, printable text, and unknown CSI cannot
submit, queue, or approve from a detail panel. `Ctrl+O`, `Esc`, or `q` returns
to Focus; arrows and PageUp/PageDown scroll. Exact local `/inspect`, `/review`,
and `/focus` commands never enter Session or the next-turn FIFO.

Approval has higher focus than a detail panel. Once a matching committed
approval question exists, the driver first commits a panel-to-Focus Dock
re-anchor. Only then may it append the trusted preview. After that preview
commits, the existing 100 ms quiet period, input flush, decoder epoch reset,
and default-Reject selector arm exactly as before. Approval never automatically
returns to the old detail view.

`dsh --resume` without an ID may later open a bounded picker after the Session
root and workspace policy are validated. The first picker may show only facts
already present in the bounded session listing: a safe workspace basename,
creation time, and shortened ID. A last-message summary or last-active time
would require a separately bounded read-only journal scan and is not inferred
from header metadata. No history is opened, mutated, or resumed merely by
moving selection.

## Commands and suggestions

`/` opens a finite command palette whose entries are product-owned. `@` asks a
bounded read-only suggestion provider supplied by CLI assembly; the TUI itself
does not traverse the filesystem. Suggestions are capped, cancellable, and
visibly relative to the workspace. Selecting a suggestion inserts text only;
it never reads the file into the model request by itself.

Ctrl+O opens Inspect, while `/review` opens Review. `/inspect` and `/focus`
provide explicit equivalents. In a running turn these exact commands change
the local panel immediately; they are never queued as model prompts. During an
approval, the approval focus wins and view-switch keys cannot authorize or
replace the question. Focus/Inspect/Review, theme,
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
| canonical patch approval path | 4 KiB UTF-8 |
| canonical patch approval preview | existing 64 KiB UTF-8 |
| canonical patch row provenance | one byte per physical preview row, at most 64 Ki entries |
| final tool-card headline / detail | 256 UTF-8 bytes each |
| receipt headline / counters / effects | 4 KiB UTF-8 each |
| Inspect committed-fact rows | 512 per turn |
| Inspect retained text aggregate | 512 KiB UTF-8 |
| retained reasoning for Inspect | 256 KiB UTF-8 per turn |
| Inspect reasoning blocks / omission-step entries | 128 each |
| frozen joined Review snapshots | 1 |
| Review activities | 256 |
| Review archived text aggregate | 144 KiB UTF-8 |
| detail-panel source lines | 4,096 |
| detail-panel physical rows | 4,096 after wrapping |
| detail-panel source text | 1 MiB UTF-8 |
| markup line-prefix candidate | 64 sanitized UTF-8 bytes |
| complete inline-code candidate, including delimiters | 4 KiB sanitized UTF-8 |
| fence language label | 32 ASCII bytes |
| complete retained fence candidate, including delimiters/newlines | 64 KiB sanitized UTF-8 |
| semantic non-plain style starts | 4,096 per assistant stream |
| markup presentation-frame soft item budget | 96 × 1,024 items, including 8,208-item conservative headroom |
| markup presentation-frame soft text budget | 768 KiB sanitized UTF-8 |
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

Input, queue, card, receipt, and Dock limits apply to source UTF-8 bytes unless
their row says otherwise. Markup limits explicitly apply after visible-control
sanitization. Inline/fence/style overflow degrades to ordinary copyable text;
frame item/text overflow emits one fixed
`[assistant display omitted: presentation limit exceeded]` marker and suppresses
the remainder of that assistant block's display. These are presentation-only
decisions: they neither modify Session facts nor cancel a valid Agent turn.
Implemented markup, input, queue, decoder, and screen limits have exact and
one-over tests. Complete exact/one-over evidence for every card, receipt, and
Dock field remains a release-checkpoint gate.
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
| resize during Inspect/Review | clamp the viewport; preserve draft/queue; never replay transcript |
| approval while Inspect/Review is open | commit Focus Dock first, then preview and the unchanged Reject-first fence |
| Inspect archive overflow | one `details omitted` fact; Session and current turn continue |
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

1. Pure reducer tests cover every `Interaction`, desired/committed view, tool, approval, queue,
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
5. **Bounded assistant markup (green)**: fragment-independent headings, lists,
   quotes, inline code, fenced code and fenced diff; authoritative-finish versus
   abort semantics; visible-control safety; graceful display omission;
   44/80/112-column terminal models; and enhanced/linear PTY evidence.
6. **Canonical patch approval (green)**: generator-provenanced, single-source
   `apply_patch` facts; one copyable semantic diff card; unchanged linear
   record; and the existing default-Reject input fence.
7. **Bounded detail views (green)**: current-turn committed-fact archive,
   reasoning moved out of enhanced Focus, exact context estimates, truthful
   compaction chronology, one joined summary Review, and transactional primary-
   screen Inspect/Review panels with local commands and approval takeover.
8. **Remaining product checkpoint**: tables, commands/suggestions, themes, and
   Session picker. Each sub-slice remains independently green and pushed; this
   line is not a claim that they are already implemented.
9. **Release checkpoint**: remove the replaced log renderer, installed-binary
   journeys, screenshots, documentation, full clean-target gates, independent
   review, non-force push, dual-platform CI, and a separate completion-status
   commit.

Each checkpoint must be coherent and green before it is pushed. Phase 11 stays
`in-progress` until the final candidate and status commit both pass the declared
platform matrix.
