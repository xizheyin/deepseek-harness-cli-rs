/**
 * Deterministic Phase 3 agent-loop oracle for DeepSeek Harness.
 * Run from the pinned upstream checkout with its locked tsx binary.
 */

import { execFileSync } from 'node:child_process'
import { writeFileSync } from 'node:fs'
import { Context } from '@deepseek-ai/cordis'
import AgentRegistry from '@deepseek-ai/dsh-agent'
import type { Agent } from '@deepseek-ai/dsh-agent'
import * as AgentInvariant from '@deepseek-ai/dsh-agent/invariant'
import AgentLoop from '@deepseek-ai/dsh-agent-loop'
import * as AgentLoopInvariant from '@deepseek-ai/dsh-agent-loop/invariant'
import InvariantRegistry from '@deepseek-ai/dsh-invariants'
import LlmRuntime, {
  CallId,
  LlmAdapter,
  MessageId,
  freezeMessage,
  resolveRetryPolicy,
} from '@deepseek-ai/dsh-llm'
import type {
  GenerateOptions,
  LlmResolvedModelInfo,
  Message,
  ResolvedRetryPolicy,
  StreamChunk,
} from '@deepseek-ai/dsh-llm'
import * as LlmInvariant from '@deepseek-ai/dsh-llm/invariant'
import * as LlmRetry from '@deepseek-ai/dsh-llm-retry'
import * as LlmRetryInvariant from '@deepseek-ai/dsh-llm-retry/invariant'
import SessionStore, {
  SessionId,
  foldRequestHeader,
} from '@deepseek-ai/dsh-session'
import type {
  EpochHeader,
  RequestContext,
  Session,
  UserMessage,
} from '@deepseek-ai/dsh-session'
import * as SessionInvariant from '@deepseek-ai/dsh-session/invariant'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ToolRuntime, { defineContentToolFixture } from '@deepseek-ai/dsh-tools'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'
const CLOCK_START_MS = 1_700_000_000_000
const CLOCK_STEP_MS = 7

interface NormalizedRequest {
  provider: string
  model: string
  reasoningEffort?: string
  temperature?: number
  maxTokens?: number
  stop?: string[]
  system?: string
  tools?: GenerateOptions['tools']
  messages: Message[]
  sessionId?: string
  frozen: { request: boolean; messages: boolean }
}

interface DispatchSnapshot {
  ordinal: number
  eventCount: number
  lastSeq: number
  eventTypes: string[]
  foldedHeader?: EpochHeader
  requestContext?: RequestContext
  derivedMessages: Message[]
  request: NormalizedRequest
  checks: {
    messagesEqualDerivation: boolean
    completeHeaderEqual: boolean
    headerLoggedBeforeDispatch: boolean
    contextLoggedBeforeDispatch: boolean
  }
}

type ScriptEntry = readonly StreamChunk[]

class ScriptedAdapter extends LlmAdapter {
  readonly dispatches: DispatchSnapshot[] = []
  private cursor = 0
  private retryPolicy: ResolvedRetryPolicy | undefined

  constructor(
    private readonly ctx: Context,
    private readonly script: readonly ScriptEntry[],
  ) {
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

  enableRetry(): void {
    this.retryPolicy = resolveRetryPolicy({
      mode: 'normal',
      maxRetries: 2,
      retryableCodes: ['RATE_LIMIT', 'SERVER'],
      backoff: { initialDelayMs: 1, maxDelayMs: 1, jitterRatio: 0 },
    }, 'Phase 3 oracle retry policy')
  }

  override providerRetryPolicy(provider: string): ResolvedRetryPolicy | undefined {
    return provider === 'mock' ? this.retryPolicy : undefined
  }

  async * stream(options: GenerateOptions): AsyncIterable<StreamChunk> {
    const entry = this.script[this.cursor]
    if (entry === undefined) throw new Error(`script exhausted at request ${this.cursor}`)
    this.cursor += 1

    const session = options.sessionId === undefined ? undefined : this.ctx.sessions.get(options.sessionId)
    if (session === undefined) throw new Error('adapter received no live session')
    const events = session.events
    const header = foldRequestHeader(events)
    const request = normalizeRequest(options)
    const derivedMessages = clone(session.deriveMessages())
    const requestContext = session.requestContext()
    this.dispatches.push({
      ordinal: this.cursor,
      eventCount: events.length,
      lastSeq: events.at(-1)?.seq ?? -1,
      eventTypes: events.map(event => event.type),
      ...header === undefined ? {} : { foldedHeader: clone(header) },
      ...requestContext === undefined ? {} : { requestContext: clone(requestContext) },
      derivedMessages,
      request,
      checks: {
        messagesEqualDerivation: same(request.messages, derivedMessages),
        completeHeaderEqual: header !== undefined && requestMatchesHeader(request, header),
        headerLoggedBeforeDispatch: events.some(event => event.type === 'request/header'),
        contextLoggedBeforeDispatch: events.some(event => event.type === 'request/context'),
      },
    })

    for (const chunk of entry) yield chunk
  }
}

function clone<T>(value: T): T {
  return structuredClone(value)
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b)
}

function normalizeRequest(options: GenerateOptions): NormalizedRequest {
  return {
    provider: options.provider,
    model: options.model,
    ...options.reasoningEffort === undefined ? {} : { reasoningEffort: options.reasoningEffort },
    ...options.temperature === undefined ? {} : { temperature: options.temperature },
    ...options.maxTokens === undefined ? {} : { maxTokens: options.maxTokens },
    ...options.stop === undefined ? {} : { stop: [...options.stop] },
    ...options.system === undefined ? {} : { system: options.system },
    ...options.tools === undefined ? {} : { tools: clone(options.tools) },
    messages: clone(options.messages) as Message[],
    ...options.sessionId === undefined ? {} : { sessionId: String(options.sessionId) },
    frozen: {
      request: Object.isFrozen(options),
      messages: Object.isFrozen(options.messages),
    },
  }
}

function requestMatchesHeader(request: NormalizedRequest, header: EpochHeader): boolean {
  return request.provider === header.config.provider
    && request.model === header.config.model
    && request.reasoningEffort === header.config.reasoningEffort
    && request.temperature === header.config.temperature
    && request.maxTokens === header.config.maxTokens
    && same(request.stop, header.config.stop)
    && request.system === header.system
    && same(request.tools ?? [], header.tools ?? [])
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
      argumentsDelta: argumentsJson.slice(0, 4),
    },
    {
      type: 'tool-call-delta',
      index: 0,
      id: CallId(callId),
      argumentsDelta: argumentsJson.slice(4),
    },
    {
      type: 'block-end',
      index: 0,
      block: { type: 'tool-call', id: CallId(callId), name, arguments: argumentsJson },
    },
    { type: 'usage', usage: { inputTokens: 5, outputTokens: 3 } },
    { type: 'finish', reason: { kind: 'tool-calls' } },
  ]
}

function maxTokensWithTool(callId: string): StreamChunk[] {
  return [
    { type: 'block-start', index: 0, blockType: 'text' },
    { type: 'text-delta', index: 0, text: 'partial' },
    { type: 'block-end', index: 0, block: { type: 'text', text: 'partial' } },
    { type: 'block-start', index: 1, blockType: 'tool-call' },
    {
      type: 'tool-call-delta',
      index: 1,
      id: CallId(callId),
      name: 'echo',
      argumentsDelta: '{"text":"unsafe"}',
    },
    {
      type: 'block-end',
      index: 1,
      block: {
        type: 'tool-call',
        id: CallId(callId),
        name: 'echo',
        arguments: '{"text":"unsafe"}',
      },
    },
    { type: 'usage', usage: { inputTokens: 7, outputTokens: 9 } },
    { type: 'finish', reason: { kind: 'max-tokens' } },
  ]
}

async function harness(script: readonly ScriptEntry[], withRetry = false): Promise<{
  ctx: Context
  adapter: ScriptedAdapter
}> {
  const ctx = new Context()
  await ctx.plugin(InvariantRegistry)
  await ctx.plugin(LlmRuntime)
  await ctx.plugin(SessionStore)
  await ctx.plugin(SystemPrompt, { persona: 'Phase 3 oracle persona.' })
  await ctx.plugin(ToolRuntime)
  await ctx.plugin(AgentRegistry)
  if (withRetry) {
    await ctx.plugin(Object.assign((inner: Context) => {
      LlmRetry.apply(inner, {}, { random: () => 0.5 })
    }, { inject: LlmRetry.inject }))
  }
  await ctx.plugin(AgentLoop, { agents: [], maxParallelToolCalls: 1 })
  await ctx.plugin(LlmInvariant)
  await ctx.plugin(SessionInvariant)
  await ctx.plugin(AgentInvariant)
  await ctx.plugin(AgentLoopInvariant)
  if (withRetry) await ctx.plugin(LlmRetryInvariant)
  const adapter = new ScriptedAdapter(ctx, script)
  if (withRetry) adapter.enableRetry()
  ctx.llm.registerAdapter(['mock'], adapter)
  return { ctx, adapter }
}

function createAgent(ctx: Context, id: string): Agent {
  return ctx.agentLoop.create(SessionId(id), {
    provider: 'mock',
    model: 'oracle-model',
    maxTokens: 1_024,
  })
}

async function drive(agent: Agent, message: UserMessage): Promise<void> {
  agent.followup(message)
  await agent.whenIdle()
}

function eventTypes(session: Session): string[] {
  return session.events.map(event => event.type)
}

function normalizeEvents(events: Session['events']): Session['events'] {
  const retryIds = new Map<string, string>()
  let nextRetryId = 1
  return clone(events).map(event => {
    // The official retry plugin imports its UUID source before this oracle can
    // replace global crypto.randomUUID. Preserve correlation while replacing
    // that incidental random spelling with a stable fixture-local ID.
    if ((event.type === 'llm/retry' || event.type === 'llm/retry-started')
      && typeof event.data.retryId === 'string') {
      let normalized = retryIds.get(event.data.retryId)
      if (normalized === undefined) {
        normalized = `oracle-retry-${nextRetryId++}`
        retryIds.set(event.data.retryId, normalized)
      }
      event.data.retryId = normalized as typeof event.data.retryId
    }
    // Timer polling inside the official retry plugin can call Date.now a
    // scheduler-dependent number of times. Event order/seq is the contract;
    // normalize time to that stable order for a byte-reproducible fixture.
    event.time = CLOCK_START_MS + event.seq * CLOCK_STEP_MS
    return event
  })
}

function baseObservation(agent: Agent, adapter: ScriptedAdapter) {
  return {
    requests: adapter.dispatches,
    events: normalizeEvents(agent.session.events),
    derivedMessages: clone(agent.session.deriveMessages()),
    surfaceNodes: [...agent.session.surface.nodes],
  }
}

async function textCompletion() {
  const { ctx, adapter } = await harness([textResponse('done')])
  ctx.systemPrompt.section({ name: 'oracle-rule', order: 10, text: 'Only report observed facts.' })
  const agent = createAgent(ctx, 'phase3-text')
  await drive(agent, fixedUser('user-text', 'complete once'))
  const observation = baseObservation(agent, adapter)
  return {
    ...observation,
    checks: {
      eventOrder: eventTypes(agent.session),
      oneRequest: adapter.dispatches.length === 1,
      requestChecksAllTrue: Object.values(adapter.dispatches[0]?.checks ?? {}).every(Boolean),
      onlySubmittedUserMessageBeforeRequest: adapter.dispatches[0]?.eventTypes.filter(type => type === 'user/message').length === 1,
    },
  }
}

async function singleToolRoundTrip() {
  const { ctx, adapter } = await harness([
    toolResponse('call-echo-1', 'echo', '{"text":"hello"}'),
    textResponse('tool complete'),
  ])
  const bodySnapshots: Array<{ callId: string; eventTypes: string[]; matchingCallSeq?: number }> = []
  ctx.tools.register(defineContentToolFixture({
    name: 'echo',
    description: 'echo text',
    parameters: { text: { type: 'string' } },
    async execute(_args, exec) {
      const events = exec.agent?.session.events ?? []
      const matching = events.findLast(event => event.type === 'tool/call' && event.data.callId === exec.callId)
      bodySnapshots.push({
        callId: String(exec.callId),
        eventTypes: events.map(event => event.type),
        ...matching === undefined ? {} : { matchingCallSeq: matching.seq },
      })
      return [{ type: 'text', text: 'echo: hello' }]
    },
  }))
  const agent = createAgent(ctx, 'phase3-tool')
  await drive(agent, fixedUser('user-tool', 'call echo'))
  const observation = baseObservation(agent, adapter)
  const call = agent.session.events.find(event => event.type === 'tool/call')
  const result = agent.session.events.find(event => event.type === 'tool/result')
  return {
    ...observation,
    toolBodySnapshots: bodySnapshots,
    checks: {
      twoRequests: adapter.dispatches.length === 2,
      everyRequestReconstructs: adapter.dispatches.every(item => Object.values(item.checks).every(Boolean)),
      callLoggedBeforeBody: bodySnapshots[0]?.matchingCallSeq !== undefined,
      resultCitesCall: call !== undefined
        && result?.type === 'tool/result'
        && same(result.sourceEventSeqs, [call.seq]),
      secondRequestHasToolResult: adapter.dispatches[1]?.request.messages.some(message =>
        message.content.some(block => block.type === 'tool-result' && block.toolCallId === CallId('call-echo-1')),
      ) === true,
    },
  }
}

async function retrySameStep() {
  const { ctx, adapter } = await harness([
    [{ type: 'finish', reason: { kind: 'error', failure: { message: 'busy', code: 'RATE_LIMIT' } } }],
    textResponse('recovered'),
  ], true)
  const agent = createAgent(ctx, 'phase3-retry')
  await drive(agent, fixedUser('user-retry', 'retry once'))
  const observation = baseObservation(agent, adapter)
  const starts = agent.session.events.filter(event => event.type === 'step/start')
  const headers = agent.session.events.filter(event => event.type === 'request/header')
  const contexts = agent.session.events.filter(event => event.type === 'request/context')
  const messages = agent.session.events.filter(event => event.type === 'assistant/message')
  const successfulChunks = agent.session.events
    .filter(event => event.type === 'assistant/chunk')
    .slice(1)
    .map(event => event.seq)
  return {
    ...observation,
    checks: {
      twoRequestsOneStep: adapter.dispatches.length === 2 && starts.length === 1,
      oneHeaderOneContext: headers.length === 1 && contexts.length === 1,
      onlySuccessfulAssistant: messages.length === 1,
      successfulProvenanceExcludesFailedAttempt: messages[0]?.type === 'assistant/message'
        && same(messages[0].sourceEventSeqs, successfulChunks),
      durableRetryPair: agent.session.events.filter(event => event.type === 'llm/retry').length === 1
        && agent.session.events.filter(event => event.type === 'llm/retry-started').length === 1,
      everyRequestReconstructs: adapter.dispatches.every(item => Object.values(item.checks).every(Boolean)),
    },
  }
}

async function maxTokens() {
  const { ctx, adapter } = await harness([maxTokensWithTool('call-truncated')])
  let toolExecutions = 0
  ctx.tools.register(defineContentToolFixture({
    name: 'echo',
    description: 'echo text',
    parameters: { text: { type: 'string' } },
    async execute() {
      toolExecutions += 1
      return [{ type: 'text', text: 'must not run' }]
    },
  }))
  const agent = createAgent(ctx, 'phase3-max-tokens')
  await drive(agent, fixedUser('user-max', 'hit output limit'))
  const observation = baseObservation(agent, adapter)
  const assistant = agent.session.events.find(event => event.type === 'assistant/message')
  const turnEnd = agent.session.events.find(event => event.type === 'turn/end')
  return {
    ...observation,
    checks: {
      toolNotExecuted: toolExecutions === 0,
      noToolCallEvent: !agent.session.events.some(event => event.type === 'tool/call'),
      truncatedToolBlockDropped: assistant?.type === 'assistant/message'
        && assistant.data.message.content.every(block => block.type !== 'tool-call'),
      turnEndedMaxTokens: turnEnd?.type === 'turn/end' && turnEnd.data.reason.kind === 'max-tokens',
      requestReconstructs: Object.values(adapter.dispatches[0]?.checks ?? {}).every(Boolean),
    },
  }
}

async function preStepReject() {
  const { ctx, adapter } = await harness([])
  const agent = createAgent(ctx, 'phase3-reject')
  ctx.on('agent/pre-step', async ({ agent: subject }, next) => {
    return subject === agent ? { kind: 'reject' } : next()
  })
  await drive(agent, fixedUser('user-reject', 'reject before request'))
  const observation = baseObservation(agent, adapter)
  const turnStart = agent.session.events.find(event => event.type === 'turn/start')
  const turnEnd = agent.session.events.find(event => event.type === 'turn/end')
  return {
    ...observation,
    checks: {
      noRequest: adapter.dispatches.length === 0,
      noStep: !agent.session.events.some(event => event.type === 'step/start'),
      noHeaderOrContext: !agent.session.events.some(event =>
        event.type === 'request/header' || event.type === 'request/context'),
      balancedBlockedTurn: turnStart?.type === 'turn/start'
        && turnEnd?.type === 'turn/end'
        && turnStart.data.turn === turnEnd.data.turn
        && turnStart.seq < turnEnd.seq
        && turnEnd.data.reason.kind === 'blocked',
    },
  }
}

function assertNoFalseChecks(value: unknown, path = 'scenarios'): void {
  if (value === false) throw new Error(`oracle check failed: ${path}`)
  if (Array.isArray(value) || value === null || typeof value !== 'object') return
  for (const [key, child] of Object.entries(value)) {
    assertNoFalseChecks(child, `${path}.${key}`)
  }
}

function installDeterminism(): { uuidCalls: () => number; clockCalls: () => number } {
  let uuidCounter = 0
  let clockCounter = 0
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
  return { uuidCalls: () => uuidCounter, clockCalls: () => clockCounter }
}

async function main(): Promise<void> {
  const actualCommit = execFileSync('git', ['rev-parse', 'HEAD'], {
    cwd: process.cwd(), encoding: 'utf8',
  }).trim()
  if (actualCommit !== BASELINE_COMMIT) {
    throw new Error(`oracle requires upstream ${BASELINE_COMMIT}, got ${actualCommit}`)
  }
  const trackedChanges = execFileSync('git', ['status', '--porcelain', '--untracked-files=no'], {
    cwd: process.cwd(), encoding: 'utf8',
  }).trim()
  if (trackedChanges !== '') throw new Error('oracle requires a clean upstream tracked working tree')

  const counters = installDeterminism()
  const scenarios = {
    textCompletion: await textCompletion(),
    singleToolRoundTrip: await singleToolRoundTrip(),
    retrySameStep: await retrySameStep(),
    maxTokens: await maxTokens(),
    preStepReject: await preStepReject(),
  }
  assertNoFalseChecks(Object.fromEntries(
    Object.entries(scenarios).map(([name, scenario]) => [name, scenario.checks]),
  ))
  const output = {
    schemaVersion: 1,
    upstream: {
      repository: 'https://github.com/deepseek-ai/deepseek-harness',
      commit: BASELINE_COMMIT,
    },
    deterministic: {
      clockStartMs: CLOCK_START_MS,
      clockStepMs: CLOCK_STEP_MS,
      eventTimes: 'normalized from each scenario event seq',
      uuidCalls: counters.uuidCalls(),
    },
    evidence: {
      sourcePaths: [
        'packages/core/agent-loop/src/agent.ts',
        'packages/core/agent-loop/src/tool-calls.ts',
        'packages/core/agent-loop/src/invariant.ts',
        'packages/core/system-prompt/src/index.ts',
        'packages/core/session/src/request-header.ts',
        'packages/core/session/src/surface.ts',
        'packages/llm/llm/src/assembler.ts',
        'packages/llm/llm-retry/src/index.ts',
        'packages/llm/llm-retry/src/invariant.ts',
      ],
      testPaths: [
        'packages/core/agent-loop/tests/request-reconstruction.spec.ts',
        'packages/core/agent-loop/tests/loop.spec.ts',
        'packages/core/agent-loop/tests/request-error.spec.ts',
        'packages/core/agent-loop/tests/tool-calls.spec.ts',
        'packages/core/agent-loop/tests/interception.spec.ts',
        'packages/llm/llm-retry/tests/retry.spec.ts',
        'packages/llm/llm-retry/tests/invariant.spec.ts',
      ],
    },
    scenarios,
  }
  const serialized = `${JSON.stringify(output, null, 2)}\n`
  const outputPath = process.argv[2]
  if (outputPath === undefined) process.stdout.write(serialized)
  else writeFileSync(outputPath, serialized, 'utf8')
}

void main().catch((error: unknown) => {
  process.stderr.write(`${error instanceof Error ? error.stack ?? error.message : String(error)}\n`)
  process.exitCode = 1
})
