/**
 * Deterministic maintainer-only Phase 8 oracle for DeepSeek Harness.
 *
 * Run from the pinned upstream checkout with its locked `tsx`. Default Rust
 * tests consume only the checked-in JSON; this script never calls a model or
 * uses credentials. JSONL byte scanning and persistence interpretation are
 * executed here, while filesystem durability mechanics remain covered by the
 * cited upstream tests rather than being overclaimed by this small oracle.
 */

import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'
import { Context } from '@deepseek-ai/cordis'
import type { Agent } from '@deepseek-ai/dsh-agent'
import BasicCompactionEngine from '@deepseek-ai/dsh-compaction-basic'
import ToolResultPruner, {
  codePointLength,
  DEFAULTS as TOOL_RESULT_PRUNER_DEFAULTS,
  PRUNE_MARKER,
  resolveConfig as resolveToolResultPrunerConfig,
} from '@deepseek-ai/dsh-compaction-tool-result-pruner'
import InvariantRegistry from '@deepseek-ai/dsh-invariants'
import {
  CallId,
  freezeMessage,
  LlmRuntime,
  MessageId,
} from '@deepseek-ai/dsh-llm'
import type {
  ContentBlock,
  Message,
  ToolResultMessage,
  ToolSchema,
} from '@deepseek-ai/dsh-llm'
import SessionStore, {
  decodeStorageRecord,
  interruptedTurnClosers,
  Session,
  SessionId,
} from '@deepseek-ai/dsh-session'
import type { SessionEvent, SessionHeader } from '@deepseek-ai/dsh-session'
import * as SessionInvariant from '@deepseek-ai/dsh-session/invariant'
import {
  PersistenceCoordinator,
  SessionPersistenceRevision,
} from '@deepseek-ai/dsh-session-persistence'
import type { PersistenceBackend } from '@deepseek-ai/dsh-session-persistence'
import TokenMeter from '@deepseek-ai/dsh-token-meter'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'
const REPOSITORY = 'https://github.com/deepseek-ai/deepseek-harness'
const FIXED_TIME = 1_700_800_000_000
const COMPLETED_TURNS = 3
const CHUNKS_PER_TURN = 1_400

const SOURCE_PATHS = [
  'packages/core/session/src/index.ts',
  'packages/core/session/src/types.ts',
  'packages/core/session/src/surface.ts',
  'packages/core/session/src/repair.ts',
  'packages/core/session/src/chunk-rows.ts',
  'packages/core/session/src/known-event-types.ts',
  'packages/compaction/compaction/src/types.ts',
  'packages/compaction/compaction/src/invariant.ts',
  'packages/compaction/compaction-basic/src/index.ts',
  'packages/compaction/compaction-basic/src/region.ts',
  'packages/compaction/compaction-basic/src/summarizer.ts',
  'packages/compaction/compaction-tool-result-pruner/src/config.ts',
  'packages/compaction/compaction-tool-result-pruner/src/index.ts',
  'packages/compaction/compaction-tool-result-pruner/src/invariant.ts',
  'packages/compaction/compaction-tool-result-pruner/src/types.ts',
  'packages/llm/token-meter/src/index.ts',
  'packages/llm/token-meter/src/estimate.ts',
  'packages/llm/token-meter/src/surface-fold.ts',
  'packages/llm/token-meter/src/surface-projection.ts',
  'packages/llm/token-meter/src/usage-projection.ts',
  'packages/llm/token-meter/src/breakdown-projection.ts',
  'packages/session/session-persistence/src/coordinator.ts',
  'packages/session/session-persistence-jsonl/src/format.ts',
  'packages/session/session-persistence-jsonl/src/index.ts',
] as const

const TEST_PATHS = [
  'packages/core/session/tests/session.spec.ts',
  'packages/core/session/tests/repair.spec.ts',
  'packages/core/session/tests/invariant.spec.ts',
  'packages/core/session/tests/surface.spec.ts',
  'packages/compaction/compaction/tests/invariant.spec.ts',
  'packages/compaction/compaction-basic/tests/compaction-basic.spec.ts',
  'packages/compaction/compaction-basic/tests/manual-compaction.spec.ts',
  'packages/compaction/compaction-tool-result-pruner/tests/tool-result-pruner.spec.ts',
  'packages/llm/token-meter/tests/context-breakdown-projection.spec.ts',
  'packages/llm/token-meter/tests/token-meter.spec.ts',
  'packages/llm/token-meter/tests/token-usage-projection.spec.ts',
  'packages/session/session-persistence/tests/contract.ts',
  'packages/session/session-persistence/tests/coordinator-contract.ts',
  'packages/session/session-persistence-jsonl/tests/jsonl.spec.ts',
] as const

function requireCondition(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(`Phase 8 oracle assertion failed: ${message}`)
}

function sha256(value: unknown): string {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

function same(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

function errorFact(error: unknown): { name: string; message: string } {
  return error instanceof Error
    ? { name: error.name, message: error.message }
    : { name: typeof error, message: String(error) }
}

function captureFailure(action: () => unknown): { name: string; message: string } {
  try {
    action()
    return { name: 'ACCEPTED', message: 'ACCEPTED' }
  } catch (error: unknown) {
    return errorFact(error)
  }
}

function typeRuns(events: readonly SessionEvent[]): Array<{ type: string; count: number }> {
  const runs: Array<{ type: string; count: number }> = []
  for (const event of events) {
    const last = runs.at(-1)
    if (last?.type === event.type) last.count += 1
    else runs.push({ type: event.type, count: 1 })
  }
  return runs
}

function textFact(type: string, text: string): Record<string, unknown> {
  return {
    type,
    chars: [...text].length,
    sha256: sha256(text),
    ...(text.length <= 512 ? { text } : {
      prefix: text.slice(0, 32),
      suffix: text.slice(-32),
    }),
  }
}

function messageFacts(messages: readonly Message[]): unknown[] {
  return messages.map(message => ({
    id: message.id,
    role: message.role,
    source: message.source,
    content: message.content.map(block => {
      if (block.type === 'text' || block.type === 'reasoning') {
        return textFact(block.type, block.text)
      }
      return structuredClone(block)
    }),
  }))
}

function replaceExactStrings(
  value: unknown,
  replacements: ReadonlyMap<string, string>,
): unknown {
  if (typeof value === 'string') return replacements.get(value) ?? value
  if (Array.isArray(value)) return value.map(item => replaceExactStrings(item, replacements))
  if (value === null || typeof value !== 'object') return value
  return Object.fromEntries(Object.entries(value).map(([key, child]) => [
    key,
    replaceExactStrings(child, replacements),
  ]))
}

function fixedUser(id: string, text: string) {
  return freezeMessage({
    id: MessageId(id),
    role: 'user' as const,
    content: [{ type: 'text' as const, text }],
    source: { kind: 'user' as const },
  })
}

function fixedAssistant(id: string, content: ContentBlock[]) {
  return freezeMessage({
    id: MessageId(id),
    role: 'assistant' as const,
    content,
    source: { kind: 'model' as const, provider: 'mock', model: 'mock-model' },
  })
}

function fixedToolResult(
  id: string,
  callId: CallId,
  content: ContentBlock[],
  isError: boolean,
): ToolResultMessage {
  return freezeMessage<ToolResultMessage>({
    id: MessageId(id),
    role: 'user',
    source: { kind: 'tool', callId },
    content: [{
      type: 'tool-result',
      toolCallId: callId,
      content,
      isError,
    }],
  })
}

function contentFacts(blocks: readonly ContentBlock[]): unknown[] {
  return blocks.map(block => {
    if (block.type === 'text' || block.type === 'reasoning') {
      return textFact(block.type, block.text)
    }
    return structuredClone(block)
  })
}

function toolResultDataWithoutContent(event: SessionEvent<'tool/result'>): unknown {
  const result = event.data.message.content[0]
  return {
    ...event.data,
    message: {
      ...event.data.message,
      content: [{ ...result, content: null }],
    },
  }
}

function appendPrunableToolStep(
  session: Session,
  sessionLabel: string,
  text: string,
  extra: Record<string, unknown> = {},
): SessionEvent<'tool/result'> {
  const callId = CallId(`call-${sessionLabel}`)
  session.append('turn/start', { turn: 1 })
  session.append('step/start', { turn: 1, step: 1 })
  session.append('assistant/message', {
    turn: 1,
    step: 1,
    message: fixedAssistant(`message-assistant-${sessionLabel}`, [{
      type: 'tool-call', id: callId, name: 'bash', arguments: '{}',
    }]),
  }, { surfaceOp: 'append' })
  session.append('tool/call', {
    turn: 1,
    step: 1,
    callId,
    name: 'bash',
    arguments: '{}',
  })
  const result = session.append('tool/result', {
    turn: 1,
    step: 1,
    message: fixedToolResult(
      `message-tool-result-${sessionLabel}`,
      callId,
      [{ type: 'text', text }],
      Object.hasOwn(extra, 'error'),
    ),
    ...extra,
  }, { surfaceOp: 'append' })
  session.append('step/end', { turn: 1, step: 1 })
  session.append('turn/end', { turn: 1, reason: { kind: 'completed' } })
  return result
}

interface OracleSummarizationInput {
  readonly system?: string
  readonly tools?: readonly ToolSchema[]
  readonly messages: readonly Message[]
}

interface JsonlScanResult {
  meta: SessionHeader
  events: SessionEvent[]
  committedBytes: number
}

interface JsonlFormatRuntime {
  eventLines(events: readonly SessionEvent[], packChunks: boolean): string
  scanLog(buffer: Buffer): JsonlScanResult
  toHeaderLine(header: SessionHeader): unknown
}

interface ShadowPriceClaim {
  readonly start: number
  readonly end: number
  readonly tokens: number
}

interface SurfaceProjectionFold {
  readonly deltaTokens: number
  readonly claim: ShadowPriceClaim | undefined
}

interface TokenSurfaceProjectionRuntime {
  foldSurfaceProjection(
    claim: ShadowPriceClaim | undefined,
    event: SessionEvent,
  ): SurfaceProjectionFold
}

async function loadJsonlFormat(upstream: string): Promise<JsonlFormatRuntime> {
  const moduleUrl = pathToFileURL(join(
    upstream,
    'packages/session/session-persistence-jsonl/src/format.ts',
  )).href
  return await import(moduleUrl) as JsonlFormatRuntime
}

async function loadTokenSurfaceProjection(
  upstream: string,
): Promise<TokenSurfaceProjectionRuntime> {
  const moduleUrl = pathToFileURL(join(
    upstream,
    'packages/llm/token-meter/src/surface-projection.ts',
  )).href
  return await import(moduleUrl) as TokenSurfaceProjectionRuntime
}

class FixedCompactionEngine extends BasicCompactionEngine {
  readonly calls: OracleSummarizationInput[] = []

  override summarize(
    input: OracleSummarizationInput,
    _agent: Agent,
    _signal?: AbortSignal,
  ): Promise<{
      summary: ContentBlock[]
      rawOutput: ContentBlock[]
      provider: string
      model: string
      maxTokens: number
      usage: { inputTokens: number; outputTokens: number; reasoningTokens: number }
    }> {
    this.calls.push(structuredClone(input))
    return Promise.resolve({
      summary: [{
        type: 'text',
        text: '## Primary Request and Intent\n- Continue the deterministic oracle task.\n\n'
          + '## Current Work\n- Long reasoning history was preserved.\n\n'
          + '## Next Step\n- Continue after the checkpoint.',
      }],
      rawOutput: [
        { type: 'reasoning', text: 'fixed private summary reasoning' },
        { type: 'text', text: 'fixed raw summary text' },
      ],
      provider: 'oracle-summary-provider',
      model: 'oracle-summary-model',
      maxTokens: 256,
      usage: { inputTokens: 80, outputTokens: 40, reasoningTokens: 8 },
    })
  }
}

function agentFor(session: Session): Agent {
  return {
    session,
    options: { provider: 'mock', model: 'mock-model' },
  } as Agent
}

function appendReasoningTurn(session: Session, turn: number): void {
  const historyText = `turn ${turn} history ` + 'context '.repeat(48).trim()
  const reasoningText = Array.from(
    { length: CHUNKS_PER_TURN },
    (_, index) => String.fromCharCode(97 + ((turn + index) % 26)),
  ).join('')
  session.append('turn/start', { turn })
  session.append('user/message', fixedUser(`message-user-${turn}`, historyText), {
    surfaceOp: 'append',
  })
  session.append('step/start', { turn, step: 1 })
  if (turn === 1) {
    session.append('request/header', {
      header: {
        config: { provider: 'mock', model: 'mock-model' },
        system: 'Phase 8 deterministic oracle.',
        tools: [],
      },
      reason: 'initial',
    })
  }
  const sourceEventSeqs: number[] = []
  for (const text of reasoningText) {
    sourceEventSeqs.push(session.append('assistant/chunk', {
      turn,
      step: 1,
      chunk: { type: 'reasoning-delta', index: 0, text },
    }).seq)
  }
  session.append('assistant/message', {
    turn,
    step: 1,
    message: fixedAssistant(`message-assistant-${turn}`, [
      { type: 'reasoning', text: reasoningText },
      { type: 'text', text: `answer ${turn}: ${historyText}` },
    ]),
  }, { surfaceOp: 'append', sourceEventSeqs })
  session.append('step/end', { turn, step: 1 })
  session.append('turn/end', { turn, reason: { kind: 'completed' } })
}

async function longReasoningAndCompaction(format: JsonlFormatRuntime): Promise<unknown> {
  const ctx = new Context()
  void new LlmRuntime(ctx)
  void new TokenMeter(ctx)
  const engine = new FixedCompactionEngine(ctx, { auto: false })
  try {
    const session = Session.create(SessionId('phase8-long-reasoning'))
    for (let turn = 1; turn <= COMPLETED_TURNS; turn += 1) {
      appendReasoningTurn(session, turn)
    }
    session.append('turn/start', { turn: COMPLETED_TURNS + 1 })
    session.append('user/message', fixedUser(
      'message-user-continue',
      'Continue after compacting the old reasoning-heavy turns.',
    ), { surfaceOp: 'append' })

    const beforeEvents = session.events
    const beforeSurface = [...session.surface.nodes]
    const beforeMessages = session.deriveMessages()
    const beforeHash = sha256(beforeEvents)
    const beforeChunks = beforeEvents.filter(event => event.type === 'assistant/chunk')
    const beforeChunkHash = sha256(beforeChunks)
    const beforeMeasurement = ctx.tokenMeter.measure(session)
    const assistantSourceRanges = beforeEvents
      .filter((event): event is SessionEvent<'assistant/message'> => event.type === 'assistant/message')
      .map(event => ({
        assistantSeq: event.seq,
        sourceEventSeqCount: event.sourceEventSeqs?.length ?? 0,
        firstSourceEventSeq: event.sourceEventSeqs?.[0],
        lastSourceEventSeq: event.sourceEventSeqs?.at(-1),
        sourceEventSeqSha256: sha256(event.sourceEventSeqs ?? []),
        contiguous: event.sourceEventSeqs?.every((seq, index, seqs) =>
          index === 0 || seq === (seqs[index - 1] as number) + 1) ?? false,
      }))

    const packedText = format.eventLines(beforeEvents, true)
    const packedRecords = packedText.split('\n').map(line => JSON.parse(line) as unknown)
    const decodedEvents = packedRecords.flatMap(record => decodeStorageRecord(record))
    const packedTypes = packedRecords.map(record =>
      typeof record === 'object' && record !== null && 'type' in record
        ? String((record as { type: unknown }).type)
        : typeof record)

    requireCondition(beforeEvents.length > 4_096, 'long history must exceed 4,096 logical events')
    requireCondition(beforeChunks.length === COMPLETED_TURNS * CHUNKS_PER_TURN, 'chunk count')
    requireCondition(same(decodedEvents, beforeEvents), 'packed rows must decode losslessly')
    requireCondition(beforeSurface.length >= 5, 'compaction needs a retained surface tail')

    const result = await engine.compactRegion(
      beforeSurface[0] as number,
      beforeSurface[3] as number,
      agentFor(session),
      new AbortController().signal,
    )
    const afterCompactionEvents = session.events
    const compactionAppend = afterCompactionEvents.slice(beforeEvents.length)
    const start = compactionAppend[0]
    const replacement = compactionAppend[2]
    requireCondition(start?.type === 'compaction/start', 'compaction starts first')
    requireCondition(replacement?.type === 'user/message', 'checkpoint is third')
    const compactionId = start.data.compactionId
    const checkpointMessageId = replacement.data.id
    const replacements = new Map<string, string>([
      [compactionId, 'compaction-oracle-1'],
      [checkpointMessageId, 'message-compaction-checkpoint-1'],
    ])
    const normalizedAppend = replaceExactStrings(compactionAppend, replacements)
    const normalizedResult = replaceExactStrings(result, replacements)
    const normalizedAfterMessages = replaceExactStrings(
      messageFacts(session.deriveMessages()),
      replacements,
    )
    const normalizedSummaryInput = replaceExactStrings({
      system: engine.calls[0]?.system,
      tools: engine.calls[0]?.tools,
      messages: messageFacts(engine.calls[0]?.messages ?? []),
    }, replacements)

    const compactEventCount = session.events.length
    const compactChunks = session.events.filter(event => event.type === 'assistant/chunk')
    const compactPrefixHash = sha256(session.events.slice(0, beforeEvents.length))
    const compactSurface = [...session.surface.nodes]
    const compactMeasurement = ctx.tokenMeter.measure(session)

    session.append('step/start', { turn: COMPLETED_TURNS + 1, step: 1 })
    const continuationChunk = session.append('assistant/chunk', {
      turn: COMPLETED_TURNS + 1,
      step: 1,
      chunk: { type: 'text-delta', index: 0, text: 'continued' },
    })
    session.append('assistant/message', {
      turn: COMPLETED_TURNS + 1,
      step: 1,
      message: fixedAssistant('message-assistant-continue', [
        { type: 'text', text: 'continued after compaction' },
      ]),
    }, { surfaceOp: 'append', sourceEventSeqs: [continuationChunk.seq] })
    session.append('step/end', { turn: COMPLETED_TURNS + 1, step: 1 })
    session.append('turn/end', {
      turn: COMPLETED_TURNS + 1,
      reason: { kind: 'completed' },
    })

    const replay = Session.create(
      SessionId('phase8-long-reasoning-replay'),
      structuredClone(session.events),
    )
    const finalMessages = replaceExactStrings(messageFacts(session.deriveMessages()), replacements)
    const replayMessages = replaceExactStrings(messageFacts(replay.deriveMessages()), replacements)
    const checks = {
      exceeded4096AndContinued: beforeEvents.length > 4_096 && session.seq > 4_096,
      globallyContiguous: session.events.every((event, index) => event.seq === index),
      compactionAddedExactlyFourEvents: compactEventCount - beforeEvents.length === 4,
      exactCompactionOrder: same(
        compactionAppend.map(event => event.type),
        ['compaction/start', 'compaction/summary', 'user/message', 'compaction/end'],
      ),
      oldPrefixUnchanged: compactPrefixHash === beforeHash,
      oldChunksPreserved: compactChunks.length === beforeChunks.length
        && sha256(compactChunks) === beforeChunkHash,
      everyAssistantAnchorCitesItsWholeChunkRun: assistantSourceRanges.every(range =>
        range.sourceEventSeqCount === CHUNKS_PER_TURN && range.contiguous),
      currentSurfaceShrank: compactSurface.length < beforeSurface.length,
      currentTokenPressureShrank: compactMeasurement.totalTokens < beforeMeasurement.totalTokens,
      packedRowsDecodeExactly: same(decodedEvents, beforeEvents),
      packedPhysicalRowsFewerThanLogicalEvents: packedRecords.length < beforeEvents.length,
      replayDerivesSameMessages: same(replayMessages, finalMessages),
      continuedTurnClosed: session.events.at(-1)?.type === 'turn/end',
    }
    requireCondition(Object.values(checks).every(Boolean), 'long reasoning/compaction checks')

    return {
      input: {
        completedTurns: COMPLETED_TURNS,
        reasoningChunksPerTurn: CHUNKS_PER_TURN,
        totalReasoningChunks: COMPLETED_TURNS * CHUNKS_PER_TURN,
      },
      beforeCompaction: {
        eventCount: beforeEvents.length,
        nextSeq: beforeEvents.length,
        assistantChunkCount: beforeChunks.length,
        eventSha256: beforeHash,
        assistantChunkSha256: beforeChunkHash,
        assistantSourceRanges,
        typeRuns: typeRuns(beforeEvents),
        surfaceNodes: beforeSurface,
        tokenMeasurement: beforeMeasurement,
        modelMessages: messageFacts(beforeMessages),
      },
      packedStorage: {
        batching: 'the complete pre-compaction log was supplied as one encoder batch; real write-behind batch boundaries may split runs',
        physicalRowCount: packedRecords.length,
        logicalEventCount: decodedEvents.length,
        reasoningChunkRows: packedTypes.filter(type => type === 'reasoning-chunks').length,
        physicalTypeRuns: packedTypes.reduce<Array<{ type: string; count: number }>>((runs, type) => {
          const last = runs.at(-1)
          if (last?.type === type) last.count += 1
          else runs.push({ type, count: 1 })
          return runs
        }, []),
        decodedEventSha256: sha256(decodedEvents),
      },
      compaction: {
        result: normalizedResult,
        appendedEvents: normalizedAppend,
        eventCountAfter: compactEventCount,
        eventCountDelta: compactEventCount - beforeEvents.length,
        assistantChunkCountAfter: compactChunks.length,
        assistantChunkSha256After: sha256(compactChunks),
        surfaceNodesAfter: compactSurface,
        tokenMeasurementAfter: compactMeasurement,
        modelMessagesAfter: normalizedAfterMessages,
        summarizationInput: normalizedSummaryInput,
      },
      continuation: {
        finalEventCount: session.events.length,
        nextSeq: session.seq,
        eventsAfterCompaction: session.events.slice(compactEventCount).map(event => ({
          seq: event.seq,
          type: event.type,
        })),
        finalSurfaceNodes: [...session.surface.nodes],
        finalModelMessages: finalMessages,
        replayFirstLiveSeq: replay.firstLiveSeq,
        replayModelMessages: replayMessages,
      },
      checks,
    }
  } finally {
    await ctx.fiber.dispose()
  }
}

async function toolResultPrunerScenario(
  surfaceProjection: TokenSurfaceProjectionRuntime,
): Promise<unknown> {
  const config = { thresholdChars: 50, headChars: 4, tailChars: 3 }
  const defaultConfig = resolveToolResultPrunerConfig()
  const invalidBudget = captureFailure(() => resolveToolResultPrunerConfig({
    thresholdChars: 50,
    headChars: 20,
    tailChars: 20,
  }))
  const staleConfig = captureFailure(() => resolveToolResultPrunerConfig({
    threshold: 50,
  } as never))

  const ctx = new Context()
  await ctx.plugin(SessionStore)
  await ctx.plugin(InvariantRegistry)
  await ctx.plugin(SessionInvariant)
  await ctx.plugin(TokenMeter)
  const pruner = new ToolResultPruner(ctx, config)
  try {
    const exactThresholdInput = [{ type: 'text', text: 'T'.repeat(config.thresholdChars) }] satisfies ContentBlock[]
    const exactThresholdOutput = pruner.pruneContent(exactThresholdInput)

    const unicodeInput = [{ type: 'text', text: '😀'.repeat(60) }] satisfies ContentBlock[]
    const unicodeOutput = pruner.pruneContent(unicodeInput)
    const unicodeExpected = [{
      type: 'text',
      text: `${'😀'.repeat(config.headChars)}${PRUNE_MARKER}${'😀'.repeat(config.tailChars)}`,
    }] satisfies ContentBlock[]

    const richReasoning: ContentBlock = { type: 'reasoning', text: 'private-rich-block' }
    const nestedCall: ContentBlock = {
      type: 'tool-call',
      id: CallId('call-pruner-nested'),
      name: 'nested',
      arguments: '{}',
    }
    const richInput = [
      { type: 'text', text: 'A'.repeat(40) },
      richReasoning,
      { type: 'text', text: 'B'.repeat(30) },
      nestedCall,
      { type: 'text', text: 'C'.repeat(30) },
    ] satisfies ContentBlock[]
    const richOutput = pruner.pruneContent(richInput)
    const richExpected = [
      { type: 'text', text: `AAAA${PRUNE_MARKER}` },
      richReasoning,
      nestedCall,
      { type: 'text', text: 'CCC' },
    ] satisfies ContentBlock[]

    const success = ctx.sessions.create(SessionId('phase8-pruner-success'))
    const original = appendPrunableToolStep(success, 'pruner-success', 'x'.repeat(100), {
      isError: true,
      error: { name: 'ExitError', code: 'EXIT_1' },
      meta: { diff: ['a', 'b'] },
      futureField: { nested: true },
    })
    success.append('turn/start', { turn: 2 })
    const successEventCountBefore = success.events.length
    const successSurfaceBefore = [...success.surface.nodes]
    const successMessagesBefore = messageFacts(success.deriveMessages())
    const successOriginalBefore = structuredClone(original)
    const successTokensBefore = ctx.tokenMeter.measure(success)
    const pruneResult = pruner.pruneSession(success)
    const successAdded = success.events.slice(successEventCountBefore)
    const pruneEvent = successAdded[0]
    const replacement = successAdded[1]
    requireCondition(pruneEvent?.type === 'compaction/prune', 'pruner shadow price event')
    requireCondition(replacement?.type === 'tool/result', 'pruner replacement event')
    const successOriginalAfter = success.events[original.seq]
    requireCondition(successOriginalAfter?.type === 'tool/result', 'pruner original retained')
    const successTokensAfter = ctx.tokenMeter.measure(success)
    const armedProjection = surfaceProjection.foldSurfaceProjection(undefined, pruneEvent)
    const consumedProjection = surfaceProjection.foldSurfaceProjection(
      armedProjection.claim,
      replacement,
    )
    const expectedProjectionDelta = ctx.tokenMeter.estimateMessage(replacement.data.message)
      - pruneEvent.data.shadowedTokenCount
    const successReplay = Session.create(
      SessionId('phase8-pruner-success-replay'),
      structuredClone(success.events),
    )
    const successMessagesAfter = messageFacts(success.deriveMessages())
    const successReplayMessages = messageFacts(successReplay.deriveMessages())
    const eventCountBeforeSecondPass = success.events.length
    const secondPass = pruner.pruneSession(success)

    const failure = ctx.sessions.create(SessionId('phase8-pruner-failure'))
    const failureOriginal = appendPrunableToolStep(
      failure,
      'pruner-failure',
      'z'.repeat(100),
    )
    const failureEventCountBefore = failure.events.length
    const failureSurfaceBefore = [...failure.surface.nodes]
    const failureMessagesBefore = messageFacts(failure.deriveMessages())
    const failureError = captureFailure(() => pruner.pruneSession(failure))
    const failureAdded = failure.events.slice(failureEventCountBefore)
    const orphanPrune = failureAdded[0]
    requireCondition(orphanPrune?.type === 'compaction/prune', 'orphan prune event')
    const failureRepairClosers = interruptedTurnClosers(failure.events)
    const failureReplay = Session.create(
      SessionId('phase8-pruner-failure-replay'),
      structuredClone(failure.events),
    )
    const recoveryBoundary = failureReplay.events.at(-1)
    requireCondition(recoveryBoundary?.type === 'session/end-seed', 'orphan recovery boundary')
    const orphanArmedProjection = surfaceProjection.foldSurfaceProjection(undefined, orphanPrune)
    const orphanExpiredProjection = surfaceProjection.foldSurfaceProjection(
      orphanArmedProjection.claim,
      recoveryBoundary,
    )

    const checks = {
      defaultsExact: same(defaultConfig, {
        thresholdChars: 8_192,
        headChars: 4_096,
        tailChars: 1_024,
      }) && same(defaultConfig, TOOL_RESULT_PRUNER_DEFAULTS),
      configsAreFrozen: Object.isFrozen(defaultConfig)
        && Object.isFrozen(TOOL_RESULT_PRUNER_DEFAULTS)
        && Object.isFrozen(pruner.config),
      markerHasThirtyNineCodePoints: codePointLength(PRUNE_MARKER) === 39,
      defaultReplacementHas5159TextCodePoints: TOOL_RESULT_PRUNER_DEFAULTS.headChars
        + codePointLength(PRUNE_MARKER)
        + TOOL_RESULT_PRUNER_DEFAULTS.tailChars === 5_159,
      invalidBudgetRejected: invalidBudget.name === 'Error'
        && invalidBudget.message.includes('headChars + marker + tailChars'),
      staleConfigRejected: staleConfig.name === 'Error'
        && staleConfig.message.includes('unknown key "threshold"'),
      exactThresholdIsNoOp: exactThresholdOutput === null,
      unicodeCountsCodePoints: pruner.measureContent(unicodeInput) === 60,
      unicodeHeadMarkerTailExact: same(unicodeOutput, unicodeExpected)
        && pruner.measureContent(unicodeOutput ?? []) === 46
        && !JSON.stringify(unicodeOutput).includes('\uFFFD'),
      richBlocksPreserveRelativeOrder: same(richOutput, richExpected),
      pairIsAdjacentAndOrdered: successAdded.length === 2
        && replacement.seq === pruneEvent.seq + 1,
      resultCitesExactPair: same(pruneResult, {
        pruned: [{
          originalSeq: original.seq,
          replacementSeq: replacement.seq,
          callId: original.data.message.source.callId,
          charsBefore: 100,
          charsAfter: 46,
        }],
        charsRemoved: 54,
      }),
      shadowPriceCitesOriginal: same(pruneEvent.data.shadowedRange, {
        start: original.seq,
        end: original.seq,
      }) && same(pruneEvent.data.shadowedSeqs, [original.seq])
        && pruneEvent.data.shadowedTokenCount
          === successTokensBefore.nodes.find(node => node.seq === original.seq)?.tokens,
      replacementCitesOriginal: replacement.surfaceOp !== undefined
        && replacement.surfaceOp !== 'append'
        && replacement.surfaceOp.start === original.seq
        && replacement.surfaceOp.end === original.seq
        && same(replacement.sourceEventSeqs, [original.seq]),
      replacementChangesOnlyContent: same(
        toolResultDataWithoutContent(successOriginalAfter),
        toolResultDataWithoutContent(replacement),
      ),
      originalFullEventRetained: same(successOriginalAfter, successOriginalBefore)
        && successOriginalAfter.data.message.content[0].content[0]?.type === 'text'
        && successOriginalAfter.data.message.content[0].content[0].text === 'x'.repeat(100),
      surfaceReplacedSingleNode: same(successSurfaceBefore, [2, original.seq])
        && same(success.surface.nodes, [2, replacement.seq])
        && success.surface.replaceGeneration === 1,
      tokenSurfaceShrank: successTokensAfter.surfaceTokens < successTokensBefore.surfaceTokens,
      adjacentShadowPriceProjectionConsumed: same(armedProjection.claim, {
        start: original.seq,
        end: original.seq,
        tokens: pruneEvent.data.shadowedTokenCount,
      }) && consumedProjection.claim === undefined
        && consumedProjection.deltaTokens === expectedProjectionDelta,
      replayDerivesSamePrunedMessages: same(successReplayMessages, successMessagesAfter),
      secondPassIsNoOp: same(secondPass, { pruned: [], charsRemoved: 0 })
        && success.events.length === eventCountBeforeSecondPass,
      replacementFailureIsExplicit: failureError.name !== 'ACCEPTED'
        && failureError.message.includes('tool/result surface replacement appended outside any open turn'),
      failedReplacementLeavesOnlyOrphanMarker: failureAdded.length === 1
        && failureAdded[0]?.type === 'compaction/prune'
        && failure.events.filter(event => event.type === 'tool/result').length === 1,
      orphanDoesNotChangeSurface: same(failure.surface.nodes, failureSurfaceBefore)
        && failure.surface.replaceGeneration === 0
        && failure.surface.nodes.includes(failureOriginal.seq)
        && same(messageFacts(failure.deriveMessages()), failureMessagesBefore),
      balancedOrphanGetsNoSyntheticRepair: failureRepairClosers.length === 0,
      replayKeepsOriginalAndExpiresClaim: failureReplay.events.length === failure.events.length + 1
        && same(failureReplay.surface.nodes, failureSurfaceBefore)
        && orphanArmedProjection.claim !== undefined
        && orphanExpiredProjection.claim === undefined
        && orphanExpiredProjection.deltaTokens === 0,
    }
    const failedChecks = Object.entries(checks)
      .filter(([, passed]) => !passed)
      .map(([name]) => name)
    requireCondition(failedChecks.length === 0, `tool-result pruner checks: ${failedChecks.join(', ')}`)

    return {
      configAndContent: {
        defaults: defaultConfig,
        activeConfig: pruner.config,
        marker: { text: PRUNE_MARKER, codePoints: codePointLength(PRUNE_MARKER) },
        defaultTriggeredOutputCodePoints: TOOL_RESULT_PRUNER_DEFAULTS.headChars
          + codePointLength(PRUNE_MARKER)
          + TOOL_RESULT_PRUNER_DEFAULTS.tailChars,
        invalidConfig: { outputBudget: invalidBudget, staleKey: staleConfig },
        exactThreshold: {
          inputCodePoints: pruner.measureContent(exactThresholdInput),
          output: exactThresholdOutput,
        },
        unicode: {
          inputCodePoints: pruner.measureContent(unicodeInput),
          outputCodePoints: pruner.measureContent(unicodeOutput ?? []),
          output: contentFacts(unicodeOutput ?? []),
        },
        richBlocks: {
          inputTextCodePoints: pruner.measureContent(richInput),
          outputTextCodePoints: pruner.measureContent(richOutput ?? []),
          output: contentFacts(richOutput ?? []),
        },
      },
      sessionPair: {
        before: {
          eventCount: successEventCountBefore,
          surfaceNodes: successSurfaceBefore,
          tokenMeasurement: successTokensBefore,
          modelMessages: successMessagesBefore,
          originalEvent: successOriginalBefore,
        },
        result: pruneResult,
        appendedEvents: successAdded,
        after: {
          eventCount: success.events.length,
          surfaceNodes: [...success.surface.nodes],
          replaceGeneration: success.surface.replaceGeneration,
          tokenMeasurement: successTokensAfter,
          modelMessages: successMessagesAfter,
          originalEvent: successOriginalAfter,
        },
        tokenSurfaceProjection: {
          armed: armedProjection,
          consumed: consumedProjection,
          expectedReplacementDelta: expectedProjectionDelta,
        },
        replay: {
          firstLiveSeq: successReplay.firstLiveSeq,
          surfaceNodes: [...successReplay.surface.nodes],
          modelMessages: successReplayMessages,
        },
        secondPass,
      },
      sourceDerivedReplacementFailure: {
        classification: 'source-derived-runtime-probe',
        officialTestPath: 'packages/compaction/compaction-tool-result-pruner/tests/tool-result-pruner.spec.ts',
        officialTestScope: 'the official test asserts rejection outside an open turn but does not separately assert the already-appended orphan compaction/prune marker',
        officialTestSeparatelyAssertsOrphanMarker: false,
        error: failureError,
        eventCountBefore: failureEventCountBefore,
        appendedEvents: failureAdded,
        surfaceNodesBefore: failureSurfaceBefore,
        surfaceNodesAfter: [...failure.surface.nodes],
        replaceGenerationAfter: failure.surface.replaceGeneration,
        syntheticRepairClosers: failureRepairClosers,
        replay: {
          firstLiveSeq: failureReplay.firstLiveSeq,
          finalEvent: recoveryBoundary,
          surfaceNodes: [...failureReplay.surface.nodes],
        },
        tokenSurfaceProjection: {
          armedByOrphan: orphanArmedProjection,
          expiredByRecoveryBoundary: orphanExpiredProjection,
        },
      },
      checks,
    }
  } finally {
    await ctx.fiber.dispose()
  }
}

function repairScenario(): unknown {
  const session = Session.create(SessionId('phase8-repair-open'))
  const notStarted = CallId('call-not-started')
  const outcomeUnknown = CallId('call-outcome-unknown')
  session.append('turn/start', { turn: 1 })
  session.append('step/start', { turn: 1, step: 1 })
  session.append('assistant/message', {
    turn: 1,
    step: 1,
    message: fixedAssistant('message-assistant-repair', [
      { type: 'tool-call', id: notStarted, name: 'read', arguments: '{"path":"a"}' },
      { type: 'tool-call', id: outcomeUnknown, name: 'bash', arguments: '{"command":"touch x"}' },
    ]),
  }, { surfaceOp: 'append' })
  const durableCall = session.append('tool/call', {
    turn: 1,
    step: 1,
    callId: outcomeUnknown,
    name: 'bash',
    arguments: '{"command":"touch x"}',
  })
  const closers = interruptedTurnClosers(session.events)
  const repairedSeed = [...session.events, ...closers]
  const repaired = Session.create(SessionId('phase8-repair-replay'), repairedSeed)

  const balanced = Session.create(SessionId('phase8-balanced-dangling'))
  balanced.append('turn/start', { turn: 1 })
  balanced.append('step/start', { turn: 1, step: 1 })
  balanced.append('assistant/message', {
    turn: 1,
    step: 1,
    message: fixedAssistant('message-assistant-balanced-dangling', [
      { type: 'tool-call', id: CallId('call-balanced-dangling'), name: 'read', arguments: '{}' },
    ]),
  }, { surfaceOp: 'append' })
  balanced.append('tool/call', {
    turn: 1,
    step: 1,
    callId: CallId('call-balanced-dangling'),
    name: 'read',
    arguments: '{}',
  })
  balanced.append('step/end', { turn: 1, step: 1 })
  balanced.append('turn/end', {
    turn: 1,
    reason: { kind: 'error', error: { message: 'executor failed', code: 'UNKNOWN' } },
  })
  const balancedClosers = interruptedTurnClosers(balanced.events)
  const resultEvents = closers.filter(event => event.type === 'tool/result')
  const checks = {
    twoResultsBeforeBoundaries: same(
      closers.map(event => event.type),
      ['tool/result', 'tool/result', 'step/end', 'turn/end'],
    ),
    resultOrderMatchesAssistantBlocks: resultEvents[0]?.type === 'tool/result'
      && resultEvents[0].data.message.source.callId === notStarted
      && resultEvents[1]?.type === 'tool/result'
      && resultEvents[1].data.message.source.callId === outcomeUnknown,
    riskCodesDiffer: resultEvents[0]?.type === 'tool/result'
      && resultEvents[0].data.error?.code === 'TOOL_NOT_STARTED'
      && resultEvents[1]?.type === 'tool/result'
      && resultEvents[1].data.error?.code === 'TOOL_OUTCOME_UNKNOWN',
    onlyStartedCallCitesIntent: resultEvents[0]?.sourceEventSeqs === undefined
      && resultEvents[1]?.sourceEventSeqs?.length === 1
      && resultEvents[1].sourceEventSeqs[0] === durableCall.seq,
    closerSeqsContiguous: closers.every((event, index) =>
      event.seq === session.events.length + index),
    closerTimesReuseLastRealTime: closers.every(event =>
      event.time === session.events.at(-1)?.time),
    repairedTranscriptReplayable: repaired.deriveMessages().length === 3,
    balancedClosedDanglingGetsNoRepair: balancedClosers.length === 0,
  }
  requireCondition(Object.values(checks).every(Boolean), 'repair checks')
  return {
    openTail: {
      originalEventTypes: session.events.map(event => event.type),
      durableCallSeq: durableCall.seq,
      syntheticClosers: closers,
      repairedEventTypes: repairedSeed.map(event => event.type),
      replayAddsEndSeedAtSeq: repaired.events.at(-1)?.seq,
      replayMessages: messageFacts(repaired.deriveMessages()),
    },
    balancedClosedDangling: {
      eventTypes: balanced.events.map(event => event.type),
      syntheticClosers: balancedClosers,
    },
    checks,
  }
}

function resumeScenario(): unknown {
  const source = Session.create(SessionId('phase8-resume'))
  source.append('turn/start', { turn: 1 })
  source.append('user/message', fixedUser('message-resume-user', 'persist me'), {
    surfaceOp: 'append',
  })
  source.append('turn/end', { turn: 1, reason: { kind: 'completed' } })
  const storedSeed = source.events
  const resumed = Session.create(SessionId('phase8-resume'), storedSeed)
  const firstResumeEvents = resumed.events
  const reopened = Session.create(SessionId('phase8-resume'), firstResumeEvents)
  const untouchedReopenEvents = reopened.events
  reopened.append('turn/start', { turn: 2 })
  reopened.append('turn/end', { turn: 2, reason: { kind: 'completed' } })
  const checks = {
    firstResumeAddsOneMarker: firstResumeEvents.length === storedSeed.length + 1
      && firstResumeEvents.at(-1)?.type === 'session/end-seed',
    markerSeqEqualsStoredLength: firstResumeEvents.at(-1)?.seq === storedSeed.length,
    untouchedReopenDoesNotAddMarker: untouchedReopenEvents.length === firstResumeEvents.length,
    nextAppendContinuesSeq: reopened.events.at(-2)?.seq === firstResumeEvents.length
      && reopened.events.at(-1)?.seq === firstResumeEvents.length + 1,
    messagesPreserved: same(source.deriveMessages(), reopened.deriveMessages()),
  }
  requireCondition(Object.values(checks).every(Boolean), 'resume checks')
  return {
    storedSeed: storedSeed.map(event => ({ seq: event.seq, type: event.type })),
    firstResume: {
      firstLiveSeq: resumed.firstLiveSeq,
      events: firstResumeEvents.map(event => ({ seq: event.seq, type: event.type })),
    },
    untouchedReopen: {
      firstLiveSeq: reopened.firstLiveSeq,
      events: untouchedReopenEvents.map(event => ({ seq: event.seq, type: event.type })),
    },
    afterContinuation: reopened.events.map(event => ({ seq: event.seq, type: event.type })),
    checks,
  }
}

function scannerScenario(format: JsonlFormatRuntime): unknown {
  const id = SessionId('phase8-jsonl-scan')
  const meta: SessionHeader = {
    version: 0,
    id,
    createdAt: FIXED_TIME,
    delegationDepth: 0,
  }
  const header = `${JSON.stringify(format.toHeaderLine(meta))}\n`
  const turnStart: SessionEvent = {
    type: 'turn/start', seq: 0, time: FIXED_TIME, data: { turn: 1 },
  }
  const turnEnd: SessionEvent = {
    type: 'turn/end', seq: 1, time: FIXED_TIME, data: {
      turn: 1,
      reason: { kind: 'completed' },
    },
  }
  const completeStart = `${JSON.stringify(turnStart)}\n`
  const tornLog = Buffer.from(header + completeStart + JSON.stringify(turnEnd))
  const torn = format.scanLog(tornLog)

  const gapTailLog = Buffer.from(header + completeStart + `${JSON.stringify({
    type: 'step/start', seq: 2, time: FIXED_TIME, data: { turn: 1, step: 1 },
  })}\n`)
  const gapTail = format.scanLog(gapTailLog)
  const gapCommitted = captureFailure(() => format.scanLog(Buffer.from(
    header + completeStart
      + `${JSON.stringify({ type: 'step/start', seq: 2, time: FIXED_TIME, data: { turn: 1, step: 1 } })}\n`
      + `${JSON.stringify({ ...turnEnd, seq: 3 })}\n`,
  )))
  const corruptTail = format.scanLog(Buffer.from(header + completeStart + '{not json\n'))
  const corruptCommitted = captureFailure(() => format.scanLog(Buffer.from(
    header + completeStart + '{not json\n' + `${JSON.stringify(turnEnd)}\n`,
  )))
  const newerVersion = captureFailure(() => format.scanLog(Buffer.from(
    `${JSON.stringify({ type: 'session', version: 99, id, futureOnly: true })}\n`,
  )))
  const olderVersion = captureFailure(() => format.scanLog(Buffer.from(
    `${JSON.stringify({ type: 'session', version: -1, id, legacyOnly: true })}\n`,
  )))
  const checks = {
    tornFinalRecordIgnored: same(torn.events.map(event => event.seq), [0]),
    committedBytesStopBeforeTornRecord: torn.committedBytes === Buffer.byteLength(header + completeStart),
    uncommittedGapPreservesPrefix: same(gapTail.events.map(event => event.seq), [0]),
    committedGapRejected: gapCommitted.name === 'Error'
      && gapCommitted.message.includes('seq gap in committed region'),
    corruptTailPreservesPrefix: same(corruptTail.events.map(event => event.seq), [0]),
    committedCorruptionRejected: corruptCommitted.name === 'Error'
      && corruptCommitted.message.includes('unparsable committed event'),
    newerVersionIsFormatRefusal: newerVersion.name === 'SessionFormatUnsupportedError'
      && newerVersion.message.includes('newer harness')
      && newerVersion.message.includes('upgrade the harness'),
    olderVersionIsFormatRefusal: olderVersion.name === 'SessionFormatUnsupportedError'
      && olderVersion.message.includes('older than the supported v0')
      && olderVersion.message.includes('no upgrade path'),
  }
  requireCondition(Object.values(checks).every(Boolean), 'JSONL scanner checks')
  return {
    headerLine: JSON.parse(header) as unknown,
    tornFinalRecord: {
      inputBytes: tornLog.length,
      committedBytes: torn.committedBytes,
      preservedSeqs: torn.events.map(event => event.seq),
    },
    uncommittedGap: {
      committedBytes: gapTail.committedBytes,
      preservedSeqs: gapTail.events.map(event => event.seq),
    },
    committedGap: gapCommitted,
    corruptTail: {
      committedBytes: corruptTail.committedBytes,
      preservedSeqs: corruptTail.events.map(event => event.seq),
    },
    committedCorruption: corruptCommitted,
    versionRefusals: { newer: newerVersion, older: olderVersion },
    checks,
  }
}

async function supportedEventProbe(ignorable: boolean): Promise<unknown> {
  const id = SessionId(ignorable ? 'phase8-unknown-ignorable' : 'phase8-unknown-required')
  const meta: SessionHeader = {
    version: 0,
    id,
    createdAt: FIXED_TIME,
    delegationDepth: 0,
  }
  let storedEvents: SessionEvent[] = [
    { type: 'turn/start', seq: 0, time: FIXED_TIME, data: { turn: 1 } },
    {
      type: 'turn/end',
      seq: 1,
      time: FIXED_TIME,
      data: { turn: 1, reason: { kind: 'completed' } },
    },
    {
      type: 'future/event',
      seq: 2,
      time: FIXED_TIME,
      data: { payload: 1 },
      ...(ignorable ? { ignorable: true as const } : {}),
    } as unknown as SessionEvent,
  ]
  let revision = SessionPersistenceRevision('phase8-oracle-revision-0')
  const backend: PersistenceBackend<string> = {
    name: 'phase8-oracle-memory-backend',
    loadStored: (_id) => Promise.resolve({
      meta: structuredClone(meta),
      events: structuredClone(storedEvents),
      revision,
    }),
    readStoredRevision: (_id) => Promise.resolve(revision),
    appendBatch: (_meta, events) => {
      storedEvents = [...storedEvents, ...structuredClone(events)]
      revision = SessionPersistenceRevision(`phase8-oracle-revision-${storedEvents.length}`)
      return Promise.resolve()
    },
    commitRepair: (_meta, _marker, closers) => {
      storedEvents = [...storedEvents, ...structuredClone(closers)]
      revision = SessionPersistenceRevision(`phase8-oracle-revision-${storedEvents.length}`)
      return Promise.resolve()
    },
    list: () => Promise.resolve([structuredClone(meta)]),
    locate: header => ({ kind: 'oracle-memory', path: `/oracle/${header.id}/session.jsonl` }),
  }
  const ctx = new Context()
  await ctx.plugin(SessionStore)
  const coordinator = new PersistenceCoordinator(ctx, backend)
  try {
    try {
      const loaded = await coordinator.load(id)
      return {
        outcome: 'LOADED',
        eventTypes: loaded.events.map(event => event.type),
        preservedUnknownEnvelope: loaded.events.at(-1),
      }
    } catch (error: unknown) {
      return { outcome: 'REFUSED', error: errorFact(error) }
    }
  } finally {
    await ctx.fiber.dispose()
  }
}

async function formatCompatibilityScenario(format: JsonlFormatRuntime): Promise<unknown> {
  const scanner = scannerScenario(format)
  const required = await supportedEventProbe(false)
  const ignorable = await supportedEventProbe(true)
  const requiredRecord = required as {
    outcome: string
    error?: { name: string; message: string }
  }
  const ignorableRecord = ignorable as {
    outcome: string
    eventTypes?: string[]
    preservedUnknownEnvelope?: SessionEvent
  }
  const checks = {
    unknownRequiredRefused: requiredRecord.outcome === 'REFUSED'
      && requiredRecord.error?.name === 'SessionFormatUnsupportedError'
      && requiredRecord.error.message.includes('not marked ignorable'),
    unknownIgnorablePreserved: ignorableRecord.outcome === 'LOADED'
      && ignorableRecord.eventTypes?.at(-1) === 'future/event'
      && ignorableRecord.preservedUnknownEnvelope?.ignorable === true,
  }
  requireCondition(Object.values(checks).every(Boolean), 'format compatibility checks')
  return { scanner, unknownEvents: { required, ignorable }, checks }
}

function assertNoFalseChecks(value: unknown, path = 'scenarios'): void {
  if (value === false) throw new Error(`Phase 8 oracle check failed: ${path}`)
  if (Array.isArray(value) || value === null || typeof value !== 'object') return
  for (const [key, child] of Object.entries(value)) {
    assertNoFalseChecks(child, `${path}.${key}`)
  }
}

function assertPinnedCleanUpstream(upstream: string): void {
  const actualCommit = execFileSync('git', ['rev-parse', 'HEAD'], {
    cwd: upstream,
    encoding: 'utf8',
  }).trim()
  if (actualCommit !== BASELINE_COMMIT) {
    throw new Error(`oracle requires upstream ${BASELINE_COMMIT}, got ${actualCommit}`)
  }
  const trackedChanges = execFileSync(
    'git',
    ['status', '--porcelain', '--untracked-files=no'],
    { cwd: upstream, encoding: 'utf8' },
  ).trim()
  if (trackedChanges !== '') {
    throw new Error('oracle requires a clean upstream tracked working tree')
  }
  for (const path of [...SOURCE_PATHS, ...TEST_PATHS]) {
    requireCondition(existsSync(join(upstream, path)), `missing cited upstream path: ${path}`)
  }
}

async function main(): Promise<void> {
  const upstream = process.cwd()
  assertPinnedCleanUpstream(upstream)
  const originalNow = Date.now
  Object.defineProperty(Date, 'now', {
    configurable: true,
    value: () => FIXED_TIME,
  })
  try {
    const jsonlFormat = await loadJsonlFormat(upstream)
    const tokenSurfaceProjection = await loadTokenSurfaceProjection(upstream)
    const scenarios = {
      longReasoningAndCompaction: await longReasoningAndCompaction(jsonlFormat),
      toolResultPruner: await toolResultPrunerScenario(tokenSurfaceProjection),
      interruptedToolRepair: repairScenario(),
      resumeEndSeed: resumeScenario(),
      jsonlAndFormatCompatibility: await formatCompatibilityScenario(jsonlFormat),
    }
    assertNoFalseChecks(Object.fromEntries(
      Object.entries(scenarios).map(([name, scenario]) => [
        name,
        (scenario as { checks?: unknown }).checks ?? scenario,
      ]),
    ))
    assertPinnedCleanUpstream(upstream)
    const output = {
      schemaVersion: 1,
      upstream: { repository: REPOSITORY, commit: BASELINE_COMMIT },
      evidence: {
        sourcePaths: SOURCE_PATHS,
        testPaths: TEST_PATHS,
        executionModes: {
          longReasoning: 'runtime upstream Session append plus lossless chunk-row encode/decode',
          compaction: 'runtime BasicCompactionEngine transaction with a fixed fake summarizer',
          toolResultPrunerContent: 'runtime ToolResultPruner config resolution, Unicode code-point measurement, and content transforms',
          toolResultPrunerSession: 'runtime ToolResultPruner surface replacement under the real Session invariant registry',
          toolResultPrunerFailure: 'source-derived runtime replacement-rejection probe; the cited official test asserts the rejection but not the orphan marker separately',
          tokenSurfaceProjection: 'runtime foldSurfaceProjection over the emitted adjacent shadow-price/replacement pair and orphan recovery boundary',
          repair: 'runtime interruptedTurnClosers plus Session replay',
          resume: 'runtime Session seed/end-seed construction and continued append',
          jsonl: 'runtime JSONL SessionLogScanner/scanLog over exact bytes',
          unknownEvents: 'runtime PersistenceCoordinator over a deterministic in-memory storage seam',
        },
      },
      deterministic: {
        fixedTimeMs: FIXED_TIME,
        randomIds: 'compaction UUID and generated checkpoint message id normalized by exact value',
        repeatedGenerationExpectation: 'byte-identical output',
      },
      safety: {
        networkAccess: 'none',
        credentialAccess: 'none',
        realModelCalls: 'none; compaction uses a fixed fake summarizer',
        filesystemWrites: 'explicit output path only',
      },
      scope: {
        summarySemanticFaithfulness: 'not claimed; the fake summary fixes transaction and replay behavior only',
        toolResultPrunerFailureCoverage: 'the orphan compaction/prune fact is a source-derived runtime probe, not a separately asserted official crash test',
        jsonlDurabilityMechanics: 'not claimed by this compact oracle; cited official JSONL tests cover fsync, rollback, and frame repair',
        rustCompatibility: 'not claimed; default Rust comparisons must consume this fixture separately',
      },
      scenarios,
    }
    const serialized = `${JSON.stringify(output, null, 2)}\n`
    requireCondition(!/[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/i.test(serialized), 'random UUID leaked')
    const outputPath = process.argv[2]
    if (outputPath === undefined) process.stdout.write(serialized)
    else writeFileSync(outputPath, serialized, 'utf8')
  } finally {
    Object.defineProperty(Date, 'now', { configurable: true, value: originalNow })
  }
}

void main().catch((error: unknown) => {
  process.stderr.write(`${error instanceof Error ? error.stack ?? error.message : String(error)}\n`)
  process.exitCode = 1
})
