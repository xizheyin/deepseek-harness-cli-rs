/**
 * Deterministic Phase 2 behavior oracle for DeepSeek Harness.
 *
 * Run from the pinned upstream checkout with its locked tsx binary. This is a
 * maintainer evidence generator; default Rust tests consume only its checked-in output.
 */

import { AttachmentId } from '@deepseek-ai/dsh-attachment'
import { Context } from '@deepseek-ai/cordis'
import InvariantRegistry from '@deepseek-ai/dsh-invariants'
import { execFileSync } from 'node:child_process'
import {
  BlockAssembler,
  CallId,
  MessageId,
  ReasoningEffortId,
} from '@deepseek-ai/dsh-llm'
import type {
  GenerateOptions,
  Message,
  StreamChunk,
} from '@deepseek-ai/dsh-llm'
import * as LlmInvariant from '@deepseek-ai/dsh-llm/invariant'
import { serializeRequest } from '@deepseek-ai/dsh-llm-deepseek/src/serialize'
import { DeepSeekAdapter } from '@deepseek-ai/dsh-llm-deepseek/src/adapter'
import { resolveAdapterOptions } from '@deepseek-ai/dsh-llm-deepseek/src/index'
import { DONE, parseSse } from '@deepseek-ai/dsh-llm-deepseek/src/sse'
import { translate } from '@deepseek-ai/dsh-llm-deepseek/src/translate'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'

interface NormalizedError {
  name: string
  message: string
  code?: string
  packageName?: string
  status?: number
  requestId?: string
}

type Observation<T> =
  | { ok: true; value: T }
  | { ok: false; error: NormalizedError }

interface StreamObservation {
  ok: boolean
  chunks: StreamChunk[]
  error?: NormalizedError
}

interface SseObservation {
  ok: boolean
  values: string[]
  comments: string[]
  error?: NormalizedError
}

function normalizedError(error: unknown): NormalizedError {
  if (!(error instanceof Error)) {
    return { name: typeof error, message: String(error) }
  }
  const facts = error as Error & {
    code?: unknown
    packageName?: unknown
    status?: unknown
    requestId?: unknown
  }
  return {
    name: error.name,
    message: error.message,
    ...typeof facts.code === 'string' ? { code: facts.code } : {},
    ...typeof facts.packageName === 'string' ? { packageName: facts.packageName } : {},
    ...typeof facts.status === 'number' ? { status: facts.status } : {},
    ...typeof facts.requestId === 'string' ? { requestId: facts.requestId } : {},
  }
}

function observeSync<T>(run: () => T): Observation<T> {
  try {
    return { ok: true, value: run() }
  } catch (error) {
    return { ok: false, error: normalizedError(error) }
  }
}

async function observeStream(stream: AsyncIterable<StreamChunk>): Promise<StreamObservation> {
  const chunks: StreamChunk[] = []
  try {
    for await (const chunk of stream) chunks.push(chunk)
    return { ok: true, chunks }
  } catch (error) {
    return { ok: false, chunks, error: normalizedError(error) }
  }
}

async function* payloads(...items: readonly (string | object)[]): AsyncGenerator<string> {
  for (const item of items) {
    yield typeof item === 'string' ? item : JSON.stringify(item)
  }
}

function bytes(...fragments: readonly string[]): ReadableStream<Uint8Array<ArrayBuffer>> {
  const encoder = new TextEncoder()
  return new ReadableStream({
    start(controller) {
      for (const fragment of fragments) controller.enqueue(encoder.encode(fragment))
      controller.close()
    },
  })
}

async function observeSse(
  fragments: readonly string[],
): Promise<SseObservation> {
  const comments: string[] = []
  const values: string[] = []
  try {
    for await (const value of parseSse(bytes(...fragments), comment => comments.push(comment))) {
      values.push(value)
    }
    return { ok: true, values, comments }
  } catch (error) {
    return { ok: false, values, comments, error: normalizedError(error) }
  }
}

function request(overrides: Partial<GenerateOptions> = {}): GenerateOptions {
  return {
    provider: 'deepseek-official',
    model: 'deepseek-v4-flash',
    messages: [],
    ...overrides,
  }
}

function assembled(chunks: readonly StreamChunk[]): {
  blocks: ReturnType<BlockAssembler['blocks']>
  usage: ReturnType<typeof nullableUsage>
  finish: BlockAssembler['finish']
} {
  const assembler = new BlockAssembler()
  for (const chunk of chunks) assembler.push(chunk)
  return {
    blocks: assembler.blocks(),
    usage: nullableUsage(assembler),
    finish: assembler.finish,
  }
}

function nullableUsage(assembler: BlockAssembler): BlockAssembler['usage'] | null {
  return assembler.usage ?? null
}

const history: Message[] = [
  {
    id: MessageId('message-user'),
    role: 'user',
    content: [{ type: 'text', text: 'weather in Paris?' }],
    source: { kind: 'user' },
  },
  {
    id: MessageId('message-assistant'),
    role: 'assistant',
    content: [
      { type: 'reasoning', text: 'I should inspect two sources.' },
      { type: 'tool-call', id: CallId('call-weather'), name: 'weather', arguments: '{"city":"Paris"}' },
      { type: 'tool-call', id: CallId('call-clock'), name: 'clock', arguments: '{"zone":"Europe/Paris"}' },
    ],
    source: { kind: 'model', provider: 'deepseek-official', model: 'deepseek-v4-flash' },
  },
  {
    id: MessageId('message-tool'),
    role: 'user',
    content: [
      { type: 'text', text: 'trusted note' },
      {
        type: 'tool-result',
        toolCallId: CallId('call-weather'),
        content: [{ type: 'text', text: 'sunny' }],
      },
      {
        type: 'tool-result',
        toolCallId: CallId('call-clock'),
        content: [],
      },
    ],
    source: { kind: 'plugin', plugin: 'phase2-oracle' },
  },
]

const serialize = {
  fullRequest: observeSync(() => serializeRequest(request({
    messages: history,
    system: 'Be concise.',
    tools: [{
      name: 'weather',
      description: 'Read weather',
      parameters: {
        type: 'object',
        properties: { city: { type: 'string' } },
        required: ['city'],
      },
    }],
    temperature: 0.2,
    maxTokens: 128,
    stop: ['END'],
    reasoningEffort: ReasoningEffortId('max'),
  }), { thinking: 'enabled', reasoningEffort: 'high' })),
  sessionTitleForcesThinkingOff: observeSync(() => serializeRequest(request({
    messages: history.slice(0, 1),
    purpose: 'session-title',
    reasoningEffort: ReasoningEffortId('max'),
  }), { thinking: 'enabled', reasoningEffort: 'max' })),
  unsupportedEffort: observeSync(() => serializeRequest(request({
    reasoningEffort: ReasoningEffortId('medium'),
  }))),
  deploymentLockedThinkingOff: observeSync(() => serializeRequest(request({
    reasoningEffort: ReasoningEffortId('high'),
  }), { thinking: 'disabled' })),
  imageRejected: observeSync(() => serializeRequest(request({
    messages: [{
      id: MessageId('message-image'),
      role: 'user',
      content: [{
        type: 'image',
        attachment: {
          attachmentId: AttachmentId(`sha256:${'a'.repeat(64)}`),
          mediaType: 'image/png',
          bytes: 68,
          width: 1,
          height: 1,
        },
      }],
      source: { kind: 'user' },
    }],
  }))),
}

const firstChunk = {
  choices: [{ delta: { role: 'assistant', content: null, reasoning_content: '' } }],
}

async function main(): Promise<void> {
const actualCommit = execFileSync('git', ['rev-parse', 'HEAD'], {
  cwd: process.cwd(),
  encoding: 'utf8',
}).trim()
if (actualCommit !== BASELINE_COMMIT) {
  throw new Error(`oracle requires upstream ${BASELINE_COMMIT}, got ${actualCommit}`)
}
const trackedChanges = execFileSync('git', [
  'status',
  '--porcelain',
  '--untracked-files=no',
], {
  cwd: process.cwd(),
  encoding: 'utf8',
}).trim()
if (trackedChanges !== '') {
  throw new Error('oracle requires a clean upstream tracked working tree')
}

const interleavedTranslation = await observeStream(translate(payloads(
  firstChunk,
  { choices: [{ delta: { reasoning_content: 'plan ' } }] },
  {
    choices: [{
      delta: {
        reasoning_content: 'first',
        content: 'Checking. ',
        tool_calls: [
          { index: 7, id: 'call-a', type: 'function', function: { name: 'one', arguments: '{"x"' } },
          { index: 3, id: 'call-b', type: 'function', function: { name: 'two', arguments: '' } },
        ],
      },
    }],
  },
  {
    choices: [{
      delta: {
        content: 'Done.',
        tool_calls: [
          { index: 3, function: { arguments: '{"y":2}' } },
          { index: 7, function: { arguments: ':1}' } },
        ],
      },
    }],
  },
  {
    choices: [{ delta: {}, finish_reason: 'tool_calls' }],
    usage: { prompt_tokens: 100, completion_tokens: 20, prompt_cache_hit_tokens: 60 },
  },
  {
    choices: [],
    usage: {
      prompt_tokens: 110,
      completion_tokens: 21,
      prompt_tokens_details: { cached_tokens: 80 },
      completion_tokens_details: { reasoning_tokens: 5 },
    },
  },
  DONE,
)))

const sseCases = {
  fragmentedUtf8BomCrLfCommentAndMultiData: await observeSse([
    '\ufeff: keep-alive\r',
    '\ndata: {"text":"你',
    '好"}\r\ndata: second\r\n\r\ndata: [DO',
    'NE]\r\n\r\n',
  ]),
  stopsAtDoneInSameRead: await observeSse([
    'data: [DONE]\n\ndata: {"late":true}\n\n',
  ]),
  emptyEof: await observeSse([]),
  unterminatedDone: await observeSse(['data: [DONE]']),
}

const defaultConnection = resolveAdapterOptions({})
const configuredConnection = resolveAdapterOptions({
  maxTokens: 4_096,
  defaultContextWindow: 256_000,
  models: [
    { id: 'inherits-default' },
    { id: 'exact-override', name: 'Exact Override', contextWindow: 64_000, maxTokens: 512 },
  ],
  retryPolicy: {
    mode: 'normal',
    maxRetries: 3,
    retryableCodes: ['RATE_LIMIT', 'SERVER'],
    backoff: { initialDelayMs: 25, maxDelayMs: 100, jitterRatio: 0.2 },
  },
})
const configuredAdapter = new DeepSeekAdapter({
  options: () => configuredConnection,
  resolveApiKey: async () => 'oracle-key-never-used',
  resolveUserId: () => 'oracle-user-never-used' as never,
})
const prepareFacts = {
  defaults: {
    maxTokens: defaultConnection.maxTokens,
    defaultContextWindow: defaultConnection.defaultContextWindow,
    models: defaultConnection.models,
    retryPolicy: defaultConnection.retryPolicy,
  },
  configured: {
    retryPolicy: configuredConnection.retryPolicy,
    inherited: await configuredAdapter.resolveModel('deepseek-official', 'inherits-default'),
    exact: await configuredAdapter.resolveModel('deepseek-official', 'exact-override'),
    unlisted: await configuredAdapter.resolveModel('deepseek-official', 'unlisted-pass-through'),
  },
  invalid: {
    duplicateModel: observeSync(() => resolveAdapterOptions({ models: [{ id: 'dup' }, { id: 'dup' }] })),
    zeroModelLimit: observeSync(() => resolveAdapterOptions({ models: [{ id: 'bad', maxTokens: 0 }] })),
    invalidRetry: observeSync(() => resolveAdapterOptions({
      retryPolicy: { mode: 'normal', retryableCodes: [] },
    })),
  },
}

const translateCases = {
  interleavedReasoningTextAndTools: {
    ...interleavedTranslation,
    ...interleavedTranslation.ok ? { assembled: assembled(interleavedTranslation.chunks) } : {},
  },
  malformedJson: await observeStream(translate(payloads('{bad json'))),
  streamClosedBeforeDone: await observeStream(translate(payloads(
    firstChunk,
    { choices: [{ delta: { content: 'partial' } }] },
  ))),
  completedWithoutContent: await observeStream(translate(payloads(
    firstChunk,
    { choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 7, completion_tokens: 0 } },
    DONE,
  ))),
  unknownFinishReason: await observeStream(translate(payloads(
    firstChunk,
    { choices: [{ delta: { content: 'filtered' } }] },
    { choices: [{ delta: {}, finish_reason: 'content_filter' }] },
    DONE,
  ))),
}

async function setupInvariantContext(): Promise<Context> {
  const ctx = new Context()
  await ctx.plugin(InvariantRegistry)
  await ctx.plugin(LlmInvariant)
  return ctx
}

async function* source(chunks: readonly StreamChunk[]): AsyncGenerator<StreamChunk> {
  yield* chunks
}

async function observeInvariant(
  ctx: Context,
  chunks: readonly StreamChunk[],
): Promise<StreamObservation> {
  const options: GenerateOptions = {
    provider: 'phase2-oracle',
    model: 'fixed-model',
    messages: [],
  }
  return observeStream(ctx.waterfall(
    ctx as never,
    'llm/stream',
    options,
    () => source(chunks),
  ))
}

const invariantContext = await setupInvariantContext()
const finishStop: StreamChunk = { type: 'finish', reason: { kind: 'stop' } }
const usage: StreamChunk = { type: 'usage', usage: { inputTokens: 1, outputTokens: 1 } }
const finishError: StreamChunk = {
  type: 'finish',
  reason: {
    kind: 'error',
    failure: { message: 'provider failed', code: 'TRANSPORT' },
  },
}

const indexReuseChunks: StreamChunk[] = [
  { type: 'block-start', index: 0, blockType: 'text' },
  { type: 'text-delta', index: 0, text: 'first' },
  { type: 'block-end', index: 0, block: { type: 'text', text: 'first' } },
  { type: 'block-start', index: 0, blockType: 'reasoning' },
  { type: 'reasoning-delta', index: 0, text: 'second' },
  { type: 'block-end', index: 0, block: { type: 'reasoning', text: 'second' } },
  finishStop,
]

const wholeStreamInvariant = {
  translatedStreamAccepted: await observeInvariant(invariantContext, interleavedTranslation.chunks),
  negativeIndexRejected: await observeInvariant(invariantContext, [
    { type: 'block-start', index: -1, blockType: 'text' },
    finishStop,
  ]),
  unsafeIntegerIndexRejected: await observeInvariant(invariantContext, [
    { type: 'block-start', index: Number.MAX_SAFE_INTEGER + 1, blockType: 'text' },
    finishStop,
  ]),
  deltaWithoutOpenRejected: await observeInvariant(invariantContext, [
    { type: 'text-delta', index: 0, text: 'orphan' },
  ]),
  wrongDeltaTypeRejected: await observeInvariant(invariantContext, [
    { type: 'block-start', index: 0, blockType: 'reasoning' },
    { type: 'text-delta', index: 0, text: 'wrong' },
  ]),
  mismatchedCloseRejected: await observeInvariant(invariantContext, [
    { type: 'block-start', index: 0, blockType: 'text' },
    { type: 'block-end', index: 0, block: { type: 'reasoning', text: '' } },
  ]),
  duplicateUsageRejected: await observeInvariant(invariantContext, [usage, usage, finishStop]),
  openBlockAtSuccessfulFinishRejected: await observeInvariant(invariantContext, [
    { type: 'block-start', index: 0, blockType: 'text' },
    finishStop,
  ]),
  openBlockAtErrorFinishAccepted: await observeInvariant(invariantContext, [
    { type: 'block-start', index: 0, blockType: 'text' },
    finishError,
  ]),
  chunkAfterFinishRejected: await observeInvariant(invariantContext, [finishStop, usage]),
  missingFinishRejected: await observeInvariant(invariantContext, []),
  closedIndexMayBeReused: {
    invariant: await observeInvariant(invariantContext, indexReuseChunks),
    assembler: assembled(indexReuseChunks),
  },
}

const deltaOnlyAssembler = new BlockAssembler()
for (const chunk of [
  { type: 'text-delta', index: 0, text: 'kept' },
  {
    type: 'tool-call-delta',
    index: 1,
    id: CallId('call-drop'),
    name: 'unsafe-partial',
    argumentsDelta: '{',
  },
  { type: 'finish', reason: { kind: 'max-tokens' } },
] satisfies StreamChunk[]) {
  deltaOnlyAssembler.push(chunk)
}

const firstCloseAssembler = new BlockAssembler()
for (const chunk of [
  { type: 'block-end', index: 0, block: { type: 'reasoning', text: 'first' } },
  { type: 'block-end', index: 0, block: { type: 'text', text: 'second' } },
] satisfies StreamChunk[]) {
  firstCloseAssembler.push(chunk)
}

const rogueAssembler = new BlockAssembler()

const assemblerCases = {
  deltaOnlyAndMaxTokensDropsToolCall: {
    blocks: deltaOnlyAssembler.blocks(),
    usage: nullableUsage(deltaOnlyAssembler),
    finish: deltaOnlyAssembler.finish,
  },
  firstCloseWins: {
    blocks: firstCloseAssembler.blocks(),
    usage: nullableUsage(firstCloseAssembler),
    finish: firstCloseAssembler.finish,
  },
  unknownChunkRejected: observeSync(() => {
    rogueAssembler.push({ type: 'rogue-chunk' } as unknown as StreamChunk)
    return rogueAssembler.blocks()
  }),
}

const output = {
  schemaVersion: 1,
  upstream: {
    repository: 'https://github.com/deepseek-ai/deepseek-harness',
    commit: BASELINE_COMMIT,
  },
  evidence: {
    sourcePaths: [
      'packages/llm/llm-deepseek/src/serialize.ts',
      'packages/llm/llm-deepseek/src/index.ts',
      'packages/llm/llm-deepseek/src/adapter.ts',
      'packages/llm/llm-deepseek/src/translate.ts',
      'packages/llm/llm-deepseek/src/sse.ts',
      'packages/llm/llm/src/invariant.ts',
      'packages/llm/llm/src/assembler.ts',
      'packages/llm/llm/src/retry-policy.ts',
    ],
    testPaths: [
      'packages/llm/llm-deepseek/tests/serialize.spec.ts',
      'packages/llm/llm-deepseek/tests/adapter.spec.ts',
      'packages/llm/llm/tests/service.spec.ts',
      'packages/llm/llm-deepseek/tests/translate.spec.ts',
      'packages/llm/llm/tests/invariant.spec.ts',
      'packages/llm/llm/tests/assembler.spec.ts',
    ],
  },
  serialize,
  prepare: prepareFacts,
  sse: sseCases,
  translate: translateCases,
  wholeStreamInvariant,
  assembler: assemblerCases,
}

  process.stdout.write(`${JSON.stringify(output, null, 2)}\n`)
}

void main().catch((error: unknown) => {
  process.stderr.write(`${JSON.stringify(normalizedError(error))}\n`)
  process.exitCode = 1
})
