/**
 * Maintainer-only Phase 1 compatibility oracle.
 *
 * Run this file with the pinned upstream checkout's `tsx` and tsconfig. It is
 * not a production dependency and default Rust tests consume only its checked-in output.
 */
import { execFileSync } from 'node:child_process'
import { Context } from '@deepseek-ai/cordis'
import { AttachmentId } from '@deepseek-ai/dsh-attachment'
import type { ImageAttachmentRef } from '@deepseek-ai/dsh-attachment'
import {
  CallId,
  callConfigEquals,
  freezeMessage,
  isTokenDelta,
  LlmError,
  MessageId,
  ProviderRequestId,
  ReasoningEffortId,
} from '@deepseek-ai/dsh-llm'
import type {
  FinishReason,
  LlmCallConfig,
  LlmCallConfigAdapterDefaults,
  Message,
  StreamChunk,
  TokenUsage,
  ToolSchema,
} from '@deepseek-ai/dsh-llm'
import SessionStore, {
  foldRequestHeader,
  SESSION_FORMAT_VERSION,
  Session,
  SessionId,
} from '@deepseek-ai/dsh-session'
import type { SessionHeader } from '@deepseek-ai/dsh-session'
import * as SessionInvariant from '@deepseek-ai/dsh-session/invariant'
import InvariantRegistry from '@deepseek-ai/dsh-invariants'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'
const FIXED_TIME = 1_700_000_000_000
const originalNow = Date.now
Date.now = () => FIXED_TIME

function errorText(error: unknown): string {
  return error instanceof Error
    ? `${error.constructor.name}: ${error.message}`
    : String(error)
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

  const ctx = new Context()
  await ctx.plugin(SessionStore)
  await ctx.plugin(InvariantRegistry)
  await ctx.plugin(SessionInvariant)

  try {
    // Canonical successful trace used as the basic cross-language fixture.
    const session = ctx.sessions.create(SessionId('oracle-session'))
    session.append('turn/start', { turn: 1 })
    session.append('user/message', freezeMessage({
      id: MessageId('message-user-1'),
      role: 'user',
      content: [{ type: 'text', text: 'echo hello' }],
      source: { kind: 'user' },
    }), { surfaceOp: 'append' })
    session.append('step/start', { turn: 1, step: 1 })
    session.append('assistant/chunk', {
      turn: 1,
      step: 1,
      chunk: { type: 'text-delta', index: 0, text: 'hello' },
    })
    session.append('assistant/message', {
      turn: 1,
      step: 1,
      message: freezeMessage({
        id: MessageId('message-assistant-1'),
        role: 'assistant',
        content: [
          { type: 'text', text: 'hello' },
          { type: 'tool-call', id: CallId('call-1'), name: 'echo', arguments: '{"text":"hello"}' },
        ],
        source: { kind: 'model', provider: 'mock', model: 'mock-model' },
      }),
    }, { surfaceOp: 'append', sourceEventSeqs: [3] })
    session.append('tool/call', {
      turn: 1,
      step: 1,
      callId: CallId('call-1'),
      name: 'echo',
      arguments: '{"text":"hello"}',
    })
    session.append('tool/result', {
      turn: 1,
      step: 1,
      message: freezeMessage({
        id: MessageId('message-tool-result-1'),
        role: 'user',
        content: [{
          type: 'tool-result',
          toolCallId: CallId('call-1'),
          content: [{ type: 'text', text: 'hello' }],
          isError: false,
        }],
        source: { kind: 'tool', callId: CallId('call-1') },
      }),
    }, { surfaceOp: 'append', sourceEventSeqs: [5] })
    session.append('step/end', { turn: 1, step: 1 })
    session.append('turn/end', { turn: 1, reason: { kind: 'completed' } })

    // Keep the exact failure text and prove a failed append is atomic.
    const illegal: Record<string, string> = {}
    const attempt = (name: string, action: () => void): void => {
      try {
        action()
        illegal[name] = 'ACCEPTED'
      } catch (error: unknown) {
        illegal[name] = errorText(error)
      }
    }
    attempt('turn_skip', () => {
      session.append('turn/start', { turn: 3 })
    })
    attempt('surface_missing_shadowed_source', () => {
      session.append('user/message', freezeMessage({
        id: MessageId('message-replacement-1'),
        role: 'user',
        content: [{ type: 'text', text: 'summary' }],
        source: { kind: 'user' },
      }), {
        surfaceOp: { op: 'replace', start: 1, end: 4 },
        sourceEventSeqs: [1],
      })
    })

    const orphan = ctx.sessions.create(SessionId('oracle-orphan'))
    orphan.append('turn/start', { turn: 1 })
    orphan.append('step/start', { turn: 1, step: 1 })
    attempt('orphan_tool_result', () => {
      orphan.append('tool/result', {
        turn: 1,
        step: 1,
        message: freezeMessage({
          id: MessageId('message-ghost-result'),
          role: 'user',
          content: [{
            type: 'tool-result',
            toolCallId: CallId('ghost'),
            content: [],
            isError: false,
          }],
          source: { kind: 'tool', callId: CallId('ghost') },
        }),
      }, { surfaceOp: 'append' })
    })
    attempt('turn_end_with_open_step', () => {
      orphan.append('turn/end', { turn: 1, reason: { kind: 'completed' } })
    })

    // SessionHeader accepts, snapshots, freezes, and preserves JSON extension fields.
    const extendedHeaderId = SessionId('oracle-header-extra')
    const suppliedExtendedHeader = {
      version: SESSION_FORMAT_VERSION,
      id: extendedHeaderId,
      createdAt: FIXED_TIME,
      cwd: '/tmp/oracle-workspace',
      pluginHeader: { owner: 'oracle-plugin', flags: [true, null] },
      explicitNull: null,
    } as unknown as SessionHeader
    const extendedHeaderSession = Session.create(
      extendedHeaderId,
      undefined,
      suppliedExtendedHeader,
    )

    // Known event payloads keep extra fields. Message source/block vocabulary is
    // merge-extensible, so replay validates only the core message shell.
    const extensionSession = ctx.sessions.create(SessionId('oracle-extensions'))
    extensionSession.append('turn/start', {
      turn: 1,
      pluginPayload: { nested: [1, null, { ok: true }] },
      explicitNull: null,
    } as never)
    const appendUntyped = extensionSession.append.bind(extensionSession) as (
      type: string,
      data: unknown,
      opts?: unknown,
    ) => unknown
    appendUntyped('user/message', {
      id: 'message-plugin-1',
      role: 'user',
      content: [{
        type: 'oracle/plugin-block',
        value: { answer: 42, explicitNull: null },
        blockExtra: ['preserved'],
      }],
      source: {
        kind: 'oracle/plugin-source',
        plugin: 'oracle-plugin',
        sourceExtra: { preserved: true },
      },
      messageExtra: { preserved: true, explicitNull: null },
    }, { surfaceOp: 'append' })
    extensionSession.append('turn/end', {
      turn: 1,
      reason: { kind: 'completed' },
      payloadExtra: 'preserved',
    } as never)
    const extensionReplay = Session.create(
      SessionId('oracle-extensions-replay'),
      structuredClone(extensionSession.events),
    )

    // Request projections are latest-full-snapshot folds. Missing optional fields
    // in a later snapshot clear old values; request/header also canonicalizes empties.
    const projectionSession = ctx.sessions.create(SessionId('oracle-projections'))
    const headerTimeline: Array<{ stage: string; value: unknown }> = [{
      stage: 'before-any-header',
      value: projectionSession.requestHeader() ?? null,
    }]
    const contextTimeline: Array<{ stage: string; value: unknown }> = [{
      stage: 'before-any-context',
      value: projectionSession.requestContext() ?? null,
    }]
    projectionSession.append('turn/start', { turn: 1 })
    projectionSession.append('request/header', {
      header: {
        config: { provider: 'mock', model: 'initial' },
        adapterDefaults: {},
        system: '',
        tools: [],
        headerExtension: 'raw-only',
      },
      reason: 'initial',
      payloadExtension: { preservedInEvent: true },
    } as never)
    headerTimeline.push({ stage: 'canonical-empty-optionals', value: projectionSession.requestHeader() })
    projectionSession.append('request/context', {
      provider: 'mock',
      model: 'initial',
      contextWindow: 128_000,
      oldExtension: 'must-disappear',
    } as never)
    contextTimeline.push({ stage: 'with-capacity', value: projectionSession.requestContext() })
    projectionSession.append('request/header', {
      header: {
        config: { provider: 'mock', model: 'populated', maxTokens: 4_096 },
        adapterDefaults: { maxTokens: true },
        system: 'You are an oracle.',
        tools: [{
          name: 'echo',
          description: 'Echo text',
          parameters: {
            type: 'object',
            properties: { text: { type: 'string' } },
            required: ['text'],
          },
        }],
      },
      reason: 'change',
    })
    headerTimeline.push({ stage: 'populated', value: projectionSession.requestHeader() })
    projectionSession.append('request/context', {
      provider: 'mock',
      model: 'populated',
      contextWindow: 256_000,
    })
    contextTimeline.push({ stage: 'updated-capacity', value: projectionSession.requestContext() })
    projectionSession.append('todo/write', { todos: [] })
    projectionSession.append('request/header', {
      header: {
        config: { provider: 'mock', model: 'cleared' },
        adapterDefaults: {},
        system: '',
        tools: [],
      },
      reason: 'change',
    })
    headerTimeline.push({ stage: 'omitted-fields-cleared', value: projectionSession.requestHeader() })
    projectionSession.append('request/context', {
      provider: 'mock',
      model: 'cleared',
      newExtension: 'kept-by-context-fold',
      explicitNull: null,
    } as never)
    contextTimeline.push({ stage: 'omitted-capacity-cleared', value: projectionSession.requestContext() })
    projectionSession.append('turn/end', { turn: 1, reason: { kind: 'completed' } })

    // JavaScript has one Number value for JSON 1 and 1.0. The surface rewrite
    // comparison therefore accepts these as equal while changing only result content.
    const parsedOnePointZero = (JSON.parse('{"score":1.0}') as { score: number }).score
    const rewriteSession = ctx.sessions.create(SessionId('oracle-numeric-rewrite'))
    rewriteSession.append('turn/start', { turn: 1 })
    rewriteSession.append('step/start', { turn: 1, step: 1 })
    rewriteSession.append('tool/call', {
      turn: 1,
      step: 1,
      callId: CallId('numeric-call'),
      name: 'echo',
      arguments: '{}',
    })
    const originalResult = rewriteSession.append('tool/result', {
      turn: 1,
      step: 1,
      message: freezeMessage({
        id: MessageId('message-numeric-result'),
        role: 'user',
        content: [{
          type: 'tool-result',
          toolCallId: CallId('numeric-call'),
          content: [{ type: 'text', text: 'full output' }],
          isError: false,
        }],
        source: { kind: 'tool', callId: CallId('numeric-call') },
      }),
      meta: { score: 1 },
    }, { surfaceOp: 'append', sourceEventSeqs: [2] })
    rewriteSession.append('step/end', { turn: 1, step: 1 })
    rewriteSession.append('turn/end', { turn: 1, reason: { kind: 'completed' } })
    rewriteSession.append('turn/start', { turn: 2 })
    let numericRewriteOutcome: string
    try {
      const originalBlock = originalResult.data.message.content[0]
      rewriteSession.append('tool/result', {
        ...originalResult.data,
        message: freezeMessage({
          ...originalResult.data.message,
          content: [{
            ...originalBlock,
            content: [{ type: 'text', text: 'pruned output' }],
          }],
        }),
        meta: { score: parsedOnePointZero },
      }, {
        surfaceOp: { op: 'replace', start: originalResult.seq, end: originalResult.seq },
        sourceEventSeqs: [originalResult.seq],
      })
      numericRewriteOutcome = 'ACCEPTED'
    } catch (error: unknown) {
      numericRewriteOutcome = errorText(error)
    }
    rewriteSession.append('turn/end', { turn: 2, reason: { kind: 'completed' } })

    // Exercise the complete Phase 1 provider-neutral wire vocabulary through
    // upstream-owned constructors/helpers or type-checked records. Rust tests
    // consume these exact JSON values instead of relying on self-round-trips.
    const detailedFailure = new LlmError('rate limited', 'RATE_LIMIT', {
      status: 429,
      providerRetryAfterMs: 1_250.5,
      requestId: ProviderRequestId('request-oracle-1'),
    }).failure
    const imageAttachment = {
      attachmentId: AttachmentId('attachment-oracle-1'),
      mediaType: 'image/png',
      bytes: 67,
      width: 1,
      height: 1,
      name: 'pixel.png',
    } satisfies ImageAttachmentRef
    const vocabularyMessages = [
      freezeMessage({
        id: MessageId('message-system-1'),
        role: 'system',
        content: [{ type: 'text', text: 'system text' }],
        source: { kind: 'plugin', plugin: 'system-prompt', form: 'instructions' },
      }),
      freezeMessage({
        id: MessageId('message-plugin-snapshot-1'),
        role: 'user',
        content: [{ type: 'text', text: 'snapshot text' }],
        source: {
          kind: 'plugin',
          plugin: 'workspace-context',
          form: 'snapshot',
          sections: [{ name: 'AGENTS.md', text: 'instructions' }],
        },
      }),
      freezeMessage({
        id: MessageId('message-plugin-notice-1'),
        role: 'user',
        content: [{ type: 'text', text: 'notice text' }],
        source: {
          kind: 'plugin',
          plugin: 'jobs',
          form: 'notice',
          summary: 'job finished',
        },
      }),
      freezeMessage({
        id: MessageId('message-all-blocks-1'),
        role: 'assistant',
        content: [
          { type: 'text', text: 'visible text' },
          { type: 'reasoning', text: 'private reasoning' },
          { type: 'image', attachment: imageAttachment },
          {
            type: 'tool-call',
            id: CallId('call-vocabulary-1'),
            name: 'read',
            arguments: '{"path":"src/lib.rs"}',
          },
        ],
        source: {
          kind: 'model',
          provider: 'mock',
          model: 'mock-model',
          replayState: { cursor: 7, explicitNull: null },
        },
      }),
      freezeMessage({
        id: MessageId('message-result-vocabulary-1'),
        role: 'user',
        content: [{
          type: 'tool-result',
          toolCallId: CallId('call-vocabulary-1'),
          content: [
            { type: 'text', text: 'file contents' },
            { type: 'image', attachment: imageAttachment },
          ],
          isError: true,
        }],
        source: { kind: 'tool', callId: CallId('call-vocabulary-1') },
      }),
    ] satisfies Message[]
    const tokenUsage = {
      inputTokens: 11,
      outputTokens: 7,
      cacheReadTokens: 5,
      cacheWriteTokens: 3,
      reasoningTokens: 2,
      usageExtension: null,
    } as TokenUsage
    const stopFinishReason = { kind: 'stop' } satisfies FinishReason
    const finishReasons = [
      stopFinishReason,
      { kind: 'tool-calls' },
      { kind: 'max-tokens' },
      { kind: 'aborted', failure: detailedFailure },
      { kind: 'error', failure: detailedFailure },
      { kind: 'oracle/future-finish', detail: null } as never,
    ] satisfies FinishReason[]
    const streamChunks = [
      { type: 'block-start', index: 0, blockType: 'text' },
      { type: 'text-delta', index: 0, text: 'visible' },
      { type: 'block-end', index: 0, block: { type: 'text', text: 'visible' } },
      { type: 'block-start', index: 1, blockType: 'reasoning' },
      { type: 'reasoning-delta', index: 1, text: 'reasoning' },
      { type: 'block-end', index: 1, block: { type: 'reasoning', text: 'reasoning' } },
      { type: 'block-start', index: 2, blockType: 'tool-call' },
      {
        type: 'tool-call-delta',
        index: 2,
        id: CallId('call-vocabulary-1'),
        name: 'read',
        argumentsDelta: '{"path":',
      },
      {
        type: 'block-end',
        index: 2,
        block: {
          type: 'tool-call',
          id: CallId('call-vocabulary-1'),
          name: 'read',
          arguments: '{"path":"src/lib.rs"}',
        },
      },
      { type: 'usage', usage: tokenUsage },
      { type: 'finish', reason: stopFinishReason, replayState: { cursor: 8, explicitNull: null } },
    ] satisfies StreamChunk[]
    const callConfig = {
      provider: 'mock',
      model: 'mock-model',
      reasoningEffort: ReasoningEffortId('high'),
      temperature: 0.25,
      maxTokens: 4_096,
      stop: ['END', 'STOP'],
    } satisfies LlmCallConfig
    const adapterDefaults = {
      reasoningEffort: true,
      maxTokens: true,
    } satisfies LlmCallConfigAdapterDefaults
    const toolSchema = {
      name: 'read',
      description: 'Read one file',
      parameters: {
        type: 'object',
        properties: { path: { type: 'string' } },
        required: ['path'],
        additionalProperties: false,
      },
    } satisfies ToolSchema

    // Current session readers retain forward-compatible values even when the
    // static TypeScript vocabulary has not yet named them.
    const forwardSession = ctx.sessions.create(SessionId('oracle-forward-values'))
    const forwardAppend = forwardSession.append.bind(forwardSession) as (
      type: string,
      data: unknown,
      opts?: unknown,
    ) => unknown
    forwardSession.append('turn/start', { turn: 1 })
    forwardAppend('request/header', {
      header: { config: { provider: 'mock', model: 'model' } },
      reason: 'future-reason',
    })
    forwardAppend('request/context', {
      provider: 'mock',
      model: 'model',
      contextWindow: null,
      pluginFact: null,
    })
    forwardSession.append('step/start', { turn: 1, step: 1 })
    forwardSession.append('tool/call', {
      turn: 1,
      step: 1,
      callId: CallId('call-forward-1'),
      name: 'read',
      arguments: '{}',
    })
    forwardAppend('tool/result', {
      turn: 1,
      step: 1,
      message: freezeMessage({
        id: MessageId('message-forward-result-1'),
        role: 'user',
        content: [{
          type: 'tool-result',
          toolCallId: CallId('call-forward-1'),
          content: [],
          isError: null,
        } as never],
        source: { kind: 'tool', callId: CallId('call-forward-1') },
      }),
      error: null,
      meta: null,
    }, { surfaceOp: 'append' })
    forwardSession.append('step/end', { turn: 1, step: 1 })
    forwardAppend('turn/end', {
      turn: 1,
      reason: { kind: 'future-end', detail: null },
    })

    // The raw upstream Session validates only the minimum request-header shell.
    // Rust deliberately rejects these malformed current-vocabulary fields so a
    // later provider never receives a half-typed config or tool schema.
    const permissiveSession = ctx.sessions.create(SessionId('oracle-permissive-known-payload'))
    permissiveSession.append('turn/start', { turn: 1 })
    const permissiveAppend = permissiveSession.append.bind(permissiveSession) as (
      type: string,
      data: unknown,
    ) => unknown
    let permissiveKnownPayloadOutcome: string
    try {
      permissiveAppend('request/header', {
        header: {
          config: {
            provider: 'mock',
            model: 'model',
            maxTokens: null,
            stop: [1],
          },
          tools: [{ name: 'incomplete-tool' }],
        },
        reason: 'initial',
      })
      permissiveKnownPayloadOutcome = 'ACCEPTED'
    } catch (error: unknown) {
      permissiveKnownPayloadOutcome = errorText(error)
    }

    const unsafeIntegerInput = '{"value":9007199254740993}'
    const unsafeIntegerParsed = JSON.parse(unsafeIntegerInput) as { value: number }
    const objectOrderInput = '{"z":1,"a":2}'
    const objectOrderParsed = JSON.parse(objectOrderInput) as Record<string, number>

    // The relational invariant is a separately installed Cordis companion in
    // upstream. A bare SessionStore accepts this malformed custom-producer trace.
    const bareCtx = new Context()
    await bareCtx.plugin(SessionStore)
    let bareInvariantOutcome: string
    let bareInvariantLength = 0
    let bareUnknownOutcome = 'NOT_RUN'
    let bareUnknownEvents: readonly unknown[] = []
    try {
      const bareSession = bareCtx.sessions.create(SessionId('oracle-no-invariant'))
      try {
        bareSession.append('turn/start', { turn: 2 })
        bareInvariantOutcome = 'ACCEPTED'
      } catch (error: unknown) {
        bareInvariantOutcome = errorText(error)
      }
      bareInvariantLength = bareSession.events.length

      const bareUnknownSession = bareCtx.sessions.create(SessionId('oracle-unknown-required'))
      const bareUnknownAppend = bareUnknownSession.append.bind(bareUnknownSession) as (
        type: string,
        data: unknown,
      ) => unknown
      try {
        bareUnknownAppend('plugin/required', { fact: 1 })
        bareUnknownOutcome = 'ACCEPTED'
      } catch (error: unknown) {
        bareUnknownOutcome = errorText(error)
      }
      bareUnknownEvents = bareUnknownSession.events
    } finally {
      await bareCtx.fiber.dispose()
    }

    const projectionHeaderEvents = projectionSession.events.filter(event => event.type === 'request/header')
    const projectionContextEvents = projectionSession.events.filter(event => event.type === 'request/context')
    console.log(JSON.stringify({
      fixture: {
        baselineRepository: 'https://github.com/deepseek-ai/deepseek-harness',
        baselineCommit: actualCommit,
        fixedTime: FIXED_TIME,
        evidencePaths: [
          'packages/llm/llm/src/brand.ts',
          'packages/llm/llm/src/message.ts',
          'packages/llm/llm/src/types.ts',
          'packages/llm/llm/src/call-config.ts',
          'packages/attachment/attachment/src/types.ts',
          'packages/core/session/src/index.ts',
          'packages/core/session/src/request-header.ts',
          'packages/core/session/src/surface.ts',
          'packages/core/session/src/invariant.ts',
          'packages/core/session/tests/session.spec.ts',
          'packages/core/session/tests/request-header.spec.ts',
          'packages/core/session/tests/surface.spec.ts',
          'packages/core/session/tests/invariant.spec.ts',
        ],
      },
      canonicalTrace: {
        header: session.header,
        events: session.events,
        surfaceNodes: session.surface.nodes,
        derivedMessages: session.deriveMessages(),
      },
      preservation: {
        suppliedSessionHeader: suppliedExtendedHeader,
        storedSessionHeader: extendedHeaderSession.header,
        headerWasDetached: extendedHeaderSession.header !== suppliedExtendedHeader,
        storedHeaderFrozen: Object.isFrozen(extendedHeaderSession.header),
        appendedEvents: extensionSession.events,
        replayedSeedEvents: extensionReplay.events.slice(0, extensionSession.events.length),
        replayedDerivedMessages: extensionReplay.deriveMessages(),
      },
      projections: {
        headerTimeline,
        contextTimeline,
        rawHeaderEvents: projectionHeaderEvents,
        rawContextEvents: projectionContextEvents,
        finalLiveHeader: projectionSession.requestHeader(),
        finalOfflineHeader: foldRequestHeader(projectionSession.events),
        finalContext: projectionSession.requestContext(),
        finalHeaderFrozen: Object.isFrozen(projectionSession.requestHeader()),
        finalContextFrozen: Object.isFrozen(projectionSession.requestContext()),
      },
      numericToolResultRewrite: {
        inputJsonLexemes: ['1', '1.0'],
        parsedNumbersAreStrictlyEqual: parsedOnePointZero === 1,
        outcome: numericRewriteOutcome,
        events: rewriteSession.events,
        surfaceNodes: rewriteSession.surface.nodes,
        derivedMessages: rewriteSession.deriveMessages(),
      },
      modelVocabulary: {
        messages: vocabularyMessages,
        failure: detailedFailure,
        finishReasons,
        tokenUsage,
        streamChunks,
        tokenDeltaDecisions: streamChunks.map(chunk => isTokenDelta(chunk)),
        callConfig,
        adapterDefaults,
        toolSchema,
        callConfigEquality: {
          identical: callConfigEquals(callConfig, structuredClone(callConfig)),
          changedStopOrder: callConfigEquals(callConfig, {
            ...callConfig,
            stop: [...callConfig.stop].reverse(),
          }),
        },
      },
      forwardCompatibility: {
        events: forwardSession.events,
        derivedMessages: forwardSession.deriveMessages(),
        requestHeader: forwardSession.requestHeader(),
        requestContext: forwardSession.requestContext(),
      },
      knownPayloadAdmission: {
        upstreamOutcome: permissiveKnownPayloadOutcome,
        events: permissiveSession.events,
      },
      numericBoundary: {
        inputJson: unsafeIntegerInput,
        parsedValue: unsafeIntegerParsed.value,
        reencodedJson: JSON.stringify(unsafeIntegerParsed),
      },
      objectOrderBoundary: {
        inputJson: objectOrderInput,
        upstreamReencodedJson: JSON.stringify(objectOrderParsed),
      },
      invariantRegistration: {
        upstreamWithoutCompanion: {
          attemptedEvent: { type: 'turn/start', data: { turn: 2 } },
          outcome: bareInvariantOutcome,
          committedLength: bareInvariantLength,
        },
        upstreamBareCoreUnknownRequired: {
          outcome: bareUnknownOutcome,
          events: bareUnknownEvents,
        },
      },
      illegal: {
        outcomes: illegal,
        atomicLengths: {
          valid: session.events.length,
          orphan: orphan.events.length,
        },
      },
    }, null, 2))
  } finally {
    await ctx.fiber.dispose()
  }
}

void main().finally(() => {
  Date.now = originalNow
})
