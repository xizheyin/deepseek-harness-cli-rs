/**
 * Deterministic Phase 4 read-only-tool oracle for DeepSeek Harness.
 * Run from the pinned upstream checkout with its locked tsx binary.
 */

import { execFileSync } from 'node:child_process'
import { writeFileSync } from 'node:fs'
import {
  mkdir,
  mkdtemp,
  realpath,
  rm,
  symlink,
  utimes,
  writeFile,
} from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { Context } from '@deepseek-ai/cordis'
import LocalFileSystem from '@deepseek-ai/dsh-fs-local'
import { CallId } from '@deepseek-ai/dsh-llm'
import LocalSubprocessRuntime from '@deepseek-ai/dsh-subprocess-local'
import SystemPrompt from '@deepseek-ai/dsh-system-prompt'
import ToolRuntime from '@deepseek-ai/dsh-tools'
import type {
  ToolExecutionResult,
  ToolExecutionSuccess,
} from '@deepseek-ai/dsh-tools'
import * as ToolFs from '@deepseek-ai/dsh-tool-fs'
import * as ToolFsSearch from '@deepseek-ai/dsh-tool-fs-search'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'
const REPOSITORY = 'https://github.com/deepseek-ai/deepseek-harness'
const MODEL_TOOL_NAMES = ['read', 'glob', 'grep'] as const

const SOURCE_PATHS = [
  'packages/core/tools/src/index.ts',
  'packages/core/tools/src/schema.ts',
  'packages/fs/fs/src/index.ts',
  'packages/fs/fs/src/types.ts',
  'packages/fs/fs-local/src/index.ts',
  'packages/fs/fs-local/src/fsio.ts',
  'packages/fs/tool-fs/README.md',
  'packages/fs/tool-fs/src/index.ts',
  'packages/fs/tool-fs/src/read.ts',
  'packages/fs/tool-fs/src/read-target.ts',
  'packages/fs/tool-fs/src/read-render.ts',
  'packages/fs/tool-fs/src/session-cwd.ts',
  'packages/fs/tool-fs-search/src/index.ts',
  'packages/fs/tool-fs-search/src/glob.ts',
  'packages/fs/tool-fs-search/src/grep.ts',
  'packages/fs/tool-fs-search/src/search-core.ts',
  'packages/fs/tool-fs-search/src/presentation.ts',
  'packages/fs/tool-fs-search/src/direct-call.ts',
] as const

const TEST_PATHS = [
  'packages/core/tools/tests/schema.spec.ts',
  'packages/core/tools/tests/tools.spec.ts',
  'packages/fs/fs-local/tests/filesystem.spec.ts',
  'packages/fs/fs-local/tests/fsio.spec.ts',
  'packages/fs/tool-fs/tests/error.spec.ts',
  'packages/fs/tool-fs/tests/integration.spec.ts',
  'packages/fs/tool-fs/tests/read-render.spec.ts',
  'packages/fs/tool-fs/tests/tools.spec.ts',
  'packages/fs/tool-fs-search/tests/integration.spec.ts',
  'packages/fs/tool-fs-search/tests/presentation.spec.ts',
  'packages/fs/tool-fs-search/tests/tools.spec.ts',
] as const

const SHIPPED_CLI_CONFIG_PATHS = [
  'apps/cli/config/agent-presets/standard/agent.cordis.yml',
  'apps/cli/config/agent-presets/cordis/agent.cordis.yml',
  'apps/cli/config/agent-presets/code/agent.cordis.yml',
  'packages/bundle/base/cordis.patch.yml',
] as const

interface WorkspaceFixture {
  root: string
  workspace: string
  outside: string
  fixedMtimes: Record<string, string>
}

interface ToolCaller {
  call(name: string, args: unknown): Promise<ToolExecutionResult>
  callIds(): string[]
}

function requireCondition(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(`oracle assertion failed: ${message}`)
}

function expectSuccess(result: ToolExecutionResult, label: string): ToolExecutionSuccess {
  if (result.isError) {
    throw new Error(`${label} unexpectedly failed: ${result.error.message}`)
  }
  return result
}

function compareAscii(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0
}

function replaceEvery(value: string, search: string, replacement: string): string {
  return value.split(search).join(replacement)
}

function normalizePaths(value: unknown, fixture: WorkspaceFixture): unknown {
  if (typeof value === 'string') {
    return replaceEvery(
      replaceEvery(value, fixture.workspace, '<workspace>'),
      fixture.outside,
      '<outside>',
    )
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

function makeCaller(ctx: Context, workspace: string): ToolCaller {
  let ordinal = 0
  const ids: string[] = []
  return {
    async call(name: string, args: unknown): Promise<ToolExecutionResult> {
      ordinal += 1
      const id = `phase4-${String(ordinal).padStart(2, '0')}`
      ids.push(id)
      return ctx.tools.execute({
        signal: new AbortController().signal,
        callId: CallId(id),
        name,
        arguments: args,
        agent: {
          session: {
            header: { id: 'phase4-oracle', cwd: workspace },
          },
        } as never,
      })
    },
    callIds(): string[] {
      return [...ids]
    },
  }
}

async function createWorkspace(): Promise<WorkspaceFixture> {
  // macOS exposes the temporary directory through a /var -> /private/var
  // symlink. Start from its real path so LocalFileSystem's canonical display
  // path can be replaced exactly rather than leaking a host-specific prefix.
  const canonicalTmp = await realpath(tmpdir())
  const root = await mkdtemp(join(canonicalTmp, 'dsh-phase4-oracle-'))
  const workspace = join(root, 'workspace')
  const outside = join(root, 'outside')
  const src = join(workspace, 'src')
  const listDir = join(workspace, 'list-dir')

  await mkdir(src, { recursive: true })
  await mkdir(join(workspace, '.git'), { recursive: true })
  await mkdir(join(listDir, 'folder'), { recursive: true })
  await mkdir(outside, { recursive: true })

  await writeFile(join(workspace, 'read.txt'), 'alpha\r\nbeta\nthird\n')
  await writeFile(join(workspace, 'empty.txt'), '')
  await writeFile(join(src, 'old.ts'), 'const oldValue = true\n')
  await writeFile(join(src, 'new.ts'), 'needle first\r\nneutral\nneedle second\n')
  await writeFile(join(workspace, '.hidden.ts'), 'const hiddenValue = true\n')
  await writeFile(join(workspace, 'ignored.ts'), 'const ignoredValue = true\n')
  await writeFile(join(workspace, '.gitignore'), 'ignored.ts\n')
  await writeFile(join(workspace, '.git', 'config.ts'), 'const vcsInternal = true\n')

  await writeFile(join(listDir, 'alpha.txt'), 'alpha')
  await writeFile(join(listDir, 'zeta.txt'), 'zeta')
  await symlink('alpha.txt', join(listDir, 'linked-alpha'), 'file')
  await symlink('missing-target', join(listDir, 'broken-link'), 'file')

  await writeFile(join(outside, 'outside.txt'), 'outside sentinel\n')
  await writeFile(join(outside, 'outside.ts'), 'outsideNeedle\n')
  await symlink(join(outside, 'outside.txt'), join(workspace, 'outside-link.txt'), 'file')

  const fixedMtimes = {
    'src/old.ts': '2000-01-01T00:00:00.000Z',
    'src/new.ts': '2001-01-01T00:00:00.000Z',
    '.hidden.ts': '2002-01-01T00:00:00.000Z',
    'ignored.ts': '2003-01-01T00:00:00.000Z',
    '.git/config.ts': '1999-01-01T00:00:00.000Z',
  }
  for (const [path, timestamp] of Object.entries(fixedMtimes)) {
    const date = new Date(timestamp)
    await utimes(join(workspace, path), date, date)
  }

  return { root, workspace, outside, fixedMtimes }
}

async function boot(workspace: string): Promise<Context> {
  const ctx = new Context()
  await ctx.plugin(SystemPrompt)
  await ctx.plugin(ToolRuntime)
  await ctx.plugin(LocalFileSystem, { cwd: workspace })
  await ctx.plugin(ToolFs)
  await ctx.plugin(LocalSubprocessRuntime)
  await ctx.plugin(ToolFsSearch, { sampleOverCapGlobResults: false })
  return ctx
}

function schemaSurface(ctx: Context) {
  const advertised = ctx.tools.schemas()
  const tools = Object.fromEntries(MODEL_TOOL_NAMES.map((name) => {
    const modelSchema = advertised.find(schema => schema.name === name)
    const definition = ctx.tools.get(name)
    requireCondition(modelSchema !== undefined, `${name} model schema must exist`)
    requireCondition(definition !== undefined, `${name} definition must exist`)
    return [name, {
      description: modelSchema.description,
      parameters: structuredClone(modelSchema.parameters),
      output: structuredClone(definition.output.schema),
      timeoutMs: definition.timeoutMs,
    }]
  }))
  const registeredNames = advertised.map(schema => schema.name).sort(compareAscii)
  const modelFacingListPresent = ctx.tools.get('list') !== undefined
    || registeredNames.includes('list')
  return {
    registeredNames,
    modelFacingListPresent,
    tools,
    checks: {
      canonicalSchemasPresent: MODEL_TOOL_NAMES.every(name => name in tools),
      listAbsent: !modelFacingListPresent,
      exactRegisteredNames: JSON.stringify(registeredNames)
        === JSON.stringify(['edit', 'glob', 'grep', 'read', 'write']),
    },
  }
}

async function listDirPrimitive(ctx: Context, fixture: WorkspaceFixture) {
  const target = await ctx.fs.resolve('list-dir', { cwd: fixture.workspace })
  const entries = (await ctx.fs.listDir(target)).map(entry => ({
    name: entry.name,
    type: entry.type,
    targetDisplayPath: normalizePaths(entry.target.displayPath, fixture),
    ...(entry.size === undefined ? {} : { size: entry.size }),
  }))
  const names = entries.map(entry => entry.name)
  return {
    modelFacingTool: false,
    providerMethod: 'ctx.fs.listDir',
    input: { path: 'list-dir' },
    targetDisplayPath: normalizePaths(target.displayPath, fixture),
    entries,
    checks: {
      oneLevelOnly: !names.includes('folder/anything'),
      sortedByName: JSON.stringify(names)
        === JSON.stringify(['alpha.txt', 'broken-link', 'folder', 'linked-alpha', 'zeta.txt']),
      followsWorkingFileSymlink: entries.some(entry =>
        entry.name === 'linked-alpha' && entry.type === 'file' && entry.size === 5),
      preservesBrokenSymlinkAsOther: entries.some(entry =>
        entry.name === 'broken-link' && entry.type === 'other' && !('size' in entry)),
    },
  }
}

async function canonicalScenarios(caller: ToolCaller, fixture: WorkspaceFixture) {
  const readFull = await caller.call('read', { file_path: 'read.txt' })
  const readWindow = await caller.call('read', { file_path: 'read.txt', offset: 2, limit: 1 })
  const readEmpty = await caller.call('read', { file_path: 'empty.txt' })
  const readMissing = await caller.call('read', { file_path: 'missing.txt' })

  const globMatching = await caller.call('glob', { pattern: '**/*.ts' })
  const globNoMatches = await caller.call('glob', { pattern: '*.nomatch' })
  const globValue = expectSuccess(globMatching, 'canonical glob').value as {
    paths: string[]
  }
  const expectedGlobPaths = [
    join('src', 'old.ts'),
    join('src', 'new.ts'),
    '.hidden.ts',
    'ignored.ts',
  ]

  const grepMatching = await caller.call('grep', {
    pattern: 'needle',
    path: join('src', 'new.ts'),
  })
  const grepNoMatches = await caller.call('grep', {
    pattern: 'absent-pattern',
    path: join('src', 'new.ts'),
  })
  const grepValue = expectSuccess(grepMatching, 'canonical grep').value as {
    matches: Array<{ path: string; lineNumber: number; line: string }>
  }

  const readFullValue = expectSuccess(readFull, 'full read').value as {
    lines: Array<{ number: number; text: string }>
    totalLines: number
  }
  const readWindowValue = expectSuccess(readWindow, 'windowed read').value as {
    lines: Array<{ number: number; text: string }>
    totalLines: number
  }
  expectSuccess(readEmpty, 'empty read')
  requireCondition(readMissing.isError, 'missing read must be an error')

  return {
    read: {
      inputs: {
        full: { file_path: 'read.txt' },
        window: { file_path: 'read.txt', offset: 2, limit: 1 },
        empty: { file_path: 'empty.txt' },
        missing: { file_path: 'missing.txt' },
      },
      full: normalizeResult(readFull, fixture),
      window: normalizeResult(readWindow, fixture),
      empty: normalizeResult(readEmpty, fixture),
      missing: normalizeResult(readMissing, fixture),
      checks: {
        crlfNormalized: JSON.stringify(readFullValue.lines)
          === JSON.stringify([
            { number: 1, text: 'alpha' },
            { number: 2, text: 'beta' },
            { number: 3, text: 'third' },
          ]),
        trailingNewlineDoesNotAddLine: readFullValue.totalLines === 3,
        offsetIsOneBased: JSON.stringify(readWindowValue.lines)
          === JSON.stringify([{ number: 2, text: 'beta' }]),
        missingUsesTypedFailure: readMissing.isError
          && readMissing.error.info?.code === 'FS_NOT_FOUND',
      },
    },
    glob: {
      inputs: {
        matching: { pattern: '**/*.ts' },
        noMatches: { pattern: '*.nomatch' },
      },
      matching: normalizeResult(globMatching, fixture),
      noMatches: normalizeResult(globNoMatches, fixture),
      checks: {
        modificationTimeAscending: JSON.stringify(globValue.paths)
          === JSON.stringify(expectedGlobPaths),
        hiddenIncluded: globValue.paths.includes('.hidden.ts'),
        gitignoreIgnored: globValue.paths.includes('ignored.ts'),
        vcsInternalsExcluded: !globValue.paths.includes(join('.git', 'config.ts')),
        shippedCliHeadMode: true,
      },
    },
    grep: {
      inputs: {
        matching: { pattern: 'needle', path: join('src', 'new.ts') },
        noMatches: { pattern: 'absent-pattern', path: join('src', 'new.ts') },
      },
      matching: normalizeResult(grepMatching, fixture),
      noMatches: normalizeResult(grepNoMatches, fixture),
      checks: {
        groupedMatchesKeepLineNumbers: JSON.stringify(grepValue.matches)
          === JSON.stringify([
            { path: join('src', 'new.ts'), lineNumber: 1, line: 'needle first' },
            { path: join('src', 'new.ts'), lineNumber: 3, line: 'needle second' },
          ]),
      },
    },
  }
}

async function ambientReadAcceptance(caller: ToolCaller, fixture: WorkspaceFixture) {
  const parentTraversalRead = await caller.call('read', {
    file_path: join('..', 'outside', 'outside.txt'),
  })
  const symlinkRead = await caller.call('read', { file_path: 'outside-link.txt' })
  const parentTraversalGlob = await caller.call('glob', {
    pattern: '*.ts',
    path: join('..', 'outside'),
  })
  const parentTraversalGrep = await caller.call('grep', {
    pattern: 'outsideNeedle',
    path: join('..', 'outside', 'outside.ts'),
  })

  const parentReadValue = expectSuccess(parentTraversalRead, 'parent traversal read').value as {
    lines: Array<{ number: number; text: string }>
  }
  const symlinkReadValue = expectSuccess(symlinkRead, 'external symlink read').value as {
    lines: Array<{ number: number; text: string }>
  }
  const outsideGlobValue = expectSuccess(parentTraversalGlob, 'parent traversal glob').value as {
    paths: string[]
  }
  const outsideGrepValue = expectSuccess(parentTraversalGrep, 'parent traversal grep').value as {
    matches: Array<{ path: string; lineNumber: number; line: string }>
  }

  return {
    parentTraversal: {
      inputKind: '../ outside workspace',
      inputs: {
        read: { file_path: join('..', 'outside', 'outside.txt') },
        glob: { pattern: '*.ts', path: join('..', 'outside') },
        grep: {
          pattern: 'outsideNeedle',
          path: join('..', 'outside', 'outside.ts'),
        },
      },
      read: normalizeResult(parentTraversalRead, fixture),
      glob: normalizeResult(parentTraversalGlob, fixture),
      grep: normalizeResult(parentTraversalGrep, fixture),
      outcome: 'accepted',
      checks: {
        readReachedOutside: JSON.stringify(parentReadValue.lines)
          === JSON.stringify([{ number: 1, text: 'outside sentinel' }]),
        globReachedOutside: JSON.stringify(outsideGlobValue.paths)
          === JSON.stringify([join('..', 'outside', 'outside.ts')]),
        grepReachedOutside: JSON.stringify(outsideGrepValue.matches)
          === JSON.stringify([{
            path: join('..', 'outside', 'outside.ts'),
            lineNumber: 1,
            line: 'outsideNeedle',
          }]),
      },
    },
    symlink: {
      inputKind: 'workspace symlink whose target is outside workspace',
      input: { file_path: 'outside-link.txt' },
      fixtureLink: {
        path: '<workspace>/outside-link.txt',
        target: '<outside>/outside.txt',
      },
      read: normalizeResult(symlinkRead, fixture),
      outcome: 'accepted',
      checks: {
        readFollowedExternalTarget: JSON.stringify(symlinkReadValue.lines)
          === JSON.stringify([{ number: 1, text: 'outside sentinel' }]),
      },
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
  const ctx = await boot(fixture.workspace)
  try {
    const caller = makeCaller(ctx, fixture.workspace)
    const surface = schemaSurface(ctx)
    const list = await listDirPrimitive(ctx, fixture)
    const canonical = await canonicalScenarios(caller, fixture)
    const ambient = await ambientReadAcceptance(caller, fixture)
    const checks = {
      schemaSurface: surface.checks,
      listDirPrimitive: list.checks,
      canonicalRead: canonical.read.checks,
      canonicalGlob: canonical.glob.checks,
      canonicalGrep: canonical.grep.checks,
      parentTraversal: ambient.parentTraversal.checks,
      symlink: ambient.symlink.checks,
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
        shippedCliConfigPaths: SHIPPED_CLI_CONFIG_PATHS,
      },
      config: {
        shippedCli: {
          sampleOverCapGlobResults: false,
          implication: 'over-cap glob retains the modification-time-ordered head',
        },
        readDefaults: {
          limit: 2_000,
          maxLineLength: 2_000,
          maxBytes: 50 * 1024,
          streamMinSize: 10 * 1024 * 1024,
        },
        searchDefaults: {
          globMaxResults: ToolFsSearch.GLOB_MAX_RESULTS,
          grepMaxMatches: ToolFsSearch.GREP_MAX_MATCHES,
          grepMaxLineBytes: ToolFsSearch.GREP_MAX_LINE_BYTES,
          searchMetaMaxBytes: ToolFsSearch.SEARCH_META_MAX_BYTES,
          rawOutputMaxBytes: ToolFsSearch.RAW_OUTPUT_MAX_BYTES,
          graceMs: ToolFsSearch.SEARCH_GRACE_MS,
          stderrMaxBytes: ToolFsSearch.SEARCH_STDERR_MAX_BYTES,
          timeoutMs: ToolFsSearch.SEARCH_TIMEOUT_MS,
        },
      },
      schemaSurface: surface,
      listDirPrimitive: list,
      canonical,
      ambientReadAcceptance: ambient,
      deterministic: {
        freshTemporaryWorkspace: true,
        tempPathsNormalized: true,
        fixedMtimes: fixture.fixedMtimes,
        callIds: caller.callIds(),
      },
      safety: {
        networkAccess: 'none',
        credentialAccess: 'none',
        subprocess: 'only the packaged @vscode/ripgrep binary via dsh-subprocess-local',
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

  const fixture = await createWorkspace()
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
