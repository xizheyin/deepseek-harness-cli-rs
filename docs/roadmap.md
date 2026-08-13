# v0.1 Roadmap

This roadmap records implementation status. It is a plan, not a list of current product features. `README.md` remains the source for behavior that users can run today.

| Phase | Scope | Status | Acceptance record |
| --- | --- | --- | --- |
| 0 | Reproducible Rust CLI foundation | `complete` | [`validation/phase-0.md`](validation/phase-0.md) |
| 1 | Core types and in-memory session | `complete` | [`validation/phase-1.md`](validation/phase-1.md) |
| 2 | DeepSeek streaming provider | `in-progress` | Pending |
| 3 | Agent Loop | `not-started` | — |
| 4 | Read-only tools | `not-started` | — |
| 5 | File changes and approval | `not-started` | — |
| 6 | Shell, timeout, and cancellation | `not-started` | — |
| 7 | Interactive CLI/TUI | `not-started` | — |
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
