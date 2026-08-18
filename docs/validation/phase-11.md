# Phase 11 validation record

## Status

`in-progress`

Phase 11 is the user-approved TUI v2 extension. This record intentionally has
no final release candidate, Phase 11 completion claim, screenshot digest, or
platform success yet. A production-reachable enhanced composer and inline Dock
now exist
on conservative terminal profiles; the strict Phase 9 linear path remains the
fallback. Phase 11 is still partial because semantic tool cards/receipts,
Markdown/diff, Inspect/Review, theme polish, the Session picker, installed Phase
11 acceptance, real screenshots, real-emulator evidence, and dual-platform CI
are not complete.

## Frozen boundary

The accepted design is [`../design/tui-v2.md`](../design/tui-v2.md). It keeps
the DeepSeek Harness semantic baseline and the existing Session, approval,
cancellation, plugin, and process-cleanup contracts. The default experience is
an inline, native-scrollback Focus view with a bounded dynamic dock; explicit
Inspect, Review, and Session-picker views may temporarily use the alternate
screen. `--tui linear` is the implemented zero-ESC, no-dynamic-control
accessible path.

The implementation work is divided into these checkpoints:

1. semantic `CommittedUiEvent` and `UiProjector` foundation;
2. long-lived cbreak ownership, Unicode decoder/composer, history, safe paste,
   next-turn queue, bounded inline dock, enhanced approval, and resize recovery;
3. semantic tool cards/receipt, Markdown/diff, Focus/Inspect/Review, themes,
   context, compaction, commands, suggestions, and Session picker;
4. installed-binary PTY journey, real screenshots, clean-target repository
   gates, independent review, and macOS/Ubuntu CI.

## Semantic foundation slice — 2026-08-19

The first part of checkpoint 1 is implemented behind the unchanged Phase 9
renderer:

- `CommittedUiEvent` retains bounded user, assistant, usage, request-context,
  tool, retry, approval, and compaction facts. Large opaque payloads are either
  retained up to 64 KiB or explicitly marked omitted; identifiers use a 4 KiB
  display bound plus a stable fingerprint for correlation. The opaque-payload
  and identity Debug views report sizes rather than their retained text.
- `UiProjector` owns `(turn, step, call-id)` tool lifecycles, distinguishes
  requested/approval/completed/failed/unknown facts, treats a nonzero shell exit
  as neutral completion rather than invented success, and interprets patch or
  shell metadata only when the exact known contract shape is present.
- prune markers remain pending until the immediately following historical
  surface replacement confirms them. That replacement is not treated or
  rendered as a second tool execution. The recorded token count is explicitly
  the old node's shadowed estimate, not a claim about tokens removed.
- wider memory-compatible/imported fact sequences degrade through bounded
  conflict/omission counters rather than cancelling valid Agent work. The
  existing renderer observes the projector only in fail-open shadow mode.

This slice did **not** implement the Phase 11 receipt, enhanced composer/dock,
Markdown/diff presentation, alternate-screen views, or PTY journey. Those
boundaries are updated by the following slice rather than retroactively claimed
by the semantic commit.

Local validation used Rust 1.85.0 on macOS arm64 without network access, a real
API key, or model billing. `./scripts/verify.sh` passed on the final working
tree: formatting, all-target checks, 547 library tests plus 305 integration
tests (852 total, zero failed and zero ignored), Clippy with warnings denied,
and whitespace checks. Focused suites additionally name the new boundaries in
`session::observer::tests`, `tui::projector::tests`, `cli::live::tests`, and
`session::phase7_tests`. The immutable implementation commit is
`bc08c6e12a0b8de3e95ab5e31948b3aebf4aba77`; it was pushed non-forced to
`origin/main`. The preceding design commit is
`dc616e46bf595ec1b45aa971dd9336891a44eb0a`.

## Composer and inline-dock slice — 2026-08-19

Implementation commit
`88ddadf1504f553294787ae0040df2dd29298113` makes these production paths real:

- `--tui auto|enhanced|linear` is a closed CLI surface. Linear is always plain
  and contains zero ESC bytes. No color, `TERM=dumb`, an unknown terminal,
  tmux/Screen/Zellij, or an initial terminal below 44×12 makes Auto choose the
  conservative linear path. Explicit enhanced remains an opt-in escape hatch
  for known multiplexers, while the same initial geometry gate still applies.
- `TerminalSession` owns long-lived cbreak/no-echo mode, preserves kernel
  signal handling, distinguishes carriage-return submit from Ctrl+J newline,
  enables bracketed paste, and restores the exact original termios on ordinary,
  signal, suspend, EOF, output-failure, and unwind paths.
- `KeyDecoder`, `Composer`, and `InputMemory` implement fragmented UTF-8 and
  escape decoding, grapheme-safe Unicode editing, multiline navigation,
  bounded undo/yank, current-process committed history, reverse search, atomic
  64 KiB paste, and an eight-item/256 KiB next-turn FIFO. Queued text enters
  Session only when it becomes the admitted next turn; cancellation and
  suspension cannot confuse a committed prompt with a local draft.
- `InlineScreen` is the sole coordinate owner. It uses only full-screen native
  scrolling, a fixed bottom Dock, transactional generation checks, a software
  cursor, and a small deterministic terminal model. Partial coordinate writes
  poison the ledger; in-process ED2 recovery keeps paste framing enabled, while
  suspend/exit ED2 disables paste and restores the real cursor. Partially drawn
  draft or approval text is not scrolled into history during recovery.
- enhanced approval keeps Reject selected, renders the trusted preview once,
  and grants only after a current-epoch direction key followed by a later CR
  Enter. Printable shortcuts, paste, unknown input, Ctrl+J, and direction plus
  Enter in the same read cannot authorize a side effect. The strict linear
  selector retains its established record-oriented behavior.
- an already enhanced session has a compact 12×5 rescue Dock. Below that exact
  floor it clears stale geometry, restores the terminal, and fails closed.

The two small dependencies are pinned: `unicode-segmentation = 1.13.3` provides
extended grapheme boundaries at the repository's Rust 1.85 MSRV, and
`unicode-width = 0.2.2` (default features disabled) provides deterministic cell
widths. Both are MIT OR Apache-2.0 and add no ordinary transitive dependency.

Local validation used Rust 1.85.0 on macOS arm64 with fake models, loopback
HTTP, temporary workspaces, and obvious fake credentials. `./scripts/verify.sh`
passed on the implementation tree: formatting, all-target checks, 610 library
tests plus 327 other tests (937 total), zero failed/ignored, Clippy with warnings
denied, and whitespace checks. The same run includes all four Phase 9 release
acceptance tests and all eleven real/fault plugin example tests. High-value PTY
regressions cover Unicode/CR-vs-LF, fragmented and rejected paste fences, busy
draft/FIFO admission, cancel-then-continue, exact terminal restoration,
directional approval, 44×12 startup and 12×5 runtime geometry, conservative
Auto profiles, output deadlines, partial writes, and signal identity.

This is a green implementation checkpoint, not a Phase 11 release candidate.
Its docs evidence is committed immediately after the implementation anchor and
both commits are published together without force. Real iTerm/Terminal/VS Code
resize/reflow/copy capture, production tool cards and receipt, Phase 11
installed acceptance, and current enhanced screenshots remain pending.

## Evidence pending

- remaining product files and default-enabled Phase 11 acceptance tests;
- exact-limit and one-over tests for later product/view resources;
- installed candidate SHA and release acceptance output;
- screenshot sizes and digests generated from real installed PTY bytes;
- macOS and Ubuntu job URLs for the same candidate;
- final compatibility status and README truth audit.

Phase 11 must remain `in-progress` until all of those fields are supported by a
green, pushed candidate and a separate green status commit.
