# Security policy

## Supported versions

There is no supported stable release yet. The current `0.1.0-alpha.0` source tree
and the latest `main` revision accept security reports, but they do not have a
security-update service-level agreement. Older commits and private forks are not
maintained release lines.

## Security model

`dsh` is a local coding agent, not an operating-system sandbox. It treats model
output as untrusted and routes file changes and Shell commands through validation,
policy, a visible preview, and interactive approval. Script mode cannot ask a
human, so it denies those side effects.

### Workspace files

The built-in list, glob, grep, read, and patch tools start from one retained
workspace capability. They normalize paths and reject known parent, sibling,
special-file, symlink, and hard-link escape cases. These checks constrain the
built-in file tools; they do not constrain arbitrary native code launched by an
approved Shell command.

### Shell commands and process cleanup

An approved Bash command runs as the current user and may leave the workspace,
read other user-accessible files, access the network, or start more processes.
Approval is an informed-consent boundary, not isolation.

On normal macOS and Linux paths, cancellation and timeout try to terminate and
reap the command's owned process group. An uninterruptible kernel operation,
permission change, `SIGKILL`, or a descendant that deliberately creates another
session/process group can delay or defeat that cleanup. Do not approve a command
you would not run directly in a terminal.

### Credentials and endpoints

The DeepSeek API key is read from `DEEPSEEK_API_KEY` for each request and is not
intentionally written to Session logs or normal output. Prompts, tool arguments,
commands, file contents, and custom error text are model- or Session-visible, so
do not place unrelated secrets in them.

`DEEPSEEK_BASE_URL` is a trusted operator setting. `dsh` requires HTTPS except
for loopback HTTP, disables redirects and the system proxy, and does not send an
anonymous device identifier. Pointing it at a custom HTTPS service still grants
that service the request content and API credential selected by the operator.

### Local Session data

Session JSONL files use a private local directory and bounded records, but their
conversation and tool content is plaintext. They are convenience state for
normal save/list/resume, not encryption, a backup, or database-grade durability.
Protect the account and storage containing them, and delete the configured
Session directory when its history is no longer needed.

### Local subprocess tool plugins

`--plugin-config` starts explicitly named local executables so they can declare
tools. This happens during CLI startup, before any per-call approval. Therefore
the config itself must be treated like permission to run those programs as the
current user. The approval card controls one later model-requested tool call; it
does not sandbox the already running plugin process.

The config must be a private regular file. Program paths are canonical and
revalidated, unsafe writable parent chains, symlinks, set-ID files, and macOS
write-grant ACLs are rejected. This narrows accidental substitution by another
account; it does not defend against the same trusted account deliberately
replacing its own executable between the final check and `exec`.

Plugin processes receive a closed five-variable environment without the
DeepSeek key, Session root, or home directory. Stdout is a bounded protocol
channel, stderr is bounded diagnostics, calls and queues have deadlines and
size limits, and normal shutdown/cancellation waits for process-group cleanup.
As with approved Shell, native code can still access anything the current user
can access, use the network, or deliberately create a new process session.
Never configure a plugin executable you would not run directly.

If a call may have been dispatched but no trustworthy matching result arrives,
`dsh` records `TOOL_OUTCOME_UNKNOWN`, makes that plugin unavailable, and does not
automatically replay the call after resume. Configured program paths, configured
program argv, stderr, and protocol IDs are not stored in Session history.
Model-requested tool arguments are necessarily recorded before dispatch and
remain model- and Session-visible.

Configured program argv may be visible to other processes running as the same
account through ordinary operating-system inspection even though dsh does not
persist it. Do not place API keys, passwords, or unrelated secrets in plugin
configuration.

### Other extensions and platforms

There is still no MCP server, Hooks, Skills, native dynamic-library loading,
background-job system, Cordis/npm plugin compatibility, or general extension
framework. Phase 10 only adds the closed subprocess tool boundary described
above.

The declared release target is macOS on arm64 and Ubuntu 24.04 on x86_64; a
candidate is accepted only after both default CI jobs pass. Windows and other
operating-system/architecture combinations are not currently supported or
security-tested.

## Reporting a vulnerability

Please do not publish exploit details in a public issue. Use the repository's
[private vulnerability reporting form](https://github.com/xizheyin/deepseek-harness-rs/security/advisories/new)
when it is available. If that form is not visible, open a public issue titled
`Security contact request` without vulnerability details or private data; the
maintainer will arrange a private channel before asking for the report.

Useful reports include the affected revision, operating system, impact, minimal
reproduction, and whether the issue can expose secrets, modify files outside the
workspace, bypass approval, or leave child processes running.

Never include a real API key, private source code, or other user data in a report.
