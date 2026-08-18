# Phase 11 validation record

## Status

`in-progress`

Phase 11 is the user-approved TUI v2 extension. This record intentionally has
no candidate commit, publication claim, screenshot digest, or platform success
yet. The shipped production path remains the completed Phase 9 renderer until
each replacement checkpoint becomes reachable and passes its regression gates.

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

## Evidence pending

- production files and default-enabled test names;
- exact-limit and one-over resource tests;
- installed candidate SHA and release acceptance output;
- screenshot sizes and digests generated from real installed PTY bytes;
- macOS and Ubuntu job URLs for the same candidate;
- final compatibility status and README truth audit.

Phase 11 must remain `in-progress` until all of those fields are supported by a
green, pushed candidate and a separate green status commit.
