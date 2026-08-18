# Phase 11 validation record

## Status

`in-progress`

Phase 11 is the user-approved TUI v2 extension. This record intentionally has
no final release candidate, publication claim, screenshot digest, or platform
success yet. The shipped production path remains the completed Phase 9 renderer
until each replacement checkpoint becomes reachable and passes its regression
gates.

## Frozen boundary

The accepted design is [`../design/tui-v2.md`](../design/tui-v2.md). It keeps
the DeepSeek Harness semantic baseline and the existing Session, approval,
cancellation, plugin, and process-cleanup contracts. The default experience is
an inline, native-scrollback Focus view with a bounded dynamic dock; explicit
Inspect, Review, and Session-picker views may temporarily use the alternate
screen. `--tui linear` is the planned no-dynamic-control accessible path.

The current implementation work is divided into these green checkpoints:

1. semantic `UiProjector`, tool lifecycle, receipt, and plain renderer;
2. long-lived cbreak ownership, Unicode decoder/composer, history, paste, and
   next-turn queue;
3. bounded inline dock, resize, throttling, and scrollback uniqueness;
4. Markdown/diff, approval, Focus/Inspect/Review, themes, context, compaction,
   commands, suggestions, and Session picker;
5. installed-binary PTY journey, real screenshots, clean-target repository
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

This slice does **not** implement the Phase 11 receipt, plain renderer, composer,
dock, Markdown/diff presentation, alternate-screen views, or PTY journey. It
does not change ordinary Phase 9 terminal bytes except that a historical prune
replacement is now correctly silent instead of looking like a duplicate tool
result.

Local validation used Rust 1.85.0 on macOS arm64 without network access, a real
API key, or model billing. `./scripts/verify.sh` passed on the final working
tree: formatting, all-target checks, 547 library tests plus 305 integration
tests (852 total, zero failed and zero ignored), Clippy with warnings denied,
and whitespace checks. Focused suites additionally name the new boundaries in
`session::observer::tests`, `tui::projector::tests`, `cli::live::tests`, and
`session::phase7_tests`. The immutable commit ID and push result are filled in
after this slice is committed.

## Evidence pending

- production files and default-enabled test names;
- exact-limit and one-over resource tests;
- installed candidate SHA and release acceptance output;
- screenshot sizes and digests generated from real installed PTY bytes;
- macOS and Ubuntu job URLs for the same candidate;
- final compatibility status and README truth audit.

Phase 11 must remain `in-progress` until all of those fields are supported by a
green, pushed candidate and a separate green status commit.
