# Product Roadmap

This roadmap records implementation status. Phases 0–9 remain the finite v0.1 plan; Phase 10 is an explicitly approved post-v0.1 extension. This is a plan, not a list of current product features. `README.md` remains the source for behavior that users can run today.

| Phase | Scope | Status | Acceptance record |
| --- | --- | --- | --- |
| 0 | Reproducible Rust CLI foundation | `complete` | [`validation/phase-0.md`](validation/phase-0.md) |
| 1 | Core types and in-memory session | `complete` | [`validation/phase-1.md`](validation/phase-1.md) |
| 2 | DeepSeek streaming provider | `complete` | [`validation/phase-2.md`](validation/phase-2.md) |
| 3 | Agent Loop | `complete` | [`validation/phase-3.md`](validation/phase-3.md) |
| 4 | Read-only tools | `complete` | [`validation/phase-4.md`](validation/phase-4.md) |
| 5 | File changes and approval | `complete` | [`validation/phase-5.md`](validation/phase-5.md) |
| 6 | Shell, timeout, and cancellation | `complete` | [`validation/phase-6.md`](validation/phase-6.md) |
| 7 | Interactive CLI/TUI | `complete` | [`validation/phase-7.md`](validation/phase-7.md) |
| 8 | Local session continuity and one-pass automatic context compaction | `complete` | [`validation/phase-8.md`](validation/phase-8.md) |
| 9 | v0.1 integration and release candidate | `complete` | [`validation/phase-9.md`](validation/phase-9.md) |
| 10 | Bounded subprocess tool plugins and examples | `complete` | [`validation/phase-10.md`](validation/phase-10.md) |

Only one phase may be `in-progress`. A phase becomes `complete` only after its production path, tests, compatibility evidence, validation record, and repository-wide checks pass.

## Phase 8 revised boundary (2026-08-18)

The user explicitly prioritized a useful Agent over database-grade session
durability. Phase 8 therefore requires only:

- bounded local JSONL save/list/resume for a normally closed current-version
  session;
- clear refusal of corrupt or unsupported history before a new model request or
  tool side effect;
- no automatic replay of a tool whose previous outcome is unknown;
- one bounded automatic summary transaction on the real Agent path when the
  committed context reaches its pressure threshold or the pending request does
  not fit;
- preservation of a recent balanced tail, followed by a successful retry of
  the same user input;
- clear failure, with no loop or tool replay, when that one reduction still
  cannot make the request fit;
- repository-wide checks, one real CLI save/resume smoke, one real automatic-
  compaction-and-continue acceptance test, Phase 8 evidence, and independent
  review.

The following are useful hardening work but no longer block v0.1: power-loss
durability, proof for every crash/repair prefix, NFS or cloud-filesystem
semantics, a near-512-MiB cold-scan stress proof, exhaustive old/future schema
migration, provider-overflow automatic replay, and the complete
32/64/96/192-MiB physical-allocation ownership proof. Existing tested locks,
barriers, repair code, and limits remain in place. The tradeoff is explicit:
`SIGKILL`, power loss, disk failure, or filesystem failure may lose the final
session tail or make that session impossible to resume.

## Phase 9 terminal experience gate (2026-08-18)

The user explicitly requires a polished Claude Code-style terminal experience,
not merely a technically functional line protocol. Phase 9 therefore also
requires a real, tested interaction pass before v0.1 can be called ready:

- permission prompts use a compact selection UI with Allow once, Reject, and
  Cancel; a human must never need to copy a random identifier;
- the selected action is visually clear, Enter confirms, Escape cancels, and
  Ctrl+C still cancels the current turn without leaking a side effect;
- file approvals present a readable diff, while Shell approvals present the
  exact command, working directory, and important environment changes;
- streaming text, working/tool state, errors, and the next input prompt have a
  consistent hierarchy; product-owned color may enhance a terminal, but
  `--no-color` and non-TTY output remain readable;
- terminal mode is restored after approval, cancellation, suspension, EOF,
  output failure, and every supported exit signal;
- real PTY tests cover keyboard selection, stale pasted input, cancellation,
  cleanup, and terminal restoration on macOS and Ubuntu.

The short `y`/`n`/`c` line answers added during Phase 8 are an immediate
usability repair, not completion of this Phase 9 gate.

## Phase 10 boundary

Phase 10 starts only after Phase 9 is complete. It adds explicitly configured local tool-plugin executables, not Cordis/npm compatibility or a general extension framework. The first protocol stays deliberately small: bounded versioned NDJSON over stdin/stdout, targeting only `hello`, `call`, `cancel`, and `result`; stderr is bounded diagnostics. Plugin tools still pass through dsh's existing schema validation, approval, append-only intent/result recording, cancellation, timeout, and owned process cleanup.

Acceptance requires two useful no-side-effect examples (`text-stats` and `json-format`) plus one protocol/cancellation fault plugin, all exercised through the real CLI. The default offline matrix must cover malformed and oversized messages, crash, timeout, cancellation, backpressure, restart/configuration, and absence of orphan processes on macOS and Ubuntu. A plugin remains a trusted local executable rather than a sandboxed capability.

## Still deferred

- Web or desktop GUI
- Cordis/npm plugin compatibility, arbitrary hooks, hot reload, and native dynamic libraries
- MCP, Hooks, Skills, subagents, and background jobs
- Multiple model providers
- Untested operating systems or sandbox claims
- Feature-for-feature or visual copying of Claude Code
