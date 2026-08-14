/**
 * Deterministic Phase 7 interaction oracle for DeepSeek Harness.
 *
 * Run this file from the pinned upstream checkout with its locked `tsx`.
 * Runtime observations use the real ACP bridge, Agent Loop, approval service,
 * tool pipeline, and headless runner over in-memory/test compositions. The Web
 * diff component needs Vite's CSS/jsdom loader, so its small rendering facts
 * are honestly checked from the pinned source plus the official test instead
 * of being presented as a runtime render from this standalone script.
 */

import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { Context } from '@deepseek-ai/cordis'
import AgentRegistry, { Inbox, type Agent, type AgentHandle, type CreateAgentOptions } from '@deepseek-ai/dsh-agent'
import AgentDefaultModelConfig from '@deepseek-ai/dsh-agent-default-model'
import AgentLoop from '@deepseek-ai/dsh-agent-loop'
import { mountAgentLoopTestDependencies } from '@deepseek-ai/dsh-agent-loop-testkit'
import * as AcpPlugin from '@deepseek-ai/dsh-acp'
import { apply as applyHeadless, internals as headlessInternals } from '@deepseek-ai/dsh-headless'
import {
  CallId,
  createAssistantMessage,
  LlmAdapter,
  type GenerateOptions,
  type Message,
  type StreamChunk,
} from '@deepseek-ai/dsh-llm'
import SessionStore, { SessionId, type Session, type SessionEvent, type UserMessage } from '@deepseek-ai/dsh-session'
import { defineContentToolFixture, type PreToolDecision } from '@deepseek-ai/dsh-tools'
import ApprovalService from '@deepseek-ai/dsh-user-approval'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'
const REPOSITORY = 'https://github.com/deepseek-ai/deepseek-harness'
const CLOCK_START_MS = 1_700_700_000_000
const CLOCK_STEP_MS = 13

const SOURCE_PATHS = [
  'packages/acp/acp/src/index.ts',
  'packages/acp/acp/src/codec.ts',
  'packages/core/agent-loop/src/agent.ts',
  'packages/core/agent-loop/src/tool-calls.ts',
  'packages/core/tools/src/index.ts',
  'packages/interaction/user-approval/src/index.ts',
  'packages/bundle/headless/src/index.ts',
  'packages/bundle/headless/src/startup.ts',
  'packages/fs/tool-fs/src/diff.ts',
  'packages/client/ui-primitives/src/DiffBlock.tsx',
  'packages/client/ui-tool/src/client/tool/models/diff-card-model.ts',
] as const

const TEST_PATHS = [
  'packages/acp/acp/tests/harness.ts',
  'packages/acp/acp/tests/bridge.spec.ts',
  'packages/acp/acp/tests/turns.spec.ts',
  'packages/acp/acp/tests/approval.spec.ts',
  'packages/acp/acp/tests/edges.spec.ts',
  'packages/core/agent-loop/tests/cancel.spec.ts',
  'packages/interaction/user-approval/tests/approval.spec.ts',
  'packages/bundle/headless/tests/headless.spec.ts',
  'packages/bundle/headless/tests/startup.spec.ts',
  'packages/fs/tool-fs/tests/diff.spec.ts',
  'packages/client/ui-primitives/tests/diff-block.client.spec.tsx',
  'packages/client/ui-tool/tests/diff-card.client.spec.tsx',
] as const

interface WorkspaceFixture {
  root: string
  workspace: string
  upstream: string
}

interface AcpTextContent {
  type: 'text'
  text: string
}

interface AcpUpdate {
  sessionUpdate: string
  content?: AcpTextContent
  [key: string]: unknown
}

interface SessionNotification {
  sessionId: string
  update: AcpUpdate
}

interface RequestPermissionRequest {
  sessionId: string
  toolCall: { toolCallId: string }
  options: Array<{ optionId: string; name: string; kind: string }>
}

type RequestPermissionResponse = {
  outcome:
    | { outcome: 'cancelled' }
    | { outcome: 'selected'; optionId: string }
}

interface AcpInitializeResponse {
  protocolVersion: number
  agentInfo: { name: string; version: string }
  agentCapabilities: {
    promptCapabilities: { image: boolean; audio: boolean; embeddedContext: boolean }
  }
  authMethods: unknown[]
}

interface AcpClientConnection {
  initialize(params: {
    protocolVersion: number
    clientCapabilities: Record<string, unknown>
  }): Promise<AcpInitializeResponse>
  newSession(params: { cwd: string; mcpServers: unknown[] }): Promise<{ sessionId: string }>
  prompt(params: {
    sessionId: string
    prompt: Array<{ type: 'text'; text: string }>
  }): Promise<{ stopReason: string }>
  cancel(params: { sessionId: string }): Promise<void>
}

interface AcpSdkRuntime {
  PROTOCOL_VERSION: number
  ndJsonStream(
    output: WritableStream<Uint8Array>,
    input: ReadableStream<Uint8Array>,
  ): unknown
  ClientSideConnection: new (
    client: (agent: unknown) => {
      sessionUpdate(params: SessionNotification): Promise<void>
      requestPermission(params: RequestPermissionRequest): Promise<RequestPermissionResponse>
    },
    stream: unknown,
  ) => AcpClientConnection
}

interface FileDiff {
  path: string
  oldText: string | null
  newText: string
}

type TurnReason = SessionEvent<'turn/end'>['data']['reason']

interface TurnEndSummary {
  turn: number
  reason: TurnReason
}

type ScriptEntry = readonly StreamChunk[] | 'hang-after-partial'

class ScriptedAdapter extends LlmAdapter {
  readonly requests: GenerateOptions[] = []
  private cursor = 0

  constructor(private readonly script: ScriptEntry[]) {
    super()
  }

  override providerInfo(provider: string) {
    if (provider !== 'mock') throw new Error(`Phase 7 oracle: unknown provider ${provider}`)
    return { id: 'mock', name: 'Mock' }
  }

  override listModels(provider: string) {
    return Promise.resolve(provider === 'mock'
      ? [{ provider: 'mock', id: 'mock', name: 'Mock' }]
      : [])
  }

  async * stream(options: GenerateOptions): AsyncIterable<StreamChunk> {
    this.requests.push(options)
    const entry = this.script[this.cursor]
    if (entry === undefined) throw new Error(`Phase 7 oracle script exhausted at ${this.cursor}`)
    this.cursor += 1
    if (entry === 'hang-after-partial') {
      yield { type: 'block-start', index: 0, blockType: 'text' }
      yield { type: 'text-delta', index: 0, text: 'partial-before-cancel' }
      await new Promise<void>((_resolve, reject) => {
        if (options.signal?.aborted) {
          reject(new Error('aborted'))
          return
        }
        options.signal?.addEventListener('abort', () => { reject(new Error('aborted')) }, { once: true })
      })
      return
    }
    for (const chunk of entry) {
      if (options.signal?.aborted) throw new Error('aborted')
      yield chunk
    }
  }
}

interface AcpHarness {
  ctx: Context
  client: AcpClientConnection
  adapter: ScriptedAdapter
  updates: AcpUpdate[]
  permissionRequests: RequestPermissionRequest[]
  onPermission: (request: RequestPermissionRequest) => RequestPermissionResponse
  dispose(): Promise<void>
}

let acpSdkPromise: Promise<AcpSdkRuntime> | undefined

function loadAcpSdk(): Promise<AcpSdkRuntime> {
  if (acpSdkPromise !== undefined) return acpSdkPromise
  const requireFromUpstream = createRequire(join(process.cwd(), 'package.json'))
  const entry = requireFromUpstream.resolve('@agentclientprotocol/sdk')
  acpSdkPromise = import(pathToFileURL(entry).href).then(module => module as AcpSdkRuntime)
  return acpSdkPromise
}

function requireCondition(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(`Phase 7 oracle assertion failed: ${message}`)
}

function textResponse(text: string): StreamChunk[] {
  return [
    { type: 'block-start', index: 0, blockType: 'text' },
    { type: 'text-delta', index: 0, text },
    { type: 'block-end', index: 0, block: { type: 'text', text } },
    { type: 'usage', usage: { inputTokens: 5, outputTokens: text.length } },
    { type: 'finish', reason: { kind: 'stop' } },
  ]
}

function toolResponse(callId: string): StreamChunk[] {
  return [
    { type: 'block-start', index: 0, blockType: 'tool-call' },
    {
      type: 'tool-call-delta',
      index: 0,
      id: CallId(callId),
      name: 'sentinel',
      argumentsDelta: '{}',
    },
    {
      type: 'block-end',
      index: 0,
      block: { type: 'tool-call', id: CallId(callId), name: 'sentinel', arguments: '{}' },
    },
    { type: 'finish', reason: { kind: 'tool-calls' } },
  ]
}

async function makeAcpHarness(script: ScriptEntry[]): Promise<AcpHarness> {
  const sdk = await loadAcpSdk()
  const adapter = new ScriptedAdapter(script)
  const ctx = new Context()
  await mountAgentLoopTestDependencies(ctx, { systemPrompt: { persona: 'Phase 7 oracle.' } })
  await ctx.plugin(AgentLoop, { agents: [], maxParallelToolCalls: 1 })
  ctx.llm.registerAdapter(['mock'], adapter)

  const agentToClient = new TransformStream<Uint8Array, Uint8Array>()
  const clientToAgent = new TransformStream<Uint8Array, Uint8Array>()
  const clientToAgentWriter = clientToAgent.writable.getWriter()
  const clientOutput = new WritableStream<Uint8Array>({
    write: chunk => clientToAgentWriter.write(chunk),
  })
  const agentStream = sdk.ndJsonStream(agentToClient.writable, clientToAgent.readable)
  const clientStream = sdk.ndJsonStream(clientOutput, agentToClient.readable)
  const updates: AcpUpdate[] = []
  const permissionRequests: RequestPermissionRequest[] = []
  const harness: AcpHarness = {
    ctx,
    adapter,
    updates,
    permissionRequests,
    onPermission: () => ({ outcome: { outcome: 'cancelled' } }),
    client: undefined as unknown as AcpClientConnection,
    dispose: async () => { await ctx.fiber.dispose() },
  }
  const makeClient = (_agent: unknown) => ({
    sessionUpdate(params: SessionNotification): Promise<void> {
      updates.push(params.update)
      return Promise.resolve()
    },
    requestPermission(params: RequestPermissionRequest): Promise<RequestPermissionResponse> {
      permissionRequests.push(params)
      return Promise.resolve(harness.onPermission(params))
    },
  })
  await ctx.plugin({
    name: 'phase7-acp-oracle',
    inject: [...AcpPlugin.inject],
    apply(inner: Context) {
      AcpPlugin.apply(inner, { provider: 'mock', model: 'mock', stream: agentStream as never })
    },
  })
  harness.client = new sdk.ClientSideConnection(makeClient, clientStream)
  return harness
}

async function initializeSession(harness: AcpHarness, cwd: string): Promise<{
  initialize: AcpInitializeResponse
  sessionId: string
  agent: Agent
}> {
  const sdk = await loadAcpSdk()
  const initialize = await harness.client.initialize({
    protocolVersion: sdk.PROTOCOL_VERSION,
    clientCapabilities: {},
  })
  const { sessionId } = await harness.client.newSession({ cwd, mcpServers: [] })
  const agent = harness.ctx.agents.get(SessionId(sessionId))
  requireCondition(agent !== undefined, 'ACP-created Agent must be registered')
  return { initialize, sessionId, agent }
}

async function waitFor(condition: () => boolean, label: string): Promise<void> {
  const deadline = Date.now() + 5_000
  while (!condition()) {
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${label}`)
    await new Promise(resolve => setTimeout(resolve, 1))
  }
}

function replaceEvery(value: string, search: string, replacement: string): string {
  return value.split(search).join(replacement)
}

function createNormalizer(fixture: WorkspaceFixture): (value: unknown) => unknown {
  const ids = new Map<string, string>()
  const uuid = /\b[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\b/giu
  const normalize = (value: unknown): unknown => {
    if (typeof value === 'string') {
      const paths = replaceEvery(
        replaceEvery(replaceEvery(value, fixture.workspace, '<workspace>'), fixture.root, '<fixture-root>'),
        fixture.upstream,
        '<upstream>',
      )
      return paths.replace(uuid, (raw) => {
        let normalized = ids.get(raw)
        if (normalized === undefined) {
          normalized = `<uuid-${String(ids.size + 1).padStart(2, '0')}>`
          ids.set(raw, normalized)
        }
        return normalized
      })
    }
    if (Array.isArray(value)) return value.map(normalize)
    if (value === null || typeof value !== 'object') return value
    return Object.fromEntries(Object.entries(value).map(([key, child]) => [key, normalize(child)]))
  }
  return normalize
}

function normalizeEvents(
  events: readonly SessionEvent[],
  normalize: (value: unknown) => unknown,
): unknown[] {
  const copied = structuredClone(events)
  for (const event of copied) event.time = CLOCK_START_MS + event.seq * CLOCK_STEP_MS
  return normalize(copied) as unknown[]
}

function contentSummary(message: Message): unknown {
  return {
    role: message.role,
    content: structuredClone(message.content),
    source: structuredClone(message.source),
  }
}

function requestTranscripts(requests: readonly GenerateOptions[]): unknown[] {
  return requests.map((request, index) => ({
    ordinal: index + 1,
    messages: request.messages.map(contentSummary),
  }))
}

function turnEndKinds(events: readonly SessionEvent[]): TurnEndSummary[] {
  return events.flatMap(event => event.type === 'turn/end'
    ? [{ turn: event.data.turn, reason: structuredClone(event.data.reason) }]
    : [])
}

function updateTexts(updates: readonly AcpUpdate[]): string[] {
  return updates.flatMap(update => update.sessionUpdate === 'agent_message_chunk'
    && update.content?.type === 'text'
    ? [update.content.text]
    : [])
}

async function acpTwoTurns(fixture: WorkspaceFixture): Promise<unknown> {
  const harness = await makeAcpHarness([
    textResponse('first committed answer'),
    textResponse('second committed answer'),
  ])
  try {
    const { initialize, sessionId, agent } = await initializeSession(harness, fixture.workspace)
    const first = await harness.client.prompt({
      sessionId,
      prompt: [{ type: 'text', text: 'first prompt' }],
    })
    const second = await harness.client.prompt({
      sessionId,
      prompt: [{ type: 'text', text: 'second prompt' }],
    })
    await waitFor(() => harness.updates.length === 2, 'two committed ACP updates')
    const normalize = createNormalizer(fixture)
    const transcripts = normalize(requestTranscripts(harness.adapter.requests))
    const ends = turnEndKinds(agent.session.events)
    const texts = updateTexts(harness.updates)
    const roles = harness.adapter.requests.map(request => request.messages.map(message => message.role))
    const checks = {
      protocolAdvertisesTextOnly: initialize.agentCapabilities.promptCapabilities.image === false
        && initialize.agentCapabilities.promptCapabilities.audio === false
        && initialize.agentCapabilities.promptCapabilities.embeddedContext === false,
      bothPromptsEndNormally: first.stopReason === 'end_turn' && second.stopReason === 'end_turn',
      oneCommittedUpdatePerTurn: JSON.stringify(texts)
        === JSON.stringify(['first committed answer', 'second committed answer']),
      secondRequestRetainsConversation: JSON.stringify(roles)
        === JSON.stringify([['user'], ['user', 'assistant', 'user']]),
      twoCompletedDurableTurns: ends.length === 2
        && ends.every(item => item.reason.kind === 'completed'),
    }
    assertNoFalseChecks(checks)
    return {
      composition: 'real ACP bridge + real Agent Loop + scripted LlmAdapter over in-memory NDJSON streams',
      initialize,
      prompts: [
        { text: 'first prompt', response: first },
        { text: 'second prompt', response: second },
      ],
      wireUpdates: harness.updates,
      providerRequestTranscripts: transcripts,
      durableTurnEnds: normalize(ends),
      durableEvents: normalizeEvents(agent.session.events, normalize),
      checks,
    }
  } finally {
    await harness.dispose()
  }
}

async function acpCancelThenContinue(fixture: WorkspaceFixture): Promise<unknown> {
  const harness = await makeAcpHarness([
    'hang-after-partial',
    textResponse('continued after cancellation'),
  ])
  try {
    const { sessionId, agent } = await initializeSession(harness, fixture.workspace)
    const firstPending = harness.client.prompt({
      sessionId,
      prompt: [{ type: 'text', text: 'start cancellable turn' }],
    })
    await waitFor(() => agent.session.events.some(event => event.type === 'assistant/chunk'
      && event.data.chunk.type === 'text-delta'
      && event.data.chunk.text === 'partial-before-cancel'), 'durable partial assistant chunk')
    await harness.client.cancel({ sessionId })
    const first = await firstPending
    await agent.whenIdle()
    const firstInterval = [...agent.session.events]
    const wireTextsAfterCancel = updateTexts(harness.updates)

    const second = await harness.client.prompt({
      sessionId,
      prompt: [{ type: 'text', text: 'continue in same session' }],
    })
    await waitFor(() => updateTexts(harness.updates).includes('continued after cancellation'), 'continued ACP update')
    const firstTypes = firstInterval.map(event => event.type)
    const partialIndex = firstInterval.findIndex(event => event.type === 'assistant/chunk'
      && event.data.chunk.type === 'text-delta')
    const stepEndIndex = firstInterval.findIndex(event => event.type === 'step/end')
    const turnEndIndex = firstInterval.findIndex(event => event.type === 'turn/end')
    const firstEnd = firstInterval.findLast(event => event.type === 'turn/end')
    const allEnds = turnEndKinds(agent.session.events)
    const checks = {
      explicitClientCancelSettlesCancelled: first.stopReason === 'cancelled',
      partialChunkDurableBeforeClosure: partialIndex >= 0
        && partialIndex < stepEndIndex
        && stepEndIndex < turnEndIndex,
      cancelledStepHasNoCommittedAssistantMessage: !firstTypes.includes('assistant/message'),
      partialChunkStaysOffAcpWire: wireTextsAfterCancel.length === 0,
      durableReasonIsUserAbort: firstEnd?.type === 'turn/end'
        && firstEnd.data.reason.kind === 'aborted'
        && firstEnd.data.reason.reason.kind === 'user',
      sameSessionContinues: second.stopReason === 'end_turn'
        && updateTexts(harness.updates).at(-1) === 'continued after cancellation',
      abortedThenCompleted: allEnds.length === 2
        && allEnds[0]?.reason.kind === 'aborted'
        && allEnds[1]?.reason.kind === 'completed',
    }
    assertNoFalseChecks(checks)
    const normalize = createNormalizer(fixture)
    return {
      composition: 'real ACP bridge + real Agent Loop; adapter emits a durable text delta then waits for caller cancellation',
      firstPrompt: { response: first, wireUpdatesBeforeContinuation: wireTextsAfterCancel },
      cancelledIntervalEvents: normalizeEvents(firstInterval, normalize),
      secondPrompt: { response: second },
      finalWireUpdates: harness.updates,
      finalDurableTurnEnds: normalize(allEnds),
      checks,
    }
  } finally {
    await harness.dispose()
  }
}

type ApprovalCase = 'allow' | 'reject' | 'cancel'

function permissionResponse(kind: ApprovalCase): RequestPermissionResponse {
  switch (kind) {
    case 'allow': return { outcome: { outcome: 'selected', optionId: 'allow-once' } }
    case 'reject': return { outcome: { outcome: 'selected', optionId: 'reject-once' } }
    case 'cancel': return { outcome: { outcome: 'cancelled' } }
  }
}

async function acpApprovalCase(fixture: WorkspaceFixture, kind: ApprovalCase): Promise<unknown> {
  const callId = `call-${kind}`
  const sentinelPath = join(fixture.workspace, `approval-${kind}.txt`)
  const trace: string[] = []
  const harness = await makeAcpHarness([
    toolResponse(callId),
    textResponse(`finished ${kind}`),
  ])
  try {
    await harness.ctx.plugin(ApprovalService, { policy: 'ask' })
    harness.ctx.tools.register(defineContentToolFixture({
      name: 'sentinel',
      description: 'Write one temporary oracle sentinel after approval.',
      parameters: {},
      async execute() {
        trace.push('tool-body')
        await writeFile(sentinelPath, `${kind}\n`, 'utf8')
        return [{ type: 'text', text: `sentinel ${kind}` }]
      },
    }))
    harness.ctx.on('tools/pre-execute', async (exec, next): Promise<PreToolDecision> => {
      if (exec.name !== 'sentinel') return next()
      trace.push('gate:ask')
      return { kind: 'ask', reason: `Phase 7 ${kind} oracle` }
    })
    harness.ctx.on('session/event', (_session, event) => {
      if (event.type === 'tool/call'
        || event.type === 'approval/asked'
        || event.type === 'approval/decided'
        || event.type === 'tool/result') {
        trace.push(`session:${event.type}`)
      }
    })
    harness.onPermission = () => {
      trace.push(`client:${kind}`)
      return permissionResponse(kind)
    }

    const { sessionId, agent } = await initializeSession(harness, fixture.workspace)
    const prompt = await harness.client.prompt({
      sessionId,
      prompt: [{ type: 'text', text: `exercise approval ${kind}` }],
    })
    await waitFor(() => updateTexts(harness.updates).includes(`finished ${kind}`), `${kind} final answer`)
    const diskAfter = existsSync(sentinelPath) ? await readFile(sentinelPath, 'utf8') : null
    const relevantEvents = agent.session.events.filter(event => event.type === 'tool/call'
      || event.type === 'approval/asked'
      || event.type === 'approval/decided'
      || event.type === 'tool/result')
    const asked = relevantEvents.find(event => event.type === 'approval/asked')
    const decided = relevantEvents.find(event => event.type === 'approval/decided')
    const result = relevantEvents.find(event => event.type === 'tool/result')
    const expectedTrace = kind === 'allow'
      ? [
          'session:tool/call',
          'gate:ask',
          'session:approval/asked',
          'client:allow',
          'session:approval/decided',
          'tool-body',
          'session:tool/result',
        ]
      : [
          'session:tool/call',
          'gate:ask',
          'session:approval/asked',
          `client:${kind}`,
          'session:approval/decided',
          'session:tool/result',
        ]
    const expectedOutcome = kind === 'allow' ? 'allowed-once' : kind === 'reject' ? 'rejected' : 'cancelled'
    const resultIsError = result?.type === 'tool/result'
      ? result.data.message.content[0].isError
      : undefined
    const checks = {
      exactAuditAndSideEffectOrder: JSON.stringify(trace) === JSON.stringify(expectedTrace),
      askedAndDecidedArePaired: asked?.type === 'approval/asked'
        && decided?.type === 'approval/decided'
        && asked.data.id === decided.data.id,
      durableOutcomeMatchesClient: decided?.type === 'approval/decided'
        && decided.data.outcome === expectedOutcome,
      onlyAllowRunsBody: (kind === 'allow' && diskAfter === 'allow\n')
        || (kind !== 'allow' && diskAfter === null),
      toolResultMatchesGrant: resultIsError === (kind !== 'allow'),
      onePermissionRequest: harness.permissionRequests.length === 1,
      exactOneShotOptions: harness.permissionRequests[0]?.options[0]?.optionId === 'allow-once'
        && harness.permissionRequests[0]?.options[1]?.optionId === 'reject-once'
        && harness.permissionRequests[0]?.options.length === 2,
      callIdentityPreserved: harness.permissionRequests[0]?.toolCall.toolCallId === callId,
      toolPresentationStaysOffAcpWire: JSON.stringify(updateTexts(harness.updates))
        === JSON.stringify([`finished ${kind}`]),
      promptCompletesAfterDeniedResult: prompt.stopReason === 'end_turn'
        && harness.adapter.requests.length === 2,
    }
    assertNoFalseChecks(checks)
    const normalize = createNormalizer(fixture)
    return {
      decision: kind,
      composition: 'real ACP permission bridge + real ApprovalService audit + real tool pre-execute pipeline',
      promptResponse: prompt,
      permissionRequest: normalize({
        ...harness.permissionRequests[0],
        sessionId: '<session>',
      }),
      trace,
      relevantDurableEvents: normalizeEvents(relevantEvents, normalize),
      sideEffect: { path: `<workspace>/approval-${kind}.txt`, diskAfter },
      wireUpdates: harness.updates,
      checks,
    }
  } finally {
    await harness.dispose()
  }
}

async function approvalScenarios(fixture: WorkspaceFixture): Promise<unknown> {
  return {
    allow: await acpApprovalCase(fixture, 'allow'),
    reject: await acpApprovalCase(fixture, 'reject'),
    cancel: await acpApprovalCase(fixture, 'cancel'),
  }
}

interface HeadlessScript {
  before?(session: Session): void
  afterPrompt(session: Session, message: UserMessage): Promise<void> | void
}

function appendHeadlessTurn(
  session: Session,
  turn: number,
  message: UserMessage,
  text: string | undefined,
  reason: TurnReason,
): void {
  session.append('turn/start', { turn })
  session.append('step/start', { turn, step: 1 })
  session.append('user/message', message, { surfaceOp: 'append' })
  if (text !== undefined) {
    session.append('assistant/message', {
      turn,
      step: 1,
      message: createAssistantMessage({
        content: [{ type: 'text', text }],
        source: { provider: 'test-provider', model: 'test-model' },
      }),
    }, { surfaceOp: 'append' })
  }
  session.append('step/end', { turn, step: 1 })
  session.append('turn/end', { turn, reason })
}

async function runHeadlessCase(script: HeadlessScript): Promise<{
  code: number
  stdout: string
  stderr: string
  order: string[]
}> {
  const ctx = new Context()
  await ctx.plugin(SessionStore)
  await ctx.plugin(AgentRegistry)
  await ctx.plugin(AgentDefaultModelConfig, { provider: 'test-provider', model: 'test-model' })
  ctx.agents.setFactory({
    async createAgent(ownerCtx: Context, options: CreateAgentOptions): Promise<AgentHandle> {
      const session = ctx.sessions.create(options.sessionId, {
        ...options.meta === undefined ? {} : { meta: options.meta },
      })
      let idle = Promise.resolve()
      const agent = {} as Agent
      const agentCtx = ownerCtx.extend({ agent })
      Object.assign(agent, {
        id: session.id,
        options: options.agentOptions ?? {},
        session,
        inbox: new Inbox(session, { inserted: () => {}, discarded: () => {}, claimed: () => {} }),
        status: 'idle',
        ctx: agentCtx,
        cancel: () => {},
        runMaintenance: () => Promise.reject(new Error('not used by Phase 7 headless oracle')),
        send: () => {},
        followup: (message: UserMessage) => {
          agent.inbox.append('next-turn', message)
          idle = Promise.resolve().then(() => script.afterPrompt(session, message))
        },
        steer: () => {},
        inject: () => {},
        whenIdle: () => idle,
      } satisfies Partial<Agent>)
      await options.setup?.(agentCtx)
      script.before?.(session)
      ctx.agents.register(agent)
      return { agent, dispose: () => Promise.resolve() }
    },
    resume: () => Promise.reject(new Error('not used by Phase 7 headless oracle')),
  })

  let stdout = ''
  let stderr = ''
  const order: string[] = []
  ctx.on('session/flush', () => { order.push('flush') })
  headlessInternals.stdout = {
    write(chunk: string) {
      order.push('stdout')
      stdout += chunk
      return true
    },
  }
  headlessInternals.stderr = {
    write(chunk: string) {
      order.push('stderr')
      stderr += chunk
      return true
    },
  }
  const exited = new Promise<number>((resolveExit) => {
    ctx.provide('appExit', (code: number) => {
      order.push('exit')
      resolveExit(code)
    })
  })
  try {
    applyHeadless(ctx, { task: 'do the thing' })
    return { code: await exited, stdout, stderr, order }
  } finally {
    await ctx.fiber.dispose()
  }
}

async function headlessScenarios(): Promise<unknown> {
  const original = { ...headlessInternals }
  try {
    const completed = await runHeadlessCase({
      before(session) {
        const message = {
          id: 'pre-task-message',
          role: 'user',
          content: [{ type: 'text', text: 'setup' }],
          source: { kind: 'user' },
        } as UserMessage
        appendHeadlessTurn(session, 0, message, 'pre-task noise', { kind: 'completed' })
      },
      afterPrompt(session, message) {
        appendHeadlessTurn(session, 1, message, 'intermediate answer', { kind: 'completed' })
        appendHeadlessTurn(session, 2, message, 'final answer', { kind: 'completed' })
      },
    })
    const aborted = await runHeadlessCase({
      afterPrompt(session, message) {
        appendHeadlessTurn(session, 1, message, undefined, { kind: 'aborted', reason: { kind: 'user' } })
      },
    })
    const providerError = await runHeadlessCase({
      afterPrompt(session, message) {
        appendHeadlessTurn(session, 1, message, undefined, {
          kind: 'error',
          error: { code: 'SERVER', message: 'provider unavailable' },
        })
      },
    })
    const checks = {
      completedPrintsOnlyLastOwnedAssistant: completed.stdout === 'final answer\n'
        && !completed.stdout.includes('pre-task noise')
        && !completed.stdout.includes('intermediate answer'),
      successfulStderrEmpty: completed.stderr === '',
      flushPrecedesOutputAndExit: JSON.stringify(completed.order)
        === JSON.stringify(['flush', 'stdout', 'exit']),
      completedExitZero: completed.code === 0,
      abortedExitOneWithBlankStdout: aborted.code === 1
        && aborted.stdout === '\n'
        && aborted.stderr === '',
      providerErrorIsDurableFailure: providerError.code === 1
        && providerError.stdout === '\n'
        && providerError.stderr === 'dsh: SERVER: provider unavailable\n',
    }
    assertNoFalseChecks(checks)
    return {
      composition: 'real exported headless apply() over the official test-style AgentRegistry/SessionStore factory seam',
      completed,
      aborted,
      providerError,
      checks,
    }
  } finally {
    Object.assign(headlessInternals, original)
  }
}

async function sourceCheckedDiffFacts(upstream: string): Promise<unknown> {
  const primitivePath = 'packages/client/ui-primitives/src/DiffBlock.tsx'
  const primitiveTestPath = 'packages/client/ui-primitives/tests/diff-block.client.spec.tsx'
  const source = readFileSync(join(upstream, primitivePath), 'utf8')
  const test = readFileSync(join(upstream, primitiveTestPath), 'utf8')
  const diffModuleUrl = pathToFileURL(join(upstream, 'packages/fs/tool-fs/src/diff.ts')).href
  const diffModule = await import(diffModuleUrl) as {
    computeHunkDiffs(path: string, before: string, after: string): FileDiff[]
  }
  const { computeHunkDiffs } = diffModule
  const noNewline = computeHunkDiffs('n.txt', 'x', 'y')
  const trailingNewline = computeHunkDiffs('new.txt', '', 'hello\n')
  const interiorBlank = computeHunkDiffs('blank.txt', '', 'x\n\ny\n')
  const checks = {
    pureDiffDropsPatchMarker: JSON.stringify(noNewline)
      === JSON.stringify([{ path: 'n.txt', oldText: 'x', newText: 'y' }])
      && !JSON.stringify(noNewline).includes('No newline at end of file'),
    pureDiffTreatsTerminatorAsNotContent: JSON.stringify(trailingNewline)
      === JSON.stringify([{ path: 'new.txt', oldText: null, newText: 'hello' }]),
    pureDiffKeepsInteriorBlank: JSON.stringify(interiorBlank)
      === JSON.stringify([{ path: 'blank.txt', oldText: null, newText: 'x\n\ny' }]),
    renderSourceDropsEmptySide: source.includes("if (text === '') return []"),
    renderSourceDropsOneTrailingTerminator: source.includes(
      "const body = text.endsWith('\\n') ? text.slice(0, -1) : text",
    ),
    renderSourceUsesSameFileGap: source.includes("else rows.push({ kind: 'gap', text: '⋯' })"),
    renderSourceCountsDistinctPaths: source.includes('const paths = new Set<string>()'),
    renderSourceHasExactFooter: source.includes(
      '└ +{added} -{removed} · {files} file{files === 1 ? \'\' : \'s\'}',
    ),
    officialRenderTestPinsTrailingNewline: test.includes(
      "it('treats a trailing newline as a terminator, not an extra blank line'",
    ) && test.includes("expect(changeRows(container)).toEqual(['hello'])"),
    officialRenderTestPinsDeletionAndInteriorBlank: test.includes(
      "it('renders a full deletion as removed-only with no phantom added line'",
    ) && test.includes("it('keeps a genuine interior blank line'"),
  }
  assertNoFalseChecks(checks)
  return {
    pureComputation: {
      mode: 'executed computeHunkDiffs from the pinned upstream source',
      noTrailingNewline: noNewline,
      trailingNewline,
      interiorBlank,
    },
    webRenderContract: {
      mode: 'source-and-official-test-derived; this standalone tsx generator does not claim to run the React/CSS/jsdom renderer',
      facts: {
        emptyTextProducesZeroRows: true,
        oneTrailingNewlineTerminatesTheLastLine: true,
        interiorBlankLineSurvives: true,
        sameFileSecondHunkUsesEllipsisGap: true,
        footerCountsAddedRemovedAndDistinctFiles: true,
      },
      sourcePath: primitivePath,
      testPath: primitiveTestPath,
    },
    checks,
  }
}

function assertNoFalseChecks(value: unknown, path = 'oracle'): void {
  if (value === false) throw new Error(`Phase 7 oracle check failed: ${path}`)
  if (Array.isArray(value) || value === null || typeof value !== 'object') return
  for (const [key, child] of Object.entries(value)) assertNoFalseChecks(child, `${path}.${key}`)
}

function assertPinnedCleanUpstream(upstream: string): void {
  const actualCommit = execFileSync('git', ['rev-parse', 'HEAD'], {
    cwd: upstream,
    encoding: 'utf8',
  }).trim()
  if (actualCommit !== BASELINE_COMMIT) {
    throw new Error(`oracle requires upstream ${BASELINE_COMMIT}, got ${actualCommit}`)
  }
  const workingTreeChanges = execFileSync('git', ['status', '--porcelain'], {
    cwd: upstream,
    encoding: 'utf8',
  }).trim()
  if (workingTreeChanges !== '') throw new Error('oracle requires a clean upstream working tree')
  for (const path of [...SOURCE_PATHS, ...TEST_PATHS]) {
    requireCondition(existsSync(join(upstream, path)), `missing cited upstream path: ${path}`)
  }
}

async function buildOracle(fixture: WorkspaceFixture): Promise<unknown> {
  const scenarios = {
    acpTwoTurns: await acpTwoTurns(fixture),
    acpCancelThenContinue: await acpCancelThenContinue(fixture),
    approval: await approvalScenarios(fixture),
    headless: await headlessScenarios(),
    diff: await sourceCheckedDiffFacts(fixture.upstream),
  }
  return {
    schemaVersion: 1,
    upstream: {
      repository: REPOSITORY,
      commit: BASELINE_COMMIT,
    },
    evidence: {
      sourcePaths: SOURCE_PATHS,
      testPaths: TEST_PATHS,
      executionModes: {
        acp: 'runtime public/test composition',
        approval: 'runtime public/test composition with a temporary-file side-effect sentinel',
        headless: 'runtime public/test composition',
        diffComputation: 'runtime pure upstream function',
        diffWebRender: 'source-and-official-test-derived checked fact; not runtime-rendered by this generator',
      },
    },
    scenarios,
    deterministic: {
      eventTimes: `normalized as ${CLOCK_START_MS} + seq * ${CLOCK_STEP_MS}`,
      randomUuids: 'normalized by first appearance within each scenario',
      temporaryPaths: 'normalized to <fixture-root> and <workspace>',
      freshTemporaryWorkspace: true,
    },
    safety: {
      networkAccess: 'none',
      credentialAccess: 'none',
      filesystemWrites: 'fresh platform temporary directory and explicit output path only',
      realModelCalls: 'none; all model streams are deterministic scripted adapters',
    },
  }
}

async function main(): Promise<void> {
  const upstream = resolve(process.cwd())
  assertPinnedCleanUpstream(upstream)
  const root = await mkdtemp(join(tmpdir(), 'dsh-phase7-oracle-'))
  const workspace = join(root, 'workspace')
  await mkdir(workspace)
  const fixture: WorkspaceFixture = { root, workspace, upstream }
  try {
    const output = await buildOracle(fixture)
    assertPinnedCleanUpstream(upstream)
    const serialized = `${JSON.stringify(output, null, 2)}\n`
    requireCondition(!serialized.includes(root), 'temporary root leaked into fixture')
    requireCondition(!serialized.includes(upstream), 'absolute upstream path leaked into fixture')
    const outputPath = process.argv[2]
    if (outputPath === undefined) process.stdout.write(serialized)
    else writeFileSync(outputPath, serialized, 'utf8')
  } finally {
    await rm(root, { recursive: true, force: true })
  }
}

void main().catch((error: unknown) => {
  process.stderr.write(`${error instanceof Error ? error.stack ?? error.message : String(error)}\n`)
  process.exitCode = 1
})
