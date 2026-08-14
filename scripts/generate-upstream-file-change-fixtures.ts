/**
 * Deterministic Phase 5 file-change and approval oracle for DeepSeek Harness.
 *
 * Run from the pinned upstream checkout with its locked tsx binary. The script
 * writes only to a fresh temporary tree and the optional output path.
 */

import { execFileSync, spawnSync } from 'node:child_process'
import { existsSync, writeFileSync } from 'node:fs'
import {
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { Context } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import type { Agent } from '@deepseek-ai/dsh-agent'
import AgentLoop from '@deepseek-ai/dsh-agent-loop'
import { SandboxedFileSystem } from '@deepseek-ai/dsh-fs-sandbox'
import * as FsPolicy from '@deepseek-ai/dsh-fs-observation-policy'
import InvariantRegistry from '@deepseek-ai/dsh-invariants'
import LlmRuntime, {
  CallId,
  LlmAdapter,
  MessageId,
  freezeMessage,
} from '@deepseek-ai/dsh-llm'
import type {
  GenerateOptions,
  LlmResolvedModelInfo,
  StreamChunk,
} from '@deepseek-ai/dsh-llm'
import SandboxPolicyService from '@deepseek-ai/dsh-sandbox-policy'
import SessionStore, { SessionId } from '@deepseek-ai/dsh-session'
import type { Session, UserMessage } from '@deepseek-ai/dsh-session'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import * as ToolFs from '@deepseek-ai/dsh-tool-fs'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import type {
  PreToolDecision,
  ToolExecutionResult,
  ToolExecutionSuccess,
} from '@deepseek-ai/dsh-tools'
import ApprovalService from '@deepseek-ai/dsh-user-approval'
import type {
  ApprovalOutcome,
  ApprovalRequest,
} from '@deepseek-ai/dsh-user-approval'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'
const REPOSITORY = 'https://github.com/deepseek-ai/deepseek-harness'
const CLOCK_START_MS = 1_700_500_000_000
const CLOCK_STEP_MS = 11

const SOURCE_PATHS = [
  'packages/core/agent-loop/src/tool-calls.ts',
  'packages/core/tools/src/index.ts',
  'packages/core/tools/src/invariant.ts',
  'packages/fs/fs/src/index.ts',
  'packages/fs/fs/src/types.ts',
  'packages/fs/fs-local/src/index.ts',
  'packages/fs/fs-local/src/fsio.ts',
  'packages/fs/fs-observation-policy/src/index.ts',
  'packages/fs/fs-sandbox/src/index.ts',
  'packages/fs/tool-fs/src/diff.ts',
  'packages/fs/tool-fs/src/edit.ts',
  'packages/fs/tool-fs/src/index.ts',
  'packages/fs/tool-fs/src/read.ts',
  'packages/fs/tool-fs/src/sandbox.ts',
  'packages/fs/tool-fs/src/write.ts',
  'packages/interaction/user-approval/src/index.ts',
  'packages/interaction/user-approval/src/invariant.ts',
  'packages/interaction/user-approval/src/types.ts',
  'packages/sandbox/sandbox-policy/src/index.ts',
] as const

const TEST_PATHS = [
  'packages/core/agent-loop/tests/interception.spec.ts',
  'packages/core/agent-loop/tests/tool-calls.spec.ts',
  'packages/core/tools/tests/tools.spec.ts',
  'packages/fs/fs-local/tests/filesystem.spec.ts',
  'packages/fs/fs-local/tests/fsio.spec.ts',
  'packages/fs/fs-observation-policy/tests/policy.spec.ts',
  'packages/fs/fs-sandbox/tests/fs-sandbox.spec.ts',
  'packages/fs/tool-fs/tests/diff.spec.ts',
  'packages/fs/tool-fs/tests/integration.spec.ts',
  'packages/fs/tool-fs/tests/tools.spec.ts',
  'packages/interaction/user-approval/tests/approval.spec.ts',
  'packages/interaction/user-approval/tests/invariant.spec.ts',
] as const

interface WorkspaceFixture {
  root: string
  workspace: string
}

interface ToolCaller {
  call(name: string, args: unknown, session?: object): Promise<ToolExecutionResult>
  callIds(): string[]
}

type ApprovalScenarioKind = 'default-allow' | 'deny' | 'ask-allowed' | 'ask-rejected'

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

function normalizePaths(value: unknown, fixture: WorkspaceFixture): unknown {
  if (typeof value === 'string') {
    return replaceEvery(replaceEvery(value, fixture.workspace, '<workspace>'), fixture.root, '<fixture-root>')
  }
  if (Array.isArray(value)) return value.map(item => normalizePaths(item, fixture))
  if (value === null || typeof value !== 'object') return value
  return Object.fromEntries(
    Object.entries(value).map(([key, child]) => [key, normalizePaths(child, fixture)]),
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
  return normalizePaths(durableShape, fixture)
}

function errorCode(result: ToolExecutionResult): string | undefined {
  return result.isError && typeof result.error.info?.code === 'string'
    ? result.error.info.code
    : undefined
}

function makeCaller(ctx: Context, fixture: WorkspaceFixture): ToolCaller {
  let ordinal = 0
  const ids: string[] = []
  return {
    async call(name: string, args: unknown, suppliedSession?: object): Promise<ToolExecutionResult> {
      ordinal += 1
      const id = `phase5-direct-${String(ordinal).padStart(2, '0')}`
      ids.push(id)
      const session = suppliedSession ?? {
        header: { id: 'phase5-direct-session', cwd: fixture.workspace },
        events: [],
      }
      return ctx.tools.execute({
        signal: new AbortController().signal,
        callId: CallId(id),
        name,
        arguments: args,
        agent: { id: 'phase5-direct-agent', session } as never,
      })
    },
    callIds(): string[] {
      return [...ids]
    },
  }
}

async function createWorkspace(prefix: string): Promise<WorkspaceFixture> {
  const canonicalTmp = await realpath(tmpdir())
  const root = await mkdtemp(join(canonicalTmp, `${prefix}-`))
  const workspace = join(root, 'workspace')
  await mkdir(workspace)
  return { root, workspace }
}

async function bootTools(fixture: WorkspaceFixture): Promise<Context> {
  const ctx = new Context()
  await ctx.plugin(SystemPrompt, { persona: 'Phase 5 oracle persona.' })
  await ctx.plugin(ToolRuntime)
  await ctx.plugin(SandboxPolicyService, {
    mode: 'workspace-write',
    workspaceRoot: fixture.workspace,
  })
  await ctx.plugin(SandboxedFileSystem, { cwd: fixture.workspace })
  await ctx.plugin(FsPolicy)
  await ctx.plugin(ToolFs)
  return ctx
}

function modelFacingSurface(ctx: Context): unknown {
  const schemas = ctx.tools.schemas()
  const registeredNames = schemas.map(schema => schema.name).sort()
  const applyPatchSearch = spawnSync(
    'git',
    ['grep', '-n', '-E', "name:[[:space:]]*['\\\"]apply_patch['\\\"]", '--', 'apps', 'packages'],
    { cwd: process.cwd(), encoding: 'utf8' },
  )
  requireCondition(
    applyPatchSearch.status === 0 || applyPatchSearch.status === 1,
    `git grep for apply_patch failed: ${applyPatchSearch.stderr}`,
  )
  const repoDefinitions = applyPatchSearch.status === 0
    ? applyPatchSearch.stdout.trim().split('\n').filter(Boolean)
    : []
  const write = schemas.find(schema => schema.name === 'write')
  const edit = schemas.find(schema => schema.name === 'edit')
  requireCondition(write !== undefined, 'write schema must exist')
  requireCondition(edit !== undefined, 'edit schema must exist')
  return {
    registeredNames,
    applyPatchPresent: registeredNames.includes('apply_patch'),
    repositoryApplyPatchToolDefinitions: repoDefinitions,
    write: structuredClone(write),
    edit: structuredClone(edit),
    checks: {
      canonicalFsToolsPresent: ['edit', 'read', 'write'].every(name => registeredNames.includes(name)),
      noApplyPatchSchema: !registeredNames.includes('apply_patch'),
      noTrackedApplyPatchToolDefinition: repoDefinitions.length === 0,
    },
  }
}

async function canonicalMutations(ctx: Context, fixture: WorkspaceFixture): Promise<unknown> {
  const caller = makeCaller(ctx, fixture)

  const createInput = { file_path: 'created.txt', content: 'line one\nline two\n' }
  const createResult = await caller.call('write', createInput)
  const createSuccess = expectSuccess(createResult, 'write create')
  const createDisk = await readFile(join(fixture.workspace, 'created.txt'), 'utf8')

  const updateSession = { header: { id: 'update-session', cwd: fixture.workspace }, events: [] }
  await writeFile(join(fixture.workspace, 'updated.txt'), 'alpha\nbeta\n')
  const updateRead = await caller.call('read', { file_path: 'updated.txt' }, updateSession)
  const updateInput = { file_path: 'updated.txt', content: 'alpha\nBETA\n' }
  const updateResult = await caller.call('write', updateInput, updateSession)
  const updateSuccess = expectSuccess(updateResult, 'read then write update')
  const updateDisk = await readFile(join(fixture.workspace, 'updated.txt'), 'utf8')

  const uniqueSession = { header: { id: 'unique-session', cwd: fixture.workspace }, events: [] }
  await writeFile(join(fixture.workspace, 'unique.txt'), 'hello world\n')
  const uniqueRead = await caller.call('read', { file_path: 'unique.txt' }, uniqueSession)
  const uniqueInput = {
    file_path: 'unique.txt',
    old_string: 'world',
    new_string: 'there',
  }
  const uniqueResult = await caller.call('edit', uniqueInput, uniqueSession)
  const uniqueSuccess = expectSuccess(uniqueResult, 'unique edit')
  const uniqueDisk = await readFile(join(fixture.workspace, 'unique.txt'), 'utf8')

  const replaceAllSession = { header: { id: 'replace-all-session', cwd: fixture.workspace }, events: [] }
  const replaceAllBefore = Array.from(
    { length: 20 },
    (_, index) => index === 2 || index === 15 ? `needle at line ${index + 1}` : `line ${index + 1}`,
  ).join('\n') + '\n'
  await writeFile(join(fixture.workspace, 'replace-all.txt'), replaceAllBefore)
  const replaceAllRead = await caller.call('read', { file_path: 'replace-all.txt' }, replaceAllSession)
  const replaceAllInput = {
    file_path: 'replace-all.txt',
    old_string: 'needle',
    new_string: 'TOKEN',
    replace_all: true,
  }
  const replaceAllResult = await caller.call('edit', replaceAllInput, replaceAllSession)
  const replaceAllSuccess = expectSuccess(replaceAllResult, 'replace_all edit')
  const replaceAllDisk = await readFile(join(fixture.workspace, 'replace-all.txt'), 'utf8')
  const replaceAllDiffs = (
    replaceAllSuccess.meta as { diffs?: unknown[] } | undefined
  )?.diffs

  return {
    writeCreate: {
      input: createInput,
      result: normalizeResult(createResult, fixture),
      diskAfter: createDisk,
      checks: {
        operationCreate: (createSuccess.value as { operation?: unknown }).operation === 'create',
        exactBytes: createDisk === createInput.content,
      },
    },
    readThenWriteUpdate: {
      initial: 'alpha\nbeta\n',
      read: normalizeResult(updateRead, fixture),
      input: updateInput,
      result: normalizeResult(updateResult, fixture),
      diskAfter: updateDisk,
      checks: {
        readSucceeded: !updateRead.isError,
        operationUpdate: (updateSuccess.value as { operation?: unknown }).operation === 'update',
        beforeCaptured: (updateSuccess.value as { before?: unknown }).before === 'alpha\nbeta\n',
        exactBytes: updateDisk === updateInput.content,
      },
    },
    uniqueEdit: {
      initial: 'hello world\n',
      read: normalizeResult(uniqueRead, fixture),
      input: uniqueInput,
      result: normalizeResult(uniqueResult, fixture),
      diskAfter: uniqueDisk,
      checks: {
        readSucceeded: !uniqueRead.isError,
        beforeCaptured: (uniqueSuccess.value as { before?: unknown }).before === 'hello world\n',
        afterCaptured: (uniqueSuccess.value as { after?: unknown }).after === 'hello there\n',
        exactBytes: uniqueDisk === 'hello there\n',
      },
    },
    replaceAllDiff: {
      initial: replaceAllBefore,
      read: normalizeResult(replaceAllRead, fixture),
      input: replaceAllInput,
      result: normalizeResult(replaceAllResult, fixture),
      diskAfter: replaceAllDisk,
      checks: {
        readSucceeded: !replaceAllRead.isError,
        bothOccurrencesReplaced: !replaceAllDisk.includes('needle')
          && replaceAllDisk.split('TOKEN').length - 1 === 2,
        twoContextualHunks: Array.isArray(replaceAllDiffs) && replaceAllDiffs.length === 2,
        diffPathsUseInputSpelling: Array.isArray(replaceAllDiffs)
          && replaceAllDiffs.every((diff) =>
            typeof diff === 'object' && diff !== null && (diff as { path?: unknown }).path === 'replace-all.txt'),
      },
    },
    deterministicCallIds: caller.callIds(),
  }
}

async function observationFailures(ctx: Context, fixture: WorkspaceFixture): Promise<unknown> {
  const caller = makeCaller(ctx, fixture)

  await writeFile(join(fixture.workspace, 'unobserved-write.txt'), 'keep write\n')
  const unobservedWrite = await caller.call('write', {
    file_path: 'unobserved-write.txt',
    content: 'must not land\n',
  }, { header: { id: 'unobserved-write-session', cwd: fixture.workspace }, events: [] })
  const unobservedWriteDisk = await readFile(join(fixture.workspace, 'unobserved-write.txt'), 'utf8')

  await writeFile(join(fixture.workspace, 'unobserved-edit.txt'), 'keep edit\n')
  const unobservedEdit = await caller.call('edit', {
    file_path: 'unobserved-edit.txt',
    old_string: 'edit',
    new_string: 'changed',
  }, { header: { id: 'unobserved-edit-session', cwd: fixture.workspace }, events: [] })
  const unobservedEditDisk = await readFile(join(fixture.workspace, 'unobserved-edit.txt'), 'utf8')

  const staleWriteSession = { header: { id: 'stale-write-session', cwd: fixture.workspace }, events: [] }
  await writeFile(join(fixture.workspace, 'stale-write.txt'), 'observed v1\n')
  const staleWriteRead = await caller.call('read', { file_path: 'stale-write.txt' }, staleWriteSession)
  await writeFile(join(fixture.workspace, 'stale-write.txt'), 'external v2\n')
  const staleWrite = await caller.call('write', {
    file_path: 'stale-write.txt',
    content: 'must not replace external\n',
  }, staleWriteSession)
  const staleWriteDisk = await readFile(join(fixture.workspace, 'stale-write.txt'), 'utf8')

  const staleEditSession = { header: { id: 'stale-edit-session', cwd: fixture.workspace }, events: [] }
  await writeFile(join(fixture.workspace, 'stale-edit.txt'), 'hello world\n')
  const staleEditRead = await caller.call('read', { file_path: 'stale-edit.txt' }, staleEditSession)
  await writeFile(join(fixture.workspace, 'stale-edit.txt'), 'external content\n')
  const staleEdit = await caller.call('edit', {
    file_path: 'stale-edit.txt',
    old_string: 'world',
    new_string: 'there',
  }, staleEditSession)
  const staleEditDisk = await readFile(join(fixture.workspace, 'stale-edit.txt'), 'utf8')

  return {
    unobservedWrite: {
      result: normalizeResult(unobservedWrite, fixture),
      diskAfter: unobservedWriteDisk,
      checks: {
        code: errorCode(unobservedWrite) === 'FS_NOT_OBSERVED',
        untouched: unobservedWriteDisk === 'keep write\n',
      },
    },
    unobservedEdit: {
      result: normalizeResult(unobservedEdit, fixture),
      diskAfter: unobservedEditDisk,
      checks: {
        code: errorCode(unobservedEdit) === 'FS_NOT_OBSERVED',
        untouched: unobservedEditDisk === 'keep edit\n',
      },
    },
    staleWrite: {
      read: normalizeResult(staleWriteRead, fixture),
      result: normalizeResult(staleWrite, fixture),
      diskAfter: staleWriteDisk,
      checks: {
        readSucceeded: !staleWriteRead.isError,
        code: errorCode(staleWrite) === 'FS_STALE_VERSION',
        externalContentPreserved: staleWriteDisk === 'external v2\n',
      },
    },
    staleEdit: {
      read: normalizeResult(staleEditRead, fixture),
      result: normalizeResult(staleEdit, fixture),
      diskAfter: staleEditDisk,
      checks: {
        readSucceeded: !staleEditRead.isError,
        staleWinsBeforeLiteralMatch: errorCode(staleEdit) === 'FS_STALE_VERSION',
        externalContentPreserved: staleEditDisk === 'external content\n',
      },
    },
    deterministicCallIds: caller.callIds(),
  }
}

async function windowedObservation(ctx: Context, fixture: WorkspaceFixture): Promise<unknown> {
  const caller = makeCaller(ctx, fixture)
  const session = { header: { id: 'window-session', cwd: fixture.workspace }, events: [] }
  const before = Array.from({ length: 20 }, (_, index) => `line ${index + 1}`).join('\n')
  await writeFile(join(fixture.workspace, 'window.txt'), before)
  const read = await caller.call('read', {
    file_path: 'window.txt',
    offset: 1,
    limit: 1,
  }, session)
  const edit = await caller.call('edit', {
    file_path: 'window.txt',
    old_string: 'line 12',
    new_string: 'LINE 12',
  }, session)
  const diskAfter = await readFile(join(fixture.workspace, 'window.txt'), 'utf8')
  return {
    readWindow: { offset: 1, limit: 1 },
    read: normalizeResult(read, fixture),
    editOutsideWindow: normalizeResult(edit, fixture),
    diskAfter,
    checks: {
      oneLineReadSucceeded: !read.isError
        && (read.value as { lines?: unknown[] }).lines?.length === 1,
      editOutsideWindowAuthorized: !edit.isError,
      wholeFileVersionNotWindowCoverage: diskAfter.includes('LINE 12'),
    },
  }
}

async function lastWindowOverwrite(fixture: WorkspaceFixture): Promise<unknown> {
  const targetPath = join(fixture.workspace, 'last-window.txt')
  await writeFile(targetPath, 'baseline')
  const ctx = new Context()
  await ctx.plugin(SandboxPolicyService, {
    mode: 'workspace-write',
    workspaceRoot: fixture.workspace,
  })
  await ctx.plugin(SandboxedFileSystem, { cwd: fixture.workspace })
  const fs = ctx.fs as SandboxedFileSystem
  const target = await fs.resolve('last-window.txt')
  const observed = await fs.stat(target)
  requireCondition(observed !== undefined, 'last-window target must exist')
  let competitorInjected = false
  fs.internals.inspectTemp = async () => {
    competitorInjected = true
    await writeFile(targetPath, 'external-in-last-window')
  }
  try {
    const outcome = await fs.writeText(target, 'official-final-write', {
      kind: 'replaceIfVersion',
      version: observed.version,
    })
    const diskAfter = await readFile(targetPath, 'utf8')
    return {
      observedBefore: 'baseline',
      externalWriteInjectedAfterStaging: 'external-in-last-window',
      requested: 'official-final-write',
      outcome: {
        operation: outcome.operation,
        before: outcome.before,
        after: outcome.after,
      },
      diskAfter,
      checks: {
        competitorInjected,
        guardedCallStillSucceeded: outcome.operation === 'update',
        finalRenameOverwroteLastWindowCompetitor: diskAfter === 'official-final-write',
      },
    }
  } finally {
    await ctx.fiber.dispose()
  }
}

function fixedUser(id: string, text: string): UserMessage {
  return freezeMessage({
    id: MessageId(id),
    role: 'user',
    content: [{ type: 'text', text }],
    source: { kind: 'user' },
  })
}

function textResponse(text: string): StreamChunk[] {
  return [
    { type: 'block-start', index: 0, blockType: 'text' },
    { type: 'text-delta', index: 0, text },
    { type: 'block-end', index: 0, block: { type: 'text', text } },
    { type: 'usage', usage: { inputTokens: 3, outputTokens: 2 } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
}

function toolResponse(callId: string, name: string, argumentsJson: string): StreamChunk[] {
  return [
    { type: 'block-start', index: 0, blockType: 'tool-call' },
    {
      type: 'tool-call-delta',
      index: 0,
      id: CallId(callId),
      name,
      argumentsDelta: argumentsJson,
    },
    {
      type: 'block-end',
      index: 0,
      block: {
        type: 'tool-call',
        id: CallId(callId),
        name,
        arguments: argumentsJson,
      },
    },
    { type: 'usage', usage: { inputTokens: 5, outputTokens: 3 } },
    { type: 'finish', reason: { kind: 'tool-calls' } },
  ]
}

class ScriptedAdapter extends LlmAdapter {
  private cursor = 0

  constructor(private readonly script: readonly (readonly StreamChunk[])[]) {
    super()
  }

  override resolveModel(provider: string, model: string): Promise<LlmResolvedModelInfo> {
    return Promise.resolve({
      provider,
      id: model,
      name: model,
      context: { contextWindow: 4_096 },
    })
  }

  async * stream(_options: GenerateOptions): AsyncIterable<StreamChunk> {
    const entry = this.script[this.cursor]
    if (entry === undefined) throw new Error(`approval oracle script exhausted at ${this.cursor}`)
    this.cursor += 1
    for (const chunk of entry) yield chunk
  }

  get dispatchCount(): number {
    return this.cursor
  }
}

function normalizeSessionEvents(events: Session['events'], fixture: WorkspaceFixture): unknown[] {
  const approvalIds = new Map<string, string>()
  let approvalOrdinal = 0
  const normalized = structuredClone(events).map((event) => {
    if ((event.type === 'approval/asked' || event.type === 'approval/decided')
      && typeof event.data.id === 'string') {
      let id = approvalIds.get(event.data.id)
      if (id === undefined) {
        approvalOrdinal += 1
        id = `approval-${approvalOrdinal}`
        approvalIds.set(event.data.id, id)
      }
      event.data.id = id as typeof event.data.id
    }
    event.time = CLOCK_START_MS + event.seq * CLOCK_STEP_MS
    return event
  })
  return normalizePaths(normalized, fixture) as unknown[]
}

async function approvalScenario(kind: ApprovalScenarioKind): Promise<unknown> {
  const fixture = await createWorkspace(`dsh-phase5-${kind}`)
  const writePath = join(fixture.workspace, 'decision.txt')
  const trace: string[] = []
  let answererCalls = 0
  const ctx = new Context()
  try {
    await ctx.plugin(InvariantRegistry)
    await ctx.plugin(LlmRuntime)
    await ctx.plugin(SessionStore)
    await ctx.plugin(SystemPrompt, { persona: 'Phase 5 approval oracle.' })
    await ctx.plugin(ToolRuntime)
    await ctx.plugin(AgentRegistry)
    await ctx.plugin(AgentLoop, { agents: [], maxParallelToolCalls: 1 })
    await ctx.plugin(SandboxPolicyService, {
      mode: 'workspace-write',
      workspaceRoot: fixture.workspace,
    })
    await ctx.plugin(SandboxedFileSystem, { cwd: fixture.workspace })
    await ctx.plugin(FsPolicy)
    await ctx.plugin(ApprovalService, { policy: 'ask' })
    await ctx.plugin(ToolFs)

    ctx.on('session/event', (_session, event) => {
      if (event.type === 'tool/call'
        || event.type === 'approval/asked'
        || event.type === 'approval/decided'
        || event.type === 'tool/result') {
        trace.push(`session:${event.type}`)
      }
    })
    ;(ctx.fs as SandboxedFileSystem).internals.inspectTemp = () => {
      trace.push('filesystem:staged')
    }

    if (kind === 'deny') {
      ctx.on('tools/pre-execute', async (exec, next): Promise<PreToolDecision> => {
        if (exec.name !== 'write') return next()
        trace.push('gate:deny')
        return { kind: 'deny', reason: 'phase5 oracle deny' }
      })
    } else if (kind === 'ask-allowed' || kind === 'ask-rejected') {
      ctx.on('tools/pre-execute', async (exec, next): Promise<PreToolDecision> => {
        if (exec.name !== 'write') return next()
        trace.push('gate:ask')
        return { kind: 'ask', reason: 'phase5 oracle asks' }
      })
      const answer = kind === 'ask-allowed' ? 'allowed-once' : 'rejected'
      ctx.on('approval/request', (request: ApprovalRequest) => {
        answererCalls += 1
        trace.push(`answerer:${answer}`)
        requireCondition(request.toolName === 'write', 'approval request must name write')
        requireCondition(request.callId === CallId(`call-${kind}`), 'approval request must cite call id')
        return Promise.resolve<ApprovalOutcome>(answer)
      })
    }

    const argumentsJson = JSON.stringify({
      file_path: 'decision.txt',
      content: `${kind}\n`,
    })
    const adapter = new ScriptedAdapter([
      toolResponse(`call-${kind}`, 'write', argumentsJson),
      textResponse(`finished ${kind}`),
    ])
    ctx.llm.registerAdapter(['mock'], adapter)
    const agent: Agent = ctx.agentLoop.create(SessionId(`phase5-${kind}`), {
      provider: 'mock',
      model: 'oracle-model',
      maxTokens: 1_024,
    }, { cwd: fixture.workspace })
    agent.followup(fixedUser(`user-${kind}`, `exercise ${kind}`))
    await agent.whenIdle()

    const diskAfter = existsSync(writePath) ? await readFile(writePath, 'utf8') : null
    const events = normalizeSessionEvents(agent.session.events, fixture)
    const eventTypes = agent.session.events.map(event => event.type)
    const relevantEventTypes = eventTypes.filter(type =>
      type === 'tool/call'
      || type === 'approval/asked'
      || type === 'approval/decided'
      || type === 'tool/result')
    const resultEvent = agent.session.events.find(event => event.type === 'tool/result')
    const resultIsError = resultEvent?.type === 'tool/result'
      ? resultEvent.data.message.content[0].isError === true
      : undefined
    const shouldWrite = kind === 'default-allow' || kind === 'ask-allowed'
    const shouldAsk = kind === 'ask-allowed' || kind === 'ask-rejected'
    const expectedRelevant = shouldAsk
      ? ['tool/call', 'approval/asked', 'approval/decided', 'tool/result']
      : ['tool/call', 'tool/result']
    const expectedTrace = kind === 'default-allow'
      ? ['session:tool/call', 'filesystem:staged', 'session:tool/result']
      : kind === 'deny'
        ? ['session:tool/call', 'gate:deny', 'session:tool/result']
        : kind === 'ask-allowed'
          ? [
              'session:tool/call',
              'gate:ask',
              'session:approval/asked',
              'answerer:allowed-once',
              'session:approval/decided',
              'filesystem:staged',
              'session:tool/result',
            ]
          : [
              'session:tool/call',
              'gate:ask',
              'session:approval/asked',
              'answerer:rejected',
              'session:approval/decided',
              'session:tool/result',
            ]

    return {
      decision: kind,
      answererCalls,
      trace,
      relevantEventTypes,
      events,
      diskAfter,
      resultIsError,
      dispatchCount: adapter.dispatchCount,
      checks: {
        exactRelevantEventOrder: JSON.stringify(relevantEventTypes) === JSON.stringify(expectedRelevant),
        exactLiveTrace: JSON.stringify(trace) === JSON.stringify(expectedTrace),
        defaultWorkspaceWriteDoesNotAsk: kind !== 'default-allow'
          || (answererCalls === 0 && !eventTypes.includes('approval/asked')),
        answererCalledOnlyForAsk: answererCalls === (shouldAsk ? 1 : 0),
        sideEffectMatchesDecision: shouldWrite
          ? diskAfter === `${kind}\n`
          : diskAfter === null,
        resultMatchesDecision: resultIsError === !shouldWrite,
        continuedToSecondModelStep: adapter.dispatchCount === 2,
      },
    }
  } finally {
    await ctx.fiber.dispose()
    await rm(fixture.root, { recursive: true, force: true })
  }
}

function assertNoFalseChecks(value: unknown, path = 'oracle'): void {
  if (value === false) throw new Error(`oracle check failed: ${path}`)
  if (Array.isArray(value) || value === null || typeof value !== 'object') return
  for (const [key, child] of Object.entries(value)) {
    assertNoFalseChecks(child, `${path}.${key}`)
  }
}

function assertEveryNamedCheckGroup(value: unknown, path = 'oracle'): void {
  if (Array.isArray(value) || value === null || typeof value !== 'object') return
  for (const [key, child] of Object.entries(value)) {
    if (key === 'checks') assertNoFalseChecks(child, `${path}.checks`)
    else assertEveryNamedCheckGroup(child, `${path}.${key}`)
  }
}

function installDeterminism(): void {
  let clockCounter = 0
  let uuidCounter = 0
  Object.defineProperty(Date, 'now', {
    configurable: true,
    value: () => CLOCK_START_MS + clockCounter++ * CLOCK_STEP_MS,
  })
  Object.defineProperty(globalThis.crypto, 'randomUUID', {
    configurable: true,
    value: () => {
      uuidCounter += 1
      return `00000000-0000-4000-8000-${String(uuidCounter).padStart(12, '0')}`
    },
  })
}

async function buildOracle(fixture: WorkspaceFixture): Promise<unknown> {
  const ctx = await bootTools(fixture)
  try {
    const surface = modelFacingSurface(ctx)
    const mutations = await canonicalMutations(ctx, fixture)
    const failures = await observationFailures(ctx, fixture)
    const windowed = await windowedObservation(ctx, fixture)
    const race = await lastWindowOverwrite(fixture)
    const approval = {
      defaultAllow: await approvalScenario('default-allow'),
      deny: await approvalScenario('deny'),
      askAllowed: await approvalScenario('ask-allowed'),
      askRejected: await approvalScenario('ask-rejected'),
    }
    assertEveryNamedCheckGroup({ surface, mutations, failures, windowed, race, approval })

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
      toolSurface: surface,
      canonicalMutations: mutations,
      observationFailures: failures,
      windowedObservation: windowed,
      lastWindowOverwrite: race,
      approvalPipeline: approval,
      deterministic: {
        freshTemporaryWorkspaces: true,
        temporaryPathsNormalized: true,
        eventTimes: `normalized as ${CLOCK_START_MS} + seq * ${CLOCK_STEP_MS}`,
        approvalIds: 'normalized by first appearance within each scenario',
      },
      safety: {
        networkAccess: 'none',
        credentialAccess: 'none',
        filesystemWrites: 'fresh directories below the platform temporary directory only',
      },
    }
  } finally {
    await ctx.fiber.dispose()
  }
}

async function main(): Promise<void> {
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

  installDeterminism()
  const fixture = await createWorkspace('dsh-phase5-oracle')
  try {
    const output = await buildOracle(fixture)
    const serialized = `${JSON.stringify(output, null, 2)}\n`
    requireCondition(!serialized.includes(fixture.root), 'temporary root leaked into fixture')
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
