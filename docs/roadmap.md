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
| 7 | Interactive CLI/TUI | `in-progress` | — |
| 8 | Persistence, resume, and compaction | `not-started` | — |
| 9 | v0.1 integration and release candidate | `not-started` | — |
| 10 | Bounded subprocess tool plugins and examples | `not-started` | — |

Only one phase may be `in-progress`. A phase becomes `complete` only after its production path, tests, compatibility evidence, validation record, and repository-wide checks pass.

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
