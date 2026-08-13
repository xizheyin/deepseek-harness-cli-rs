# Upstream baseline

DeepSeek Harness is the semantic reference for this project's agent core. The Rust implementation targets observable behavior, not a line-by-line translation of TypeScript or Cordis.

## Pinned revision

- Repository: <https://github.com/deepseek-ai/deepseek-harness>
- Commit: [`47f943859bef60e4160492346772ded9b24f765a`](https://github.com/deepseek-ai/deepseek-harness/tree/47f943859bef60e4160492346772ded9b24f765a)
- Commit date: 2026-08-13
- Baseline checked: 2026-08-14
- Upstream license at this revision: MIT

The baseline must not move as part of ordinary feature work. Updating it requires a dedicated compatibility review and regenerated behavioral fixtures.

## Phase 0 inspection

The following files were inspected at the pinned revision:

- [`LICENSE`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/LICENSE): upstream license.
- [`AGENTS.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/AGENTS.md): repository invariants, validation, and keyless-test rules.
- [`package.json`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/package.json): upstream build and test gates.
- [`docs/architecture.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/docs/architecture.md): plugin architecture, event domains, turn flow, and append-only session log.
- [`docs/testing.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/docs/testing.md): deterministic, keyless, snapshot, and live-API test tiers.
- [`apps/cli/README.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/apps/cli/README.md): official launcher purpose and modes.
- [`apps/cli/package.json`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/apps/cli/package.json): official package and binary naming.
- [`apps/cli/src/args.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/apps/cli/src/args.ts): official CLI grammar and non-zero error behavior.
- [`apps/cli/tests/args.spec.ts`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/apps/cli/tests/args.spec.ts): CLI behavior tests.
- [`.github/workflows/ci.yml`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/.github/workflows/ci.yml): upstream automated gates.
- [`THIRD_PARTY_NOTICES.md`](https://github.com/deepseek-ai/deepseek-harness/blob/47f943859bef60e4160492346772ded9b24f765a/THIRD_PARTY_NOTICES.md): notices for upstream's own dependency and vendored-source closure.

The current Phase 0 tree copies no upstream source code, test code, or fixture. It carries forward only the engineering intent of a named `dsh` executable, honest non-zero failures, pinned dependencies, and automated checks. Upstream's third-party notice set therefore does not describe this zero-dependency Rust tree and is not copied wholesale.

Later phases will add exact source/test paths as their behavior is studied. If implementation copies or adapts a substantial portion of upstream source, tests, or fixtures, that change must preserve the applicable DeepSeek MIT notice and audit any embedded third-party material.

## Local research copy

Developers may create a clone outside this repository and detach it at the baseline:

```console
git clone https://github.com/deepseek-ai/deepseek-harness.git ../deepseek-harness-upstream
git -C ../deepseek-harness-upstream checkout --detach 47f943859bef60e4160492346772ded9b24f765a
```

The upstream clone is research input and must not be committed here.
