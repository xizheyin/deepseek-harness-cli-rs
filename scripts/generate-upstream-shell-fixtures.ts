/**
 * Deterministic Phase 6 foreground-shell oracle for DeepSeek Harness.
 *
 * Run from the clean pinned upstream checkout with its locked tsx binary. The
 * script uses the real upstream tool, shell executor, and subprocess runtime;
 * it writes only below a fresh temporary tree and the optional output path.
 */

import { execFileSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { mkdir, mkdtemp, realpath, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { setTimeout as sleep } from 'node:timers/promises'
import { Context } from '@deepseek-ai/cordis'
import { LocalBashExecutor } from '@deepseek-ai/dsh-bash-local'
import { CallId } from '@deepseek-ai/dsh-llm'
import type { ShellRunResult } from '@deepseek-ai/dsh-shell'
import * as ShellEnvPlugin from '@deepseek-ai/dsh-shell-env'
import LocalSubprocessRuntime from '@deepseek-ai/dsh-subprocess-local'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import * as ToolBash from '@deepseek-ai/dsh-tool-bash'
import ToolRuntime, { TOOL_ABORTED } from '@deepseek-ai/dsh-tools'
import type {
  ToolExecutionResult,
  ToolExecutionSuccess,
} from '@deepseek-ai/dsh-tools'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'
const REPOSITORY = 'https://github.com/deepseek-ai/deepseek-harness'
const SHIPPED_BASE_CONFIG = 'packages/bundle/base/cordis.patch.yml'
const ORACLE_BASE_PATH = '/bin:/usr/bin'

const SOURCE_PATHS = [
  'packages/bundle/base/cordis.patch.yml',
  'packages/core/tools/src/index.ts',
  'packages/core/tools/src/schema.ts',
  'packages/shell/tool-bash/src/index.ts',
  'packages/shell/tool-bash/src/render.ts',
  'packages/shell/bash-local/src/index.ts',
  'packages/shell/shell/src/index.ts',
  'packages/shell/shell/src/types.ts',
  'packages/shell/shell-env/src/index.ts',
  'packages/subprocess/subprocess/src/index.ts',
  'packages/subprocess/subprocess-local/src/index.ts',
  'packages/subprocess/subprocess-local/src/process-inspector.ts',
  'packages/subprocess/subprocess-local/src/spawn.ts',
] as const

const TEST_PATHS = [
  'packages/core/tools/tests/schema.spec.ts',
  'packages/core/tools/tests/tools.spec.ts',
  'packages/shell/tool-bash/tests/tools.spec.ts',
  'packages/shell/tool-bash/tests/integration.spec.ts',
  'packages/shell/bash-local/tests/executor.spec.ts',
  'packages/shell/shell/tests/render.spec.ts',
  'packages/shell/shell/tests/service.spec.ts',
  'packages/shell/shell-env/tests/shell-env.spec.ts',
  'packages/subprocess/subprocess/tests/service.spec.ts',
  'packages/subprocess/subprocess-local/tests/spawn.spec.ts',
  'packages/subprocess/subprocess-local/tests/local.spec.ts',
  'packages/subprocess/subprocess-local/tests/process-inspector.spec.ts',
] as const

interface WorkspaceFixture {
  root: string
  workspace: string
  nested: string
  spill: string
  emptyPath: string
}

interface ToolCaller {
  call(args: unknown, signal?: AbortSignal): Promise<ToolExecutionResult>
  callIds(): string[]
}

interface ForegroundValue {
  kind: 'foreground'
  exitCode: number | null
  signal: string | null
  timedOut: boolean
  aborted: boolean
  timeoutMs: number
  stdout: { text: string; truncated: boolean; spillPath?: string }
  stderr: { text: string; truncated: boolean; spillPath?: string }
}

const ENV_KEYS = [
  'PHASE6_ORACLE_TOKEN',
  'PHASE6_ORACLE_SAFE',
  'DSH_PHASE6_ORACLE_AMBIENT',
] as const

function requireCondition(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(`oracle assertion failed: ${message}`)
}

function expectSuccess(result: ToolExecutionResult, label: string): ToolExecutionSuccess {
  if (result.isError) throw new Error(`${label} unexpectedly failed: ${result.error.message}`)
  return result
}

function replaceEvery(value: string, search: string, replacement: string): string {
  return value.split(search).join(replacement)
}

function normalizeText(value: string, fixture: WorkspaceFixture): string {
  return replaceEvery(
    replaceEvery(value, fixture.workspace, '<workspace>'),
    fixture.root,
    '<fixture-root>',
  )
}

function normalize(value: unknown, fixture: WorkspaceFixture): unknown {
  if (typeof value === 'string') return normalizeText(value, fixture)
  if (Array.isArray(value)) return value.map(item => normalize(item, fixture))
  if (value === null || typeof value !== 'object') return value
  return Object.fromEntries(
    Object.entries(value).map(([key, child]) => [key, normalize(child, fixture)]),
  )
}

function normalizeResult(result: ToolExecutionResult, fixture: WorkspaceFixture): unknown {
  const durableShape = result.isError
    ? {
        isError: true,
        error: result.error,
        content: result.content,
        ...(result.meta === undefined ? {} : { meta: result.meta }),
      }
    : {
        isError: false,
        value: result.value,
        content: result.content,
        ...(result.meta === undefined ? {} : { meta: result.meta }),
      }
  return normalize(durableShape, fixture)
}

/** Timeout termination status varies by shell/platform; retain its stable facts only. */
function normalizeTimeoutResult(result: ToolExecutionResult, fixture: WorkspaceFixture): unknown {
  const success = expectSuccess(result, 'timeout')
  const value = success.value as unknown as ForegroundValue
  const content = success.content.map(block => block.type === 'text'
    ? {
        ...block,
        text: normalizeText(block.text, fixture)
          .replace(/\n\[killed by signal: [^\]]+\]$/, '')
          .replace(/\n\[exit code: -?\d+\]$/, ''),
      }
    : block)
  return {
    isError: false,
    value: {
      kind: value.kind,
      termination: '<normalized-after-timeout>',
      timedOut: value.timedOut,
      aborted: value.aborted,
      timeoutMs: value.timeoutMs,
      stdout: normalize(value.stdout, fixture),
      stderr: normalize(value.stderr, fixture),
    },
    content,
    ...(success.meta === undefined ? {} : { meta: normalize(success.meta, fixture) }),
  }
}

function textOf(result: ToolExecutionResult): string {
  return result.content
    .filter((block): block is Extract<typeof block, { type: 'text' }> => block.type === 'text')
    .map(block => block.text)
    .join('')
}

function makeCaller(
  ctx: Context,
  fixture: WorkspaceFixture,
  callPrefix = 'phase6-direct',
): ToolCaller {
  let ordinal = 0
  const ids: string[] = []
  return {
    async call(
      args: unknown,
      signal = new AbortController().signal,
    ): Promise<ToolExecutionResult> {
      ordinal += 1
      const callId = `${callPrefix}-${String(ordinal).padStart(2, '0')}`
      ids.push(callId)
      return ctx.tools.execute({
        signal,
        callId: CallId(callId),
        name: 'bash',
        arguments: args,
        agent: {
          id: 'phase6-oracle-agent',
          session: {
            header: {
              version: 0,
              id: 'phase6-oracle-session',
              createdAt: 0,
              cwd: fixture.workspace,
            },
            events: [],
          },
        } as never,
      })
    },
    callIds(): string[] {
      return [...ids]
    },
  }
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'\"'\"'`)}'`
}

async function waitForPidFile(path: string, label: string): Promise<number> {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    try {
      const text = readFileSync(path, 'utf8').trim()
      if (/^\d+$/.test(text)) {
        const pid = Number(text)
        if (Number.isSafeInteger(pid) && pid > 0) return pid
      }
    } catch (error: unknown) {
      if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
    }
    await sleep(10)
  }
  throw new Error(`oracle assertion failed: ${label} did not publish one positive decimal pid`)
}

function processIsAlive(pid: number): boolean {
  try {
    process.kill(pid, 0)
    return true
  } catch (error: unknown) {
    if ((error as NodeJS.ErrnoException).code === 'ESRCH') return false
    throw error
  }
}

async function createWorkspace(): Promise<WorkspaceFixture> {
  const canonicalTmp = await realpath(tmpdir())
  const root = await mkdtemp(join(canonicalTmp, 'dsh-phase6-oracle-'))
  const workspace = join(root, 'workspace')
  const nested = join(workspace, 'nested')
  const spill = join(root, 'spill')
  const emptyPath = join(root, 'empty-path')
  await mkdir(nested, { recursive: true })
  await mkdir(spill)
  await mkdir(emptyPath)
  return { root, workspace, nested, spill, emptyPath }
}

async function boot(
  fixture: WorkspaceFixture,
  enableRunInBackground: boolean | undefined,
  oracleGraceMs?: number,
): Promise<Context> {
  const ctx = new Context()
  await ctx.plugin(SystemPrompt)
  await ctx.plugin(ToolRuntime)
  await ctx.plugin(LocalSubprocessRuntime)
  ;(ctx.subprocess as LocalSubprocessRuntime).internals = { spillDir: fixture.spill }
  await ctx.plugin(ShellEnvPlugin, { dshHome: join(fixture.workspace, '.dsh-home') })
  // The execution context overrides only grace to keep the real timeout
  // scenario quick. A second context below retains every library default.
  await ctx.plugin(
    LocalBashExecutor,
    oracleGraceMs === undefined ? {} : { graceMs: oracleGraceMs },
  )
  await ctx.plugin(
    ToolBash,
    enableRunInBackground === undefined ? {} : { enableRunInBackground },
  )
  return ctx
}

function schemaSurface(ctx: Context): unknown {
  const modelSchema = ctx.tools.schemas().find(schema => schema.name === 'bash')
  const definition = ctx.tools.get('bash')
  requireCondition(modelSchema !== undefined, 'bash model schema must exist')
  requireCondition(definition !== undefined, 'bash definition must exist')
  const parameters = modelSchema.parameters as {
    properties?: Record<string, unknown>
    additionalProperties?: unknown
  }
  const output = definition.output.schema as {
    oneOf?: Array<{ properties?: Record<string, { const?: unknown }> }>
  }
  const parameterNames = Object.keys(parameters.properties ?? {})
  const backgroundOutputPresent = output.oneOf?.some(branch =>
    branch.properties?.kind?.const === 'background') ?? false
  return {
    composition: {
      enableRunInBackground: false,
      sandboxExecutorMounted: false,
    },
    modelSchema: structuredClone(modelSchema),
    outputSchema: structuredClone(definition.output.schema),
    timeoutMs: definition.timeoutMs,
    checks: {
      exactForegroundParameterOrder: JSON.stringify(parameterNames)
        === JSON.stringify(['command', 'description', 'timeoutMs', 'workdir']),
      requiredCommandAndDescription: JSON.stringify(
        (modelSchema.parameters as { required?: string[] }).required,
      ) === JSON.stringify(['command', 'description']),
      implicitParameterRootIsOpen: !Object.hasOwn(parameters, 'additionalProperties'),
      backgroundInputAbsent: !parameterNames.includes('run_in_background'),
      sandboxEscalationInputAbsent: !parameterNames.includes('sandbox_permissions')
        && !parameterNames.includes('justification'),
      backgroundOutputUnionStillPresent: backgroundOutputPresent,
    },
  }
}

function defaultBackgroundSurface(ctx: Context): unknown {
  const schema = ctx.tools.schemas().find(candidate => candidate.name === 'bash')
  requireCondition(schema !== undefined, 'default bash schema must exist')
  const parameters = schema.parameters as { properties?: Record<string, unknown> }
  const parameterNames = Object.keys(parameters.properties ?? {})
  return {
    composition: {
      enableRunInBackground: 'upstream plugin default true',
      sandboxExecutorMounted: false,
    },
    inputParameterNames: parameterNames,
    checks: {
      upstreamPluginDefaultExposesBackground: parameterNames.includes('run_in_background'),
    },
  }
}

function foregroundValue(result: ToolExecutionResult, label: string): ForegroundValue {
  return expectSuccess(result, label).value as unknown as ForegroundValue
}

async function withOracleEnvironment<T>(operation: () => Promise<T>): Promise<T> {
  const previous = Object.fromEntries(ENV_KEYS.map(key => [key, process.env[key]]))
  process.env.PHASE6_ORACLE_TOKEN = 'obviously-fake-token'
  process.env.PHASE6_ORACLE_SAFE = 'safe-value'
  process.env.DSH_PHASE6_ORACLE_AMBIENT = 'stale-value'
  try {
    return await operation()
  } finally {
    for (const key of ENV_KEYS) {
      const value = previous[key]
      if (value === undefined) delete process.env[key]
      else process.env[key] = value
    }
  }
}

async function withProcessEnvironment<T>(
  name: string,
  value: string | undefined,
  operation: () => Promise<T>,
): Promise<T> {
  const previous = process.env[name]
  if (value === undefined) delete process.env[name]
  else process.env[name] = value
  try {
    return await operation()
  } finally {
    if (previous === undefined) delete process.env[name]
    else process.env[name] = previous
  }
}

async function processScenarios(
  ctx: Context,
  fixture: WorkspaceFixture,
): Promise<unknown> {
  const caller = makeCaller(ctx, fixture)
  const comparisonTimeoutMs = 25_000
  const successInput = {
    command: "printf 'hello\\n'",
    description: 'Print a greeting',
    timeoutMs: comparisonTimeoutMs,
  }
  const silentInput = {
    command: ':',
    description: 'Produce no output',
    timeoutMs: comparisonTimeoutMs,
  }
  const mixedInput = {
    command: "printf 'out\\n'; printf 'err\\n' >&2",
    description: 'Print both output streams',
    timeoutMs: comparisonTimeoutMs,
  }
  const nonzeroInput = {
    command: "printf 'failing\\n'; exit 3",
    description: 'Exit with status three',
    timeoutMs: comparisonTimeoutMs,
  }
  const selfSignalInput = {
    command: 'kill -TERM $$',
    description: 'Terminate the shell with SIGTERM',
    timeoutMs: comparisonTimeoutMs,
  }
  const timeoutInput = {
    command: "trap 'exit 0' TERM; while :; do :; done",
    description: 'Wait until command timeout',
    timeoutMs: 100,
  }

  const success = await caller.call(successInput)
  const silent = await caller.call(silentInput)
  const mixed = await caller.call(mixedInput)
  const nonzero = await caller.call(nonzeroInput)
  const selfSignal = await caller.call(selfSignalInput)
  const timeout = await caller.call(timeoutInput)

  const defaultWorkdirInput = { command: 'pwd', description: 'Print default work directory' }
  const relativeWorkdirInput = {
    command: 'pwd',
    description: 'Print relative work directory',
    workdir: 'nested',
  }
  const absoluteWorkdirInput = {
    command: 'pwd',
    description: 'Print absolute work directory',
    workdir: fixture.nested,
  }
  const defaultWorkdir = await caller.call(defaultWorkdirInput)
  const relativeWorkdir = await caller.call(relativeWorkdirInput)
  const absoluteWorkdir = await caller.call(absoluteWorkdirInput)

  const environmentInput = {
    command: [
      "printf 'token=%s\\n' \"${PHASE6_ORACLE_TOKEN+present}\"",
      "printf 'safe=%s\\n' \"$PHASE6_ORACLE_SAFE\"",
      "printf 'ambient_dsh=%s\\n' \"${DSH_PHASE6_ORACLE_AMBIENT+present}\"",
      "printf 'managed=%s,%s,%s\\n' \"$DSH_SHELL\" \"$DSH_SESSION_ID\" \"$DSH_HOME\"",
      "printf 'terminal=%s,%s,%s,%s\\n' \"$NO_COLOR\" \"$TERM\" \"$PAGER\" \"$GIT_PAGER\"",
    ].join('; '),
    description: 'Inspect scrubbed shell environment',
  }
  const environment = await withOracleEnvironment(() => caller.call(environmentInput))

  const pathResolutionInput = {
    command: ':',
    description: 'Prove that the bare Bash executable is resolved through PATH',
  }
  const pathResolution = await withProcessEnvironment(
    'PATH',
    fixture.emptyPath,
    () => caller.call(pathResolutionInput),
  )

  const bashEnvHookPath = join(fixture.root, 'bash-env-hook.sh')
  const bashEnvHookSource = 'export PHASE6_ORACLE_BASH_ENV_RAN=hook-ran\n'
  writeFileSync(bashEnvHookPath, bashEnvHookSource, { encoding: 'utf8', mode: 0o600 })
  const bashEnvInput = {
    command: "printf 'bash_env=%s\\nargv0=%s\\n' \"$PHASE6_ORACLE_BASH_ENV_RAN\" \"$0\"",
    description: 'Prove the non-interactive Bash startup hook and argv zero',
  }
  const bashEnv = await withProcessEnvironment(
    'BASH_ENV',
    bashEnvHookPath,
    () => caller.call(bashEnvInput),
  )

  const successValue = foregroundValue(success, 'success')
  const silentValue = foregroundValue(silent, 'silent')
  const mixedValue = foregroundValue(mixed, 'mixed streams')
  const nonzeroValue = foregroundValue(nonzero, 'nonzero')
  const selfSignalValue = foregroundValue(selfSignal, 'self signal')
  const timeoutValue = foregroundValue(timeout, 'timeout')
  const bashEnvValue = foregroundValue(bashEnv, 'BASH_ENV startup hook')

  const checks = {
    successIsOrdinary: successValue.exitCode === 0
      && !successValue.timedOut
      && textOf(success) === 'hello\n',
    silentUsesMarker: silentValue.exitCode === 0 && textOf(silent) === '(no output)',
    stdoutPrecedesMarkedStderr: mixedValue.exitCode === 0
      && textOf(mixed) === 'out\n[stderr]\nerr\n',
    nonzeroIsOrdinary: !nonzero.isError
      && nonzeroValue.exitCode === 3
      && textOf(nonzero) === 'failing\n[exit code: 3]',
    selfSignalIsOrdinary: !selfSignal.isError
      && selfSignalValue.exitCode === null
      && selfSignalValue.signal === 'SIGTERM'
      && !selfSignalValue.timedOut
      && textOf(selfSignal) === '(no output)\n[killed by signal: SIGTERM]',
    timeoutIsOrdinaryAndDistinctFromAbort: !timeout.isError
      && timeoutValue.timedOut
      && !timeoutValue.aborted
      && timeoutValue.timeoutMs === 100
      && textOf(timeout).includes('[timed out after 100ms]'),
    defaultWorkdirUsesSession: textOf(defaultWorkdir) === `${fixture.workspace}\n`,
    relativeWorkdirUsesSessionBase: textOf(relativeWorkdir) === `${fixture.nested}\n`,
    absoluteWorkdirAccepted: textOf(absoluteWorkdir) === `${fixture.nested}\n`,
    credentialShapedAmbientRemoved: textOf(environment).includes('token=\n'),
    ordinaryAmbientRetained: textOf(environment).includes('safe=safe-value\n'),
    ambientDshRemoved: textOf(environment).includes('ambient_dsh=\n'),
    managedDshRebuilt: textOf(environment).includes(
      `managed=1,phase6-oracle-session,${fixture.workspace}/.dsh-home\n`,
    ),
    terminalOverridesApplied: textOf(environment).includes('terminal=1,dumb,cat,cat\n'),
    bareBashUsesEffectivePath: pathResolution.isError
      && pathResolution.error.message === 'spawn bash ENOENT'
      && textOf(pathResolution) === 'Error: spawn bash ENOENT',
    bashEnvHookExecuted: bashEnvValue.exitCode === 0
      && textOf(bashEnv).includes('bash_env=hook-ran\n'),
    argvZeroIsBareBash: textOf(bashEnv).includes('argv0=bash\n'),
  }
  assertNoFalseChecks(checks)

  return {
    success: {
      input: successInput,
      result: normalizeResult(success, fixture),
    },
    silent: {
      input: silentInput,
      result: normalizeResult(silent, fixture),
    },
    stdoutAndStderr: {
      input: mixedInput,
      result: normalizeResult(mixed, fixture),
    },
    nonzero: {
      input: nonzeroInput,
      result: normalizeResult(nonzero, fixture),
    },
    selfSignal: {
      input: selfSignalInput,
      result: normalizeResult(selfSignal, fixture),
    },
    timeout: {
      input: timeoutInput,
      result: normalizeTimeoutResult(timeout, fixture),
      normalization: 'exitCode, signal, and any corresponding final marker are platform facts',
    },
    workdir: {
      default: {
        input: defaultWorkdirInput,
        result: normalizeResult(defaultWorkdir, fixture),
      },
      relative: {
        input: relativeWorkdirInput,
        result: normalizeResult(relativeWorkdir, fixture),
      },
      absolute: {
        input: normalize(absoluteWorkdirInput, fixture),
        result: normalizeResult(absoluteWorkdir, fixture),
      },
    },
    environment: {
      input: environmentInput,
      result: normalizeResult(environment, fixture),
      fakeAmbientNames: ENV_KEYS,
    },
    executableAndStartup: {
      pathResolution: {
        environment: normalize({ PATH: fixture.emptyPath }, fixture),
        input: pathResolutionInput,
        result: normalizeResult(pathResolution, fixture),
      },
      bashEnvHook: {
        environment: normalize({ PATH: ORACLE_BASE_PATH, BASH_ENV: bashEnvHookPath }, fixture),
        hookSource: bashEnvHookSource,
        input: bashEnvInput,
        result: normalizeResult(bashEnv, fixture),
      },
    },
    checks,
    deterministicCallIds: caller.callIds(),
  }
}

async function lifecycleBoundaryScenarios(fixture: WorkspaceFixture): Promise<unknown> {
  const ctx = await boot(fixture, false, 75)
  const caller = makeCaller(ctx, fixture, 'phase6-lifecycle')
  let disposed = false
  try {
    const abortLeaderPath = join(fixture.root, 'caller-abort-leader.pid')
    const callerAbortInput = {
      command: [
        "trap '' TERM",
        `printf '%s\\n' \"$$\" > ${shellQuote(abortLeaderPath)}`,
        'while :; do sleep 60; done',
      ].join('; '),
      description: 'Observe the real caller-abort boundary',
      timeoutMs: 25_000,
    }
    const controller = new AbortController()
    const callerAbortPending = caller.call(callerAbortInput, controller.signal)
    const abortLeaderPid = await waitForPidFile(abortLeaderPath, 'caller-abort leader')
    const abortLeaderAliveBeforeAbort = processIsAlive(abortLeaderPid)
    requireCondition(abortLeaderAliveBeforeAbort, 'caller-abort leader must be alive before abort')
    controller.abort('phase6 oracle caller abort')
    const callerAbortResult = await callerAbortPending
    const abortLeaderAliveAtToolResult = processIsAlive(abortLeaderPid)

    const survivorPath = join(fixture.root, 'foreground-survivor.pid')
    const survivorScript = [
      "trap '' TERM",
      `printf '%s\\n' \"$$\" > ${shellQuote(survivorPath)}`,
      'sleep 60',
    ].join('; ')
    const directCompletionInput = {
      command: [
        `bash -c ${shellQuote(survivorScript)} >/dev/null 2>&1 &`,
        'disown;',
        "printf 'direct-complete\\n'",
      ].join(' '),
      description: 'Leave a service-owned same-group survivor after direct completion',
      timeoutMs: 25_000,
    }
    const directCompletionResult = await caller.call(directCompletionInput)
    const survivorPid = await waitForPidFile(survivorPath, 'foreground survivor')
    const survivorAliveAtToolResult = processIsAlive(survivorPid)

    await ctx.fiber.dispose()
    disposed = true
    const survivorAliveAfterServiceDispose = processIsAlive(survivorPid)

    const callerAbortChecks = {
      bodyStartedBeforeCallerAbort: abortLeaderAliveBeforeAbort,
      callerAbortCleanupReapedLeaderBeforeResult: !abortLeaderAliveAtToolResult,
      toolBoundaryReturnsGenericAbort: callerAbortResult.isError
        && callerAbortResult.error.info?.name === 'AbortError'
        && callerAbortResult.error.info?.code === TOOL_ABORTED
        && textOf(callerAbortResult) === 'Error: tool call aborted',
      internalShellResultNotExposed: !Object.hasOwn(callerAbortResult, 'value'),
      callerAbortReasonNotExposed: !JSON.stringify(callerAbortResult)
        .includes('phase6 oracle caller abort'),
    }
    const directCompletionChecks = {
      foregroundResultSettlesNormally: !directCompletionResult.isError
        && foregroundValue(directCompletionResult, 'direct completion').exitCode === 0
        && textOf(directCompletionResult) === 'direct-complete\n',
      sameGroupDescendantOutlivesForegroundResult: survivorAliveAtToolResult,
      subprocessServiceDisposeAwaitsWholeGroup: !survivorAliveAfterServiceDispose,
    }
    const checks = { callerAbort: callerAbortChecks, directCompletion: directCompletionChecks }
    assertNoFalseChecks(checks)

    return {
      callerAbortAtToolBoundary: {
        input: normalize(callerAbortInput, fixture),
        result: normalizeResult(callerAbortResult, fixture),
        observations: {
          leaderPidObservedButNotSerialized: true,
          leaderAliveBeforeCallerAbort: abortLeaderAliveBeforeAbort,
          leaderGoneWhenToolResultSettled: !abortLeaderAliveAtToolResult,
        },
      },
      foregroundDirectCompletionVsServiceCleanup: {
        input: normalize(directCompletionInput, fixture),
        result: normalizeResult(directCompletionResult, fixture),
        observations: {
          descendantPidObservedButNotSerialized: true,
          descendantAliveWhenForegroundResultSettled: survivorAliveAtToolResult,
          descendantGoneWhenSubprocessServiceDisposeSettled: !survivorAliveAfterServiceDispose,
        },
      },
      checks,
      deterministicCallIds: caller.callIds(),
    }
  } finally {
    if (!disposed) await ctx.fiber.dispose()
  }
}

function renderScenarios(ctx: Context): unknown {
  const definition = ctx.tools.get('bash')
  requireCondition(definition !== undefined, 'bash definition must exist for rendering')
  const render = (value: ShellRunResult): string => definition.output
    .render({}, { kind: 'foreground', ...value } as never)
    .filter((block): block is Extract<typeof block, { type: 'text' }> => block.type === 'text')
    .map(block => block.text)
    .join('')
  const base = {
    exitCode: 0,
    signal: null,
    timedOut: false,
    aborted: false,
    timeoutMs: 1_000,
    stdout: { text: '', truncated: false },
    stderr: { text: '', truncated: false },
  } satisfies ShellRunResult
  const cases = {
    silent: render(base),
    stdoutAndStderr: render({
      ...base,
      stdout: { text: 'out', truncated: false },
      stderr: { text: 'err', truncated: false },
    }),
    nonzero: render({ ...base, exitCode: 7 }),
    timeoutWithSignal: render({
      ...base,
      exitCode: null,
      signal: 'SIGTERM',
      timedOut: true,
      timeoutMs: 125,
    }),
    truncatedWithSpill: render({
      ...base,
      stdout: { text: 'tail', truncated: true, spillPath: '<spill>' },
    }),
    truncatedWithoutSpill: render({
      ...base,
      stderr: { text: 'tail', truncated: true },
    }),
  }
  const checks = {
    silentMarker: cases.silent === '(no output)',
    streamOrderAndSeparator: cases.stdoutAndStderr === 'out\n[stderr]\nerr',
    exitMarkerLast: cases.nonzero === '(no output)\n[exit code: 7]',
    timeoutBeforeSignal: cases.timeoutWithSignal
      === '(no output)\n[timed out after 125ms]\n[killed by signal: SIGTERM]',
    spillPathRendered: cases.truncatedWithSpill
      === 'tail\n[output truncated; full output: <spill>]',
    unavailableSpillRendered: cases.truncatedWithoutSpill
      === '[stderr]\ntail\n[output truncated; full output: (unavailable)]',
  }
  assertNoFalseChecks(checks)
  return { cases, checks }
}

function configurationFacts(ctx: Context): unknown {
  const config = (ctx.shell as LocalBashExecutor).config
  const shipped = readFileSync(join(process.cwd(), SHIPPED_BASE_CONFIG), 'utf8')
  const shippedBash = shipped.match(/    - id: bash-sandbox\n([\s\S]*?)(?=\n    - id:)/)?.[1]
  requireCondition(shippedBash !== undefined, 'shipped bash-sandbox section must exist')
  const shippedTimeout = Number(shippedBash.match(/timeoutMs:\s*(\d+)/)?.[1])
  requireCondition(Number.isFinite(shippedTimeout), 'shipped bash timeout must be numeric')
  requireCondition(shippedTimeout === 60_000, 'shipped base timeout must be 60000ms')
  requireCondition(
    shipped.includes("process.env.DSH_PERMISSION_MODE ?? 'workspace-write'"),
    'shipped base must default to workspace-write',
  )
  return {
    libraryExecutorDefaults: {
      timeoutMs: config.timeoutMs,
      maxTimeoutMs: config.maxTimeoutMs,
      maxOutputBytes: config.maxOutputBytes,
      maxSpillBytes: config.maxSpillBytes,
      graceMs: config.graceMs,
    },
    shippedBaseComposition: {
      timeoutMs: shippedTimeout,
      sandboxModeDefault: 'workspace-write',
      ordinaryCallApproval: 'no prompt unless another pre-rule asks or escalation is requested',
    },
    checks: {
      libraryTimeoutDefault120Seconds: config.timeoutMs === 120_000,
      libraryTimeoutCap600Seconds: config.maxTimeoutMs === 600_000,
      perStreamTail64000Bytes: config.maxOutputBytes === 64_000,
      perStreamSpill64MiB: config.maxSpillBytes === 64 * 1024 * 1024,
      libraryGraceThreeSeconds: config.graceMs === 3_000,
      shippedTimeoutOverride60Seconds: shippedTimeout === 60_000,
    },
  }
}

function assertNoFalseChecks(value: unknown, path = 'oracle'): void {
  if (value === false) throw new Error(`oracle check failed: ${path}`)
  if (Array.isArray(value) || value === null || typeof value !== 'object') return
  for (const [key, child] of Object.entries(value)) {
    assertNoFalseChecks(child, `${path}.${key}`)
  }
}

async function buildOracle(fixture: WorkspaceFixture): Promise<unknown> {
  const foregroundCtx = await boot(fixture, false, 75)
  const defaultCtx = await boot(fixture, undefined)
  try {
    const foregroundSchema = schemaSurface(foregroundCtx)
    const defaultSurface = defaultBackgroundSurface(defaultCtx)
    const scenarios = await withProcessEnvironment(
      'PATH',
      ORACLE_BASE_PATH,
      () => withProcessEnvironment(
        'BASH_ENV',
        undefined,
        () => processScenarios(foregroundCtx, fixture),
      ),
    )
    const rendering = renderScenarios(foregroundCtx)
    const config = configurationFacts(defaultCtx)
    const lifecycleBoundaries = await withProcessEnvironment(
      'PATH',
      ORACLE_BASE_PATH,
      () => withProcessEnvironment(
        'BASH_ENV',
        undefined,
        () => lifecycleBoundaryScenarios(fixture),
      ),
    )
    const checks = {
      foregroundSchema: (foregroundSchema as { checks: unknown }).checks,
      defaultSurface: (defaultSurface as { checks: unknown }).checks,
      scenarios: (scenarios as { checks: unknown }).checks,
      rendering: (rendering as { checks: unknown }).checks,
      configuration: (config as { checks: unknown }).checks,
      lifecycleBoundaries: (lifecycleBoundaries as { checks: unknown }).checks,
    }
    assertNoFalseChecks(checks)

    return {
      schemaVersion: 1,
      upstream: {
        repository: REPOSITORY,
        commit: BASELINE_COMMIT,
      },
      evidence: {
        sourcePaths: SOURCE_PATHS,
        testPaths: TEST_PATHS,
      },
      schemaSurface: {
        foregroundOnly: foregroundSchema,
        upstreamUnsandboxedPluginDefault: defaultSurface,
      },
      configuration: config,
      processScenarios: scenarios,
      lifecycleBoundaries,
      renderScenarios: rendering,
      deterministic: {
        freshTemporaryWorkspace: true,
        temporaryPathsNormalized: true,
        timeoutTerminationFactsNormalized: true,
        fixedCallIds: true,
        controlledExecutablePath: ORACLE_BASE_PATH,
        ambientBashEnvClearedOutsideExplicitHookScenario: true,
        lifecycleProcessIdsObservedButNotSerialized: true,
        realTimeoutGraceMs: 75,
      },
      safety: {
        networkAccess: 'none',
        credentialRequirement: 'none',
        environmentProbe: 'only explicit non-secret markers are rendered; fake credential values are asserted absent',
        filesystemWrites: 'fresh platform temporary directory and explicit output path only',
      },
    }
  } finally {
    await defaultCtx.fiber.dispose()
    await foregroundCtx.fiber.dispose()
  }
}

function assertPinnedCleanUpstream(): void {
  const actualCommit = execFileSync('git', ['rev-parse', 'HEAD'], {
    cwd: process.cwd(),
    encoding: 'utf8',
  }).trim()
  if (actualCommit !== BASELINE_COMMIT) {
    throw new Error(`oracle requires upstream ${BASELINE_COMMIT}, got ${actualCommit}`)
  }
  const workingTreeChanges = execFileSync('git', ['status', '--porcelain'], {
    cwd: process.cwd(),
    encoding: 'utf8',
  }).trim()
  if (workingTreeChanges !== '') throw new Error('oracle requires a clean upstream working tree')
}

async function main(): Promise<void> {
  assertPinnedCleanUpstream()
  const fixture = await createWorkspace()
  try {
    const output = await buildOracle(fixture)
    assertPinnedCleanUpstream()
    const serialized = `${JSON.stringify(output, null, 2)}\n`
    requireCondition(!serialized.includes(fixture.root), 'temporary root leaked into fixture')
    requireCondition(!serialized.includes('obviously-fake-token'), 'fake token value leaked')
    const outputPath = process.argv[2]
    if (outputPath === undefined) process.stdout.write(serialized)
    else writeFileSync(outputPath, serialized, 'utf8')
  } finally {
    await rm(fixture.root, { recursive: true, force: true })
  }
}

void main().catch((error: unknown) => {
  process.stderr.write(`${error instanceof Error ? error.stack ?? error.message : String(error)}\n`)
  process.exitCode = 1
})
