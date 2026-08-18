# Phase 10 validation

## Status

The Phase 10 bounded subprocess-plugin extension is accepted. The immutable
implementation candidate is `f2bd55ca1a2d554198a26d5d9778713ed21dcdef`
on `main`, pushed non-force to `origin/main`. The project remains
`0.1.0-alpha.0`; this acceptance does not publish a crate, prebuilt binary,
tag, GitHub Release, or stable support commitment.

## Candidate identity

- Project and Cargo package: `dsh-rs`
- Installed command: `dsh`
- Library target: `deepseek_harness_cli`
- Rust toolchain: `1.85.0`
- Candidate commit: `f2bd55ca1a2d554198a26d5d9778713ed21dcdef`
- Branch and remote: `main` on
  `git@github.com:xizheyin/deepseek-harness-rs.git`
- Pinned semantic baseline:
  `deepseek-ai/deepseek-harness@47f943859bef60e4160492346772ded9b24f765a`

## Implemented scope

- The Cargo package now matches the public project brand, `dsh-rs`; the
  installed command remains `dsh` and the library target remains
  `deepseek_harness_cli`. The independent HTTP User-Agent changes accordingly
  to `dsh-rs/<version>` and is fixed by the loopback transport test.
- `--plugin-config <PATH>` explicitly loads a private version-1 JSON file; there
  is no discovery, profile, npm, hot-reload, Hook, Provider, or Session extension
  seam.
- Each configured plugin is one owned local subprocess and one managed actor
  thread. The closed, bounded NDJSON protocol contains only `hello`, `call`,
  `cancel`, and `result` semantics.
- Plugin schemas are validated before they reach the model. The output schema is
  host-only and never enters the Provider tool declaration.
- Valid interactive calls use the existing Ask selector. Script/piped calls are
  denied. Invalid arguments, known-unavailable plugins, rejection, and
  cancellation do not dispatch a call.
- A matching result is normalized into the existing tool result model. A call
  that may have been dispatched without a trustworthy result becomes
  `TOOL_OUTCOME_UNKNOWN` and is never automatically replayed after resume.
- Normal exit, startup cancellation, action cancellation, faults, output limits,
  and partial multi-plugin startup all await bounded process-group cleanup.

## Upstream comparison

The semantic baseline remains
`deepseek-ai/deepseek-harness@47f943859bef60e4160492346772ded9b24f765a`.
Upstream has no subprocess-plugin wire ABI: it registers trusted in-process
Cordis `ToolDefinition` values. The transport is therefore an
`intentional-difference`.

The shared observable subset is compared against the committed Phase 5 oracle:

```text
tool/call -> approval/asked -> approval/decided -> tool/result
```

The real plugin CLI test reads its Session JSONL and compares that exact ordered
subset with `tests/fixtures/tools/upstream_phase5_oracle.json`. Rust validates
plugin arguments before Ask, while upstream `defineTool()` validates inside the
execution wrapper after its pre/ask stages; that difference is documented in
the design and compatibility matrix.

## Default offline evidence

The repository gate includes:

- strict JSON, protocol, schema, config, dispatch-evidence, actor lifecycle,
  queue, timeout, startup rollback, and shutdown-race unit tests;
- `tests/plugin_cli.rs` for model-visible schemas, host-only output schemas,
  Session event order, no path leakage, interactive Allow/Reject/Cancel,
  script Deny, invalid arguments, resume with/without explicit config, and
  fail-before-Session invalid config;
- `tests/plugin_examples.rs` for the installed `text_stats_plugin` and
  `json_format_plugin`, wrong ID, extra/duplicate output poisoning,
  post-dispatch crash with no replay, invalid output, matching cancellation,
  ignored cancellation with descendant cleanup, stdout/stderr limits, and
  startup Ctrl+C cleanup;
- `scripts/accept-phase10.sh`, which installs the candidate `dsh`, builds the
  real examples/fault fixture, and runs the installed-plugin journey without a
  real API key or public model request.

Run locally on Rust 1.85.0:

```console
./scripts/verify.sh
./scripts/accept-phase9.sh
./scripts/accept-phase10.sh
git diff --check
```

The 2026-08-18 macOS arm64 pre-publication run passed: 524 library tests plus
every integration/example target in `verify.sh`, 4/4 Phase 9 installed-release
tests, and 11/11 Phase 10 installed-plugin tests, with zero ignored tests.

The install steps may fetch already locked crates when the Cargo cache is empty;
the Agent/plugin scenarios themselves use loopback Provider fixtures and
temporary workspaces.

## Remote acceptance

GitHub Actions [CI run 32133405407](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32133405407)
completed successfully for the exact candidate commit:

- [Ubuntu job 95699262190](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32133405407/job/95699262190),
  completed at `2026-08-18T11:50:48Z` in 4m 12s;
- [macOS job 95699262199](https://github.com/xizheyin/deepseek-harness-rs/actions/runs/32133405407/job/95699262199),
  completed at `2026-08-18T11:52:01Z` in 5m 25s.

Both jobs concluded `success` for `Run repository checks`,
`Verify the installed release journey`, and
`Verify the installed plugin journey`. The pushed implementation tree matches
the locally reviewed candidate. The separate status commit changes only this
acceptance record, the roadmap, and the README status wording.
