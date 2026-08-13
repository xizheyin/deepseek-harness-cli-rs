# Compatibility status

This table is the source of truth for claims about compatibility with the pinned DeepSeek Harness revision. Status values are:

- `unresearched`: upstream behavior has not been studied in enough detail;
- `planned`: upstream behavior and a Rust design exist, but implementation does not;
- `partial`: only part of the behavior is implemented;
- `compatible`: deterministic comparison tests support the claim;
- `intentional-difference`: the difference, reason, impact, and tests are recorded.

| Area | Upstream evidence | Rust implementation | Comparison evidence | Status | Known difference |
| --- | --- | --- | --- | --- | --- |
| CLI launcher | `apps/cli/src/args.ts`, `apps/cli/tests/args.spec.ts` | `src/main.rs` exposes only honest help/version and errors | `tests/cli_smoke.rs` | `intentional-difference` | Rust v0.1 is a standalone terminal agent, so profile, plugin, and Web launcher commands are rejected; usage errors return 2 instead of upstream Commander's 1. |
| Session events and projection | Not yet recorded | Not implemented | None | `unresearched` | None recorded |
| DeepSeek streaming provider | Not yet recorded | Not implemented | None | `unresearched` | None recorded |
| Agent Loop | `docs/architecture.md` overview only | Not implemented | None | `unresearched` | None recorded |
| Tool execution pipeline | `docs/architecture.md` overview only | Not implemented | None | `unresearched` | None recorded |
| Approval and file changes | Not yet recorded | Not implemented | None | `unresearched` | None recorded |
| Shell, timeout, and cancellation | Not yet recorded | Not implemented | None | `unresearched` | None recorded |
| Interactive terminal | Not yet recorded | Not implemented | None | `unresearched` | None recorded |
| Persistence and resume | Not yet recorded | Not implemented | None | `unresearched` | None recorded |
| Context compaction | Not yet recorded | Not implemented | None | `unresearched` | None recorded |

No row may become `compatible` without a deterministic behavioral fixture or comparison test tied to an exact upstream source or test path.
