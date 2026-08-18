# Phase 4 read-only tools design

This document records the implemented Phase 4 contract. The goal is a small,
auditable set of model-callable tools that can inspect one
workspace without writing files, spawning subprocesses, or reading outside that
workspace.

## Scope and non-goals

Phase 4 adds:

- one immutable registry for `list`, `glob`, `grep`, and `read`;
- strict argument validation and stable model-facing failures;
- a capability-confined workspace root shared by all four tools;
- bounded, deterministic output and cooperative cancellation;
- integration with the existing `ToolExecutor`, which already guarantees that
  the durable `tool/call` intention precedes tool execution and that a correlated
  `tool/result` follows an ordinary success or failure.

Phase 4 does not add writes, edits, shell commands, subprocess search, approval,
interactive UI, persistent spill files, or an operating-system sandbox. Those
remain in Phases 5–8. The `dsh` binary still does not start an interactive agent;
the production path in this phase is the public Rust Agent Loop wired to the real
read-only registry.

## Upstream behavior used as the reference

The semantic baseline remains DeepSeek Harness commit
`47f943859bef60e4160492346772ded9b24f765a`.

- `read` accepts `{file_path, offset?, limit?}`, reads UTF-8 regular files,
  returns at most 2,000 lines, truncates long lines, and renders the selected
  page inside `<path>`, `<type>`, and `<content>` markers.
- `glob` accepts `{pattern, path?}`, returns files (not directories), includes
  hidden and ignored files, excludes version-control metadata, sorts by oldest
  modification time, and inlines at most 100 paths in the shipped CLI profile.
- `grep` accepts `{pattern, path?, include?}`, uses regular expressions, groups
  matches by file, previews at most 2,000 bytes per matching line, and inlines at
  most 250 matches.
- the upstream tool runtime validates and snapshots arguments, runs policy and
  body hooks, normalizes the result, and leaves durable call/result publication
  to the Agent Loop.

The pinned upstream has no model-facing `list` tool. It has an internal,
single-directory `FileSystem.listDir` service only. The Rust `list` tool is a
deliberate product extension required by this repository's roadmap.

The upstream read side is also not workspace-confined: absolute paths, `..`, and
followed symlinks can reach outside the session cwd, while its filesystem
sandbox protects writes only. Rust rejects those reads. This is a deliberate
security difference, not an upstream-compatible behavior claim.

## Ownership and public API

The new public owner is conceptually:

```rust,ignore
pub struct ReadOnlyToolRegistry { /* immutable workspace capability + schemas */ }

impl ReadOnlyToolRegistry {
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, ToolRegistryBuildError>;
    pub fn schemas(&self) -> &[ToolSchema];
    pub fn workspace(&self) -> &Path;
}

impl ToolExecutor for ReadOnlyToolRegistry { /* dispatch the four closed tools */ }
```

`open` is the only ambient filesystem-authority entry point. It first opens one
existing directory as a capability, making that handle the authorization
linearization point. It then canonicalizes the display path and verifies that its
device/inode identity still matches the opened handle. The normalized startup and
canonical aliases are retained only for display and inside-root absolute-input
translation. Tool arguments cannot replace the root.

Production assembly must take the schema snapshot and executor from the same
registry instance:

```rust,ignore
let tools = Arc::new(ReadOnlyToolRegistry::open(workspace)?);
let config = AgentLoopConfig::new(call).with_tools(tools.schemas().to_vec())?;
let agent = AgentLoop::new(session, provider, tools, config)?;
```

This keeps the Phase 3 `ToolExecutor` seam stable. Rust's type system does not
prevent an embedder from deliberately pairing unrelated schemas and executors,
so this is the canonical assembly pattern and the real-registry Agent test locks
that pairing.
`Debug` output reports only counts and a safe workspace summary; it never prints
file contents or raw arguments.

The implementation is split by responsibility rather than by upstream npm
package:

```text
src/tools/mod.rs          public facade and registry owner
src/tools/arguments.rs    closed argument DTOs and limits
src/tools/workspace.rs    capability-confined path and filesystem operations
src/tools/read.rs         UTF-8 line scan and rendering
src/tools/list.rs         one-level directory listing
src/tools/glob.rs         bounded file traversal and pattern matching
src/tools/grep.rs         bounded byte-regex search and rendering
src/tools/error.rs        stable model-facing failure classification
```

No module may reopen a rendered absolute path with ambient `std::fs` APIs.

## Registry pipeline

Every invocation follows one path:

```text
lookup exact tool name
→ parse the already bounded JsonValue into a closed object
→ reject unknown/missing/wrong-type/range-invalid fields
→ normalize and authorize the path relative to the fixed workspace
→ check cancellation
→ perform bounded filesystem work
→ normalize success or ordinary failure
→ return ToolExecutionResult
```

Unknown tools and every filesystem/user-input error are ordinary model-facing
failures, not `ToolExecutorError`. `ToolExecutorError` is reserved for an
internally inconsistent registry or an impossible result-normalization failure;
using it would poison the Agent because no trustworthy correlated result exists.

The registry dispatches only four exact names through an exhaustive match. There
is no dynamic plugin registration, callback chain, or placeholder handler.
`list`, `glob`, `grep`, and `read` are all read-only and do not require approval
in Phase 4.
The Phase 4 policy is therefore fixed and auditable: allow one of those four
operations only after typed argument and workspace-capability authorization;
deny unknown, outside, or unsafe targets. It has no `ask` state. Phase 5 may add
interactive policy/approval without changing the call/result order.

## Argument schemas

All root objects use `additionalProperties: false`, unlike the looser upstream
input objects. Tool names are exactly `list`, `glob`, `grep`, and `read`.

```text
list { path?: string = "." }
glob { pattern: string, path?: string = "." }
grep { pattern: string, path?: string = ".", include?: string }
read { file_path: string, offset?: integer = 1, limit?: integer = 2000 }
```

Rules common to path, glob, include, and regex strings:

- encoded length is at most 4,096 bytes;
- NUL and Unicode control characters are rejected;
- paths must not be empty after trimming; `grep.pattern` may contain whitespace
  but may not be the empty string;
- `read.offset` and `read.limit` are positive safe integers, and `limit` is at
  most 2,000;
- `grep.include` is one positive glob: blank, leading `!`, and a top-level comma
  are rejected, while brace alternation such as `*.{rs,toml}` is accepted.

Tool schemas are fixed JSON values constructed at registry creation. The schema
and parser agree on fields, required fields, primitive types, explicit `null`,
and closed root objects. JSON Schema `maxLength` counts characters, while the
runtime deliberately enforces the safer 4,096-byte UTF-8 budget; control
characters, safe-integer/range rules, glob/regex syntax, and path authorization
are also second-stage semantic checks. Tests lock both layers, including a
multibyte value that is structurally schema-valid but rejected by the byte limit.

## Workspace confinement and path rules

The authorization boundary is a `cap_std::fs::Dir`, not a sequence of
`canonicalize`, string-prefix check, then ambient reopen. The latter has a
time-of-check/time-of-use race: a parent directory or symlink can be replaced
after checking but before opening.

Paths are handled as follows:

1. A relative input is lexically normalized. `.` and safe `a/../b` are allowed;
   attempting to climb above the root is rejected.
2. An absolute input is accepted only when it is lexically beneath the normalized
   startup workspace, then converted to a relative capability path. Prefix-like
   siblings such as `/work-secret` do not match `/work`.
3. The capability API performs the real open/read authorization. A
   capability-resolvable relative symlink to an internal regular file may be
   read; an absolute-target, external, broken, or cyclic link fails without
   exposing its target.
4. Recursive `glob` and `grep` never descend through symlink directories. `list`
   reports a symlink as `symlink` without following it for classification.
5. Only regular files are read. Directories, FIFOs, sockets, devices, and other
   special files fail before content I/O.
6. Displayed paths are normalized workspace-relative UTF-8 with `/` separators.
   Non-UTF-8 names fail closed rather than being silently changed with lossy
   conversion.

Opening the workspace once also means that renaming the original directory or
replacing its old pathname does not silently move authority to the replacement.
Tests replace the pathname after opening and assert that the handle still names
the original directory; static parent/symlink escape cases and the startup
identity check cover the authorization boundary. This is not a filesystem
snapshot or a claim that tests can deterministically schedule every rename race.

## Tool behavior

### `read`

`read` scans one regular file in bounded chunks, validates the complete accepted
file as UTF-8, rejects a NUL anywhere in the file, counts all lines, and keeps
only the requested page. LF separates lines; one trailing CR is removed; a final
LF does not add a phantom line. Empty files have zero lines and allow offset 1.
For a non-empty file, an offset beyond EOF is `FS_NOT_FOUND`.

The page uses upstream-compatible line numbering and markup. At most 2,000 lines
are selected, a displayed line is at most 2,000 Unicode scalar values, and the
selected line text plus intervening LF bytes is at most 51,200 bytes. A truncated
line has `... (line truncated to 2000 chars)`. The footer distinguishes end of file,
pagination, and output-byte capping. The file is still scanned to EOF so a late
invalid UTF-8 sequence, NUL, or size overflow cannot be hidden outside the page.

### `list`

`list` reads exactly one directory level. It returns file, directory, symlink, or
other entries without reading file content, orders them by normalized UTF-8 byte
path, and renders one entry per line. Directories end in `/` and symlinks in `@`;
ordinary files may show a bounded size. Empty directories return an explicit
message. The result includes total and shown counts, and a truncation footer when
only the first 500 entries are rendered.

This model-facing tool has no upstream counterpart and is always documented as a
Rust extension. The upstream internal `listDir` is used only as evidence for the
one-level, entry-type, and stable-order design.

### `glob`

`glob` recursively walks beneath its authorized directory without following
symlink directories. It includes hidden and ignore-file-matched files, excludes
`.git`, `.svn`, `.hg`, `.bzr`, `.jj`, and `.sl` directories, and returns regular
files only. A pattern without `/` matches a basename at any depth. A pattern with
`/` matches the normalized path relative to the selected root.

Matches are ordered by modification time ascending, then normalized path
ascending to resolve the upstream's unspecified tie. The first 100 are rendered.
No match is `No files found`; truncation states the total and explicitly says the
complete set was not saved because Phase 8 spill storage does not yet exist.

### `grep`

`grep` searches one authorized regular file or a bounded recursive set of files.
It uses a Rust byte regular expression, so a line containing invalid UTF-8 can
produce the upstream placeholder `(line is not valid UTF-8)`. Binary files with a
NUL are skipped. Recursive search does not follow symlink directories or enter the
six version-control metadata directories.

The optional include glob filters normalized relative file paths. One result is
retained per matching line; matches are ordered by path ascending and then line
number ascending.
At most 250 are rendered in the upstream-style grouped form. A preview is at most
2,000 UTF-8 bytes without splitting a code point and uses ` (line truncated)`;
invalid UTF-8 uses the fixed placeholder. Zero matches is `No matches found`.

Upstream `grep` inherits ripgrep's ambient ignore/hidden choices and does not
promise a cross-file order. Rust's capability-native traversal deliberately uses
the explicit rules above so a host config cannot change the result.

## Resource limits

Untrusted byte/count accumulation uses checked or saturating arithmetic as
appropriate; direct additions are used only where smaller validated ceilings
already prove they cannot overflow. A limit failure returns no partial result.
The fixed Phase 4 ceilings are:

| Resource | Limit |
| --- | ---: |
| one path/pattern/include | 4,096 bytes |
| one `read` file | 16 MiB |
| filesystem read chunk | 64 KiB |
| `read` selected lines / display chars / selected bytes | 2,000 / 2,000 / 51,200 |
| `list` scanned / rendered entries | 10,000 / 500 |
| recursive depth / visited entries | 64 / 50,000 |
| retained normalized traversal paths | 8 MiB |
| stored `glob` matches / rendered matches | 10,000 / 100 |
| one `grep` file / all `grep` input | 8 MiB / 32 MiB |
| one `grep` line / stored matches / rendered matches | 1 MiB / 10,000 / 250 |
| one normalized text block as compact JSON | 64 KiB |

The registry limit is intentionally below the Agent's 256 KiB tool-result ceiling,
leaving room for durable event structure. Shared parameterized budget primitives
have exact-limit and one-over tests; representative fixed ceilings also exercise
16 MiB reads, 32 MiB aggregate grep input, 10,000 glob/grep matches, 1 MiB lines,
depth 64, and compact-JSON output. The suite does not manufacture a literal
50,000-entry tree or an 8 MiB pathname merely to duplicate the same lower-level
counter proof. Traversal stops and fails once a scan ceiling is crossed; it does
not call a partial scan complete.

## Async work, cancellation, and timeout

Capability filesystem calls are synchronous. The runtime therefore performs only
one short directory batch, metadata/open operation, or 64 KiB file read per
blocking job, awaits that job, and checks the child `CancellationToken` before and
after it. It does not place an entire recursive scan in one detached
`spawn_blocking` task. Once cancellation is observed, no new filesystem operation
starts and the registry returns a stable interrupted result.

The existing Agent supplies the per-tool deadline and a bounded cleanup grace. A
portable Rust process cannot forcibly interrupt one kernel call on a stuck FUSE or
network filesystem; dropping a blocking JoinHandle does not stop its worker. This
phase therefore promises prompt cooperative cancellation on normal local macOS and
Linux filesystems, not a hard timeout for a broken kernel filesystem. A killable
helper process would belong to Phase 6.

The registry future is lazy: constructing it touches no files. Its synchronous
factory returns promptly, it starts no detached task, and it owns all opened file
handles until completion or drop.

## Failures and durable facts

Stable failure codes include:

- `INVALID_ARGS` for unknown, missing, wrong-type, or out-of-range fields;
- `UNKNOWN_TOOL` for a name absent from the closed registry;
- `WORKSPACE_PATH_DENIED` for absolute/relative escape or outside symlink;
- `FS_NOT_FOUND`, `FS_NOT_DIRECTORY`, `FS_NOT_REGULAR_FILE`,
  `FS_PERMISSION_DENIED`, `FS_NOT_TEXT`, `FS_TOO_LARGE`, `FS_INVALID_NAME`,
  `FS_CHANGED`, and `FS_IO_ERROR`;
- `SEARCH_INVALID_PATTERN`, `SEARCH_LIMIT_EXCEEDED`, and `TOOL_OUTPUT_LIMIT`;
- `ABORTED` after body entry (the Agent owns `ABORTED_BEFORE_DISPATCH` for a call
  cancelled before it starts).

Messages contain a normalized relative path when useful, but never the workspace
absolute path, symlink target, raw OS error string, file content, or extension
panic text. Normal failures use a short text content block plus `ToolFailure`.
The text and `isError` flag are model-visible. The text plus `ToolFailure` are
durable and replayable without poisoning the session; Phase 4 produces no result
`meta` object.

The Session needs no new event type. The existing order remains:

```text
assistant/message → tool/call → tool/result → step/end
```

`tool/call` contains the exact model-provided arguments string and commits before
the registry body is polled. `tool/result.sourceEventSeqs` points to that call.

### Intentional `read` presentation difference

The pinned upstream `read` result displays an absolute path and returns a
structured file `value` plus presentation `meta`. Rust v0.1 intentionally emits
one workspace-relative text result and no success `meta`. This avoids publishing
the host's absolute workspace root and gives the model, append-only Session, and
terminal one authoritative replayable value instead of a second UI-only file
object. The observable cost is that an upstream presentation-card consumer cannot
read Rust's result byte-for-byte and sees no structured file object; the model
still receives numbered content, pagination facts, and normalized failures. The
Session schema gains no hidden state, and replay never rereads the file.

The type-checked Phase 4 oracle retains the upstream absolute text, `value`, and
`meta`; the paired Rust comparator normalizes only the known workspace prefix.
Rust result tests additionally prove that the persisted result has no success
`meta`, contains only the relative path, and replays the exact same model-visible
text. This fixture-backed privacy/ownership choice is therefore an
`intentional-difference`, not an unfinished presentation feature.

## Determinism, replay, and state

The registry has no mutable logical state after construction. Given the same
workspace snapshot, schema, arguments, and limits, it produces the same normalized
result. Directory mutation during a call is detected where practical by comparing
metadata from the opened handle and returns `FS_CHANGED`; this is not a filesystem
snapshot guarantee.

Only the call arguments and normalized result are durable. Replay never reruns a
filesystem tool. Session projection reuses the recorded result content exactly as
it does for every Phase 3 tool. Sorting uses explicit byte/path/line keys rather
than locale, hash-map order, or host configuration.

## Security and privacy boundary

- The registry is a capability boundary inside the process, not protection from
  hostile native code in the same process.
- Tool arguments cannot choose a new root, execute a shell, load ripgrep config,
  or create a process.
- Tests use fresh temporary directories and sentinel sibling files only. They do
  not read the user's repository, home directory, credentials, or environment
  configuration.
- File content and raw arguments are intentionally model-visible and durable only
  after the model has requested an authorized path. They stay out of `Debug` and
  infrastructure diagnostics.
- Non-UTF-8 names and Unicode characters classified by Rust as controls fail
  closed instead of being silently altered. Bidirectional-format characters are
  not classified as controls here; Phase 7 must escape untrusted paths/content
  when rendering a terminal.

## Dependencies and Rust 1.85

The direct production dependencies added by Phase 4 are:

- `cap-std = "=3.4.5"` (MIT OR Apache-2.0 WITH LLVM-exception OR Apache-2.0):
  capability-relative filesystem access; it declares no Rust version, so the
  locked tree must be compiled on Rust 1.85 rather than trusted from metadata;
- `globset = "=0.4.18"` (MIT OR Unlicense): bounded glob parsing/matching; newer
  `0.4.20` requires Rust 1.88 and is deliberately not selected;
- `regex = "1.12.3"` (MIT OR Apache-2.0, Rust 1.65): bounded byte-regex search;
- `rustix = "=1.1.4"` with `fs` (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR
  MIT, Rust 1.63): capability-relative nonblocking opens for special-file safety.

`ignore` is not used as the walker because its ambient-path traversal would bypass
the capability boundary and its defaults would silently change the glob contract.
No Tokio filesystem feature, ripgrep binary, shell parser, subprocess library,
serializer, async-stream helper, or general plugin framework is added. `cargo
+1.85.0 check --all-targets --locked`, license review, and macOS/Ubuntu CI are
mandatory before acceptance.

## Verification plan

Default tests are offline and use only temporary workspaces. They cover:

1. each schema, strict unknown fields, wrong types, empty/control/overlong strings,
   integer boundaries, unknown tools, and schema/parser parity;
2. relative/inside-absolute paths, safe normalization, sibling-prefix traps,
   outside `..`, external/broken/cyclic symlinks, directory-symlink non-traversal,
   non-UTF-8 names, and replacement races;
3. empty/LF/CRLF/trailing-newline files, pagination, byte/line truncation,
   directory/special files, early/late NUL, cross-chunk invalid UTF-8, size limits,
   and pre/mid-read cancellation;
4. one-level list types/order/truncation without reading contents;
5. glob basename/path matching, hidden/ignored inclusion, six VCS exclusions,
   files-only behavior, hostile-looking patterns, mtime/path ordering, and the
   parameterized traversal/match/output budget boundaries;
6. grep file/directory/include behavior, whitespace/invalid regex, CRLF, Unicode,
   invalid UTF-8 placeholder, binary skip, line preview, stable order, zero match,
   and file/aggregate/match ceilings;
7. cancellation before body and between bounded operations, with no filesystem
   operation after cancellation;
8. the real registry wired into `AgentLoop`, proving schema/executor identity,
   correlated bounded result and next-request reconstruction; combined with the
   Phase 3 executor-body prefix test and the registry's lazy future, this proves
   intention-before-filesystem-work; cancellation closes without network/write;
9. a type-checked deterministic oracle generated from the pinned upstream for
   canonical workspace-internal `read`, `glob`, and `grep`, plus paired evidence
   that upstream accepts and Rust rejects outside/symlink escape;
10. repository-wide fmt, check, test, Clippy, whitespace, Rust 1.85, and Ubuntu CI.

The oracle records exact source/test paths, schema surfaces, relevant upstream
configuration, canonical small read/glob/grep inputs and outputs, one missing-file
error, the internal list primitive, and paired ambient outside-path behavior. It
does not claim a two-runtime comparison for every truncation/resource/error case;
those Rust policies have independent boundary tests. It generates twice
byte-identically and the default Rust suite consumes only the committed JSON.
`list` is tested as a Rust extension rather than disguised as an
upstream-compatible scenario.

## Intentional differences and deferred work

The compatibility table must keep separate rows for:

- strict closed argument objects (upstream input roots ignore some extras);
- workspace confinement and external-symlink rejection (upstream read side is
ambient);
- full-file NUL detection and fixed resource limits;
- deterministic path/line tie-breaks and explicit grep traversal rules;
- truncation without Phase 8 persistent spill storage;
- the model-facing `list` product extension;
- in-process capability traversal instead of the upstream ripgrep subprocess.
- relative path presentation in Rust tool text, where upstream `read` displays an
  absolute path;
- text-only Rust results with no upstream structured `value`/presentation `meta`.

These differences make output or admission observably different for outside,
oversized, ignored, binary, tied-order, or extension-field inputs. They must not be
hidden inside a broad `compatible` claim. Approval/policy hooks, writes, shell,
interactive rendering, persistent spill/replay repair, and OS-level process
isolation remain explicitly deferred.
