# Phase 4 validation

## Scope

Phase 4 validates an immutable public Rust `ReadOnlyToolRegistry` for `list`,
`glob`, `grep`, and `read`. One capability handle fixes the workspace at registry
construction; strict typed arguments, regular-file checks, stable rendering,
resource budgets, and cooperative cancellation apply to every call. The existing
Agent Loop remains responsible for committing the tool intention before the body,
correlating the result, and replaying that result into the next model request.

This does not claim that `dsh` can read a project. The registry and Agent Loop are
reachable public Rust APIs, but the executable still exposes only help, version,
and argument errors. Writes/patches and approval are Phase 5; shell/process work is
Phase 6; interactive assembly is Phase 7.

## Local validation

- Date: 2026-08-14 (Asia/Shanghai)
- Git base before the Phase 4 checkpoint: `b373920c1725f5882f0733600caa169a639d0dd9`
- Tested Phase 4 checkpoint: pending the first non-force push
- Host: macOS 27.0 (build 26A5406e), arm64
- Rust: `rustc 1.85.0 (4d91de4e4 2025-02-17)`
- Cargo: `cargo 1.85.0 (d73d2caf9 2024-12-31)`
- Default Rust verification public-network use: no; existing HTTP tests use loopback only
- Credentials or API keys used: no; tests use no credential or conspicuous fake values
- User project data modified/read by tests: no; Phase 4 tests use fresh temporary trees

The repository-wide command completed successfully:

```console
./scripts/verify.sh
git diff --check
```

It ran formatting, locked all-target/all-feature compilation, all tests, Clippy
with warnings denied, deterministic whitespace self-tests, and complete-tree
whitespace checks on Rust 1.85.0. All 207 Rust tests passed:

- 20 private read-only-tool argument, encoding, traversal, renderer, and resource tests;
- 13 public registry/Agent integration and upstream-oracle tests;
- 8 macOS read-only security/lifecycle tests;
- 44 Agent Loop tests and 1 retry-key unit test;
- 34 private and 21 public/loopback Provider tests;
- 6 CLI and 9 model-value tests;
- 51 Session/replay/projection/reservation/retry tests.

The locked graph compiled on the repository MSRV. Phase 4 adds four direct
production dependencies:

- `cap-std 3.4.5`: capability-relative filesystem access; Apache-2.0 WITH
  LLVM-exception OR Apache-2.0 OR MIT; crate metadata does not declare an MSRV;
- `globset 0.4.18`: bounded glob parsing/matching; Unlicense OR MIT; crate
  metadata does not declare an MSRV;
- `regex 1.12.3`: bounded byte-regex search; MIT OR Apache-2.0; Rust 1.65;
- `rustix 1.1.4` with `fs`: nonblocking capability-relative special-file open;
  Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT; Rust 1.63.

The two crates without declared `rust-version` are accepted because the complete
locked tree compiled, tested, and linted under Rust 1.85.0 rather than relying on
their package metadata.

## Upstream behavior validation

The official checkout was clean at
`47f943859bef60e4160492346772ded9b24f765a`. Its targeted keyless tool suites
passed with Vitest 4.1.8:

```console
cd /Users/xizheyin/workspace/deepseek-harness-upstream

pnpm exec vitest run \
  packages/core/tools/tests \
  packages/fs/fs-local/tests \
  packages/fs/tool-fs/tests \
  packages/fs/tool-fs-search/tests
# 26 files: 848 passed, 1 Windows-only test skipped on macOS
```

The broader research run had already exercised 30 filesystem/tool files (886
passed, 1 platform skip) plus 56 focused Agent tool-order/cancellation tests. The
credential-controlled filesystem E2E test was inspected but not run because the
phase does not require a live model call.

Node 26.0.0, TypeScript 6.0.3, and the checkout's locked `tsx 4.22.4`
type-checked and generated the Phase 4 oracle twice:

```console
node /Users/xizheyin/workspace/ds-harness-rs/scripts/typecheck-upstream-tool-fixtures.mjs \
  /Users/xizheyin/workspace/deepseek-harness-upstream

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-tool-fixtures.ts \
  /tmp/upstream-phase4-a.json

./node_modules/.bin/tsx --tsconfig tsconfig.base.json \
  /Users/xizheyin/workspace/ds-harness-rs/scripts/generate-upstream-tool-fixtures.ts \
  /tmp/upstream-phase4-b.json

cmp -s /tmp/upstream-phase4-a.json /tmp/upstream-phase4-b.json
cmp -s /tmp/upstream-phase4-a.json \
  /Users/xizheyin/workspace/ds-harness-rs/tests/fixtures/tools/upstream_phase4_oracle.json
```

The checker passed; both outputs were byte-identical and matched the committed
fixture. SHA-256:

- type checker: `aadc60f480c1d6cff1625ab96c143dd500dde154808e30dcb74dda1a217e58ec`;
- generator: `cc18b5233da64026336c209d9ba63d304ab79d010c4f5bfe0819a897ef7763c9`;
- fixture: `32b47ea94ec65168084a31b0a1ee5a2b614865241a380bc97d1ada141296ee0a`.

The Rust comparator uses the exact fixture inputs. It compares canonical small
`read`, `glob`, and `grep` model text plus the missing-file code; consumes schema
field/required facts; verifies the official catalogue has no model-facing
`list`; and pairs official ambient parent/symlink-outside acceptance with Rust
denial. It normalizes only the explicit read-path presentation difference. It
does not call structured upstream `value`/`meta`, large-boundary policy, or every
error compatible; those limitations have separate compatibility rows and Rust
tests.

## Behavior, failure, and safety categories

| Category | Evidence |
| --- | --- |
| Normal | All four real tools run through `AgentLoop`; list type/order, read pagination, glob matching/order, grep file/directory/include, next-request replay, and canonical oracle cases pass. |
| Failure | Missing/not-directory/not-regular, binary/NUL, invalid UTF-8, permission, mutation, oversized file/scan/output, invalid pattern, and ordinary I/O facts become bounded correlated model-facing failures. |
| Rejection | Unknown/missing/extra/wrong/null/range-invalid arguments; parent, sibling, absolute-outside, external/broken/cyclic link, selected directory-link, special file, and unsafe discovered name are rejected before content disclosure. |
| Cancellation | Pre-cancelled Agent calls never dispatch; a pre-cancelled direct registry call returns `ABORTED`; directory batches and file chunks check before starting later filesystem work. |
| Timeout | N/A for a registry-owned timer: Phase 4 starts no process and the existing Agent supplies the tested tool deadline/cleanup grace. A single stuck kernel filesystem call cannot be portably preempted and is explicitly not claimed. |
| Recovery/replay | The durable call/result provenance is checked, the second provider request contains the result, and Session replay never reruns filesystem work. Persistent crash-tail repair remains Phase 8. |
| Writes/approval | N/A: the registry exposes no write, edit, shell, subprocess, or approval capability. Phase 5 adds changes and policy. |
| Confinement | Capability-first root binding, prefix-sibling checks, four-tool escape matrices, external-link denial, directory-link non-traversal, and path-replacement tests prevent reads from moving to a new ambient root. |
| Resource safety | Shared counters have exact/one-over tests, with representative fixed tests for 16 MiB read, 32 MiB grep aggregate, 1 MiB line, 10,000 matches, depth 64, and 64 KiB compact-JSON output. No result is presented as a complete partial scan. |
| Privacy | Tool/argument/file-content carrying private types do not implement payload-revealing `Debug`; failures omit absolute roots, targets, raw OS strings, and contents. Authorized result text remains deliberately model-visible and durable. |

## Independent review

Three independent read-only reviews covered upstream compatibility, API/test
evidence, and path/resource/cancellation safety. They found and drove fixes for:

- capability-root startup and reopen time-of-check/time-of-use windows;
- FIFO blocking, synchronous metadata on an async worker, and cancellation checks
  that occurred after a directory operation had already started;
- direct and discovered directory-symlink traversal, external/broken/cyclic file
  links, and ordinary permission failures misclassified as workspace escape;
- explicit `null`, schema/parser structural drift, UTF-8 byte semantics, and a
  public integral-number path hidden by a private parser test;
- high-offset/read-control JSON escaping, long-line suffix parity, traversal and
  aggregate-budget off-by-one errors, and grep's one-file input overshoot;
- raw file/argument/match payloads exposed through derived `Debug`;
- oracle inputs, schema facts, outside read/glob/grep pairs, real-registry durable
  order/provenance, next-request replay, and overly broad compatibility wording.

After the fixes, the safety reviewer reported no reproducible containment,
special-file, cancellation, resource-growth, or privacy blocker. The upstream
review reported a reproducible oracle and the API review found only documentation
and evidence wording to close before checkpointing.

## Known limitations

- `ReadOnlyToolRegistry` is a public library API but is not assembled into `dsh`;
  the current executable cannot inspect a project or call a model.
- Rust adds model-facing `list`; upstream has only an internal one-level service.
- Canonical small read/glob/grep text is compared, but Rust displays relative
  paths and does not implement upstream structured success `value`/presentation
  `meta`.
- Rust is deliberately stricter: one fixed workspace capability, closed args,
  regular files, no recursive directory links, and fixed scan/result budgets.
- An internal file link is accepted only when the capability can resolve its
  relative target inside the root; absolute-target links are rejected even if
  they ultimately name an internal file.
- Filesystem contents can change during a call. Handle metadata detects practical
  replacement cases, but this is not a snapshot guarantee.
- Cancellation is cooperative between bounded operations. A broken FUSE/network
  filesystem can trap one kernel call beyond the Agent deadline; no hard-kill
  helper process exists before Phase 6.
- Non-UTF-8 discovered names are tested on Ubuntu/Linux. The macOS fixture cannot
  create that byte sequence; both platforms test control-character rejection.
- Bidirectional-format characters are not Rust `char::is_control`; Phase 7 must
  escape untrusted paths/content when rendering the terminal.
- No real DeepSeek API call was run. Evidence is the pinned executable oracle,
  official keyless tests, fake providers, and the already-validated loopback path.

## Remote acceptance

Pending the coherent Phase 4 checkpoint push and exact Ubuntu GitHub Actions run.
