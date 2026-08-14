#!/usr/bin/env node

/** Type-check the Phase 6 shell oracle against the pinned upstream source graph. */
import { execFileSync } from 'node:child_process'
import { createRequire } from 'node:module'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const BASELINE_COMMIT = '47f943859bef60e4160492346772ded9b24f765a'
const upstreamRoot = resolve(process.argv[2] ?? process.cwd())
const scriptDir = dirname(fileURLToPath(import.meta.url))
const oraclePath = join(scriptDir, 'generate-upstream-shell-fixtures.ts')

const actualCommit = execFileSync('git', ['rev-parse', 'HEAD'], {
  cwd: upstreamRoot,
  encoding: 'utf8',
}).trim()
if (actualCommit !== BASELINE_COMMIT) {
  throw new Error(`typecheck requires upstream ${BASELINE_COMMIT}, got ${actualCommit}`)
}

const workingTreeChanges = execFileSync('git', ['status', '--porcelain'], {
  cwd: upstreamRoot,
  encoding: 'utf8',
}).trim()
if (workingTreeChanges !== '') throw new Error('typecheck requires a clean upstream working tree')

const requireFromUpstream = createRequire(join(upstreamRoot, 'package.json'))
const ts = requireFromUpstream('typescript')
const configPath = join(upstreamRoot, 'tsconfig.base.json')
const loaded = ts.readConfigFile(configPath, ts.sys.readFile)
if (loaded.error) throw new Error(ts.formatDiagnostic(loaded.error, formatHost()))

const parsed = ts.parseJsonConfigFileContent(
  loaded.config,
  ts.sys,
  upstreamRoot,
  {
    composite: false,
    declaration: false,
    declarationMap: false,
    incremental: false,
    noEmit: true,
  },
  configPath,
)
if (parsed.errors.length > 0) throw new Error(ts.formatDiagnostics(parsed.errors, formatHost()))

const program = ts.createProgram({ rootNames: [oraclePath], options: parsed.options })
const oracleSource = program.getSourceFile(oraclePath)
if (!oracleSource) throw new Error(`TypeScript did not load ${oraclePath}`)

// The upstream monorepo has unrelated cross-package diagnostics under this
// ad-hoc program. This gate intentionally reports diagnostics attached to the
// oracle itself, including bad imports and misuse of upstream public types.
const diagnostics = [
  ...program.getSyntacticDiagnostics(oracleSource),
  ...program.getSemanticDiagnostics(oracleSource),
]
if (diagnostics.length > 0) {
  process.stderr.write(ts.formatDiagnosticsWithColorAndContext(diagnostics, formatHost()))
  process.exitCode = 1
} else {
  process.stdout.write('upstream Phase 6 shell oracle typecheck passed\n')
}

function formatHost() {
  return {
    getCanonicalFileName: fileName => fileName,
    getCurrentDirectory: () => upstreamRoot,
    getNewLine: () => '\n',
  }
}
