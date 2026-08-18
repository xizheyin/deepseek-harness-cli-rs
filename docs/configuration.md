# Configuration

`dsh-rs` keeps configuration deliberately small. The installed command is
`dsh`; it reads command-line flags and process environment variables. Phase 10
adds one explicit local tool-plugin file, but there is still no project-wide
auto-discovery, global profile, or hot reload.

## Required credential

```console
export DEEPSEEK_API_KEY='your DeepSeek API key'
dsh --workspace .
```

The Provider reads `DEEPSEEK_API_KEY` for each request. It is not intentionally
persisted, but prompts, file contents, tool arguments, commands, and Session
events are model-visible. Do not put unrelated secrets in those values.

## Command-line settings

| Flag | Default | Meaning |
| --- | --- | --- |
| `--workspace <PATH>` | current directory for a new Session | Workspace retained by file tools and Shell startup |
| `--model <MODEL>` | `deepseek-v4-flash` for a new Session | DeepSeek model; resume otherwise reuses the stored model |
| `--prompt <TEXT>` | interactive terminal | Run one prompt and exit; write, Shell, and plugin approvals are denied |
| `--list-sessions` | off | List bounded local Session headers |
| `--resume <SESSION_ID>` | new Session | Continue one validated stored Session |
| `--plugin-config <PATH>` | no plugins | Start the explicitly configured local tool plugins for this process |
| `--no-color` | color when supported | Disable product-owned ANSI styling |

Run `dsh --help` for exact syntax and mutually exclusive combinations.

## Environment variables

| Variable | Rule |
| --- | --- |
| `DEEPSEEK_API_KEY` | Required when a model request is made |
| `DEEPSEEK_BASE_URL` | Optional trusted base URL; HTTPS only, except loopback HTTP for offline tests |
| `DSH_SESSION_ROOT` | Optional absolute Session directory override |
| `XDG_STATE_HOME` | Linux state base when `DSH_SESSION_ROOT` is absent |
| `NO_COLOR` | Presence disables ANSI styling |
| `TERM=dumb` | Also selects the plain terminal presentation |

The HTTP client does not follow redirects and ignores system proxy settings.
Choosing a custom HTTPS endpoint still grants that endpoint the API key and
model-visible request content.

## Local subprocess tool plugins

Plugins are an experimental, tool-only Phase 10 extension. A plugin is a
trusted native executable started by `dsh`; it is **not sandboxed**. Passing a
config authorizes process startup and schema discovery. In interactive mode,
each valid model-requested plugin call still asks for approval. Script and piped
input modes deny plugin calls because no human can approve them.

Build the two no-side-effect examples from this repository:

```console
cargo +1.85.0 build --locked --examples
```

Create a private JSON file whose program paths are absolute, canonical paths:

```json
{
  "version": 1,
  "plugins": [
    {
      "id": "text-tools",
      "program": "/absolute/path/to/dsh-rs/target/debug/examples/text_stats_plugin",
      "args": []
    },
    {
      "id": "json-tools",
      "program": "/absolute/path/to/dsh-rs/target/debug/examples/json_format_plugin",
      "args": []
    }
  ]
}
```

Then restrict the file and launch it explicitly:

```console
chmod 600 /absolute/path/to/plugins.json
dsh --workspace . --plugin-config /absolute/path/to/plugins.json
```

The file is limited to eight plugins. IDs match
`[a-z][a-z0-9-]{0,31}`; programs must be regular executable files with no set-ID or writable
unsafe path component. Each plugin receives only `PATH=/usr/bin:/bin`,
`LANG=C`, `LC_ALL=C`, `DSH_PLUGIN_PROTOCOL=1`, and its `DSH_PLUGIN_ID`; it starts
in the program's parent directory. Its stdout is reserved for the bounded
version-1 NDJSON protocol and stderr is bounded diagnostics.

Configured `args` become ordinary operating-system process argv. They are not
automatically persisted into Session JSONL, but same-account process inspection
may expose them; never put an API key, password, or unrelated secret there.

Plugin configuration, executable paths, and configured program argv are not
automatically written into Session JSONL. Model-requested tool arguments and
results are recorded as normal Agent facts. Plugins are not restored
automatically: pass `--plugin-config` again with
`--resume` if that new process should expose the tools. An already recorded
unknown tool outcome is never replayed to a restarted plugin. The closed
protocol/schema limits are documented in
[the Phase 10 design](design/subprocess-tool-plugins.md).

## Session locations

- macOS: `~/Library/Application Support/dsh/sessions`
- Linux with `XDG_STATE_HOME`: `$XDG_STATE_HOME/dsh/sessions`
- Linux fallback: `~/.local/state/dsh/sessions`

`DSH_SESSION_ROOT` is primarily useful for isolated tests and operator-managed
storage. It must be absolute. Session JSONL contains plaintext conversation and
tool history; see [SECURITY.md](../SECURITY.md) before retaining sensitive work.
