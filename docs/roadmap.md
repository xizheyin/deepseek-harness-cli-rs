# v0.1 Roadmap

This roadmap records implementation status. It is a plan, not a list of current product features. `README.md` remains the source for behavior that users can run today.

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

Only one phase may be `in-progress`. A phase becomes `complete` only after its production path, tests, compatibility evidence, validation record, and repository-wide checks pass.

## Deferred beyond v0.1

- Web or desktop GUI
- Cordis/npm plugin compatibility
- MCP, Hooks, Skills, subagents, and background jobs
- Multiple model providers
- Untested operating systems or sandbox claims
- Feature-for-feature or visual copying of Claude Code
