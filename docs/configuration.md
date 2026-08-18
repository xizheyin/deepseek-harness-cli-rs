# Configuration

`dsh-rs` v0.1 keeps configuration deliberately small. The installed command is
`dsh`; it reads command-line flags and process environment variables, with no
project config file, global profile, or hot reload.

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
| `--prompt <TEXT>` | interactive terminal | Run one prompt and exit; write/Shell approvals are denied |
| `--list-sessions` | off | List bounded local Session headers |
| `--resume <SESSION_ID>` | new Session | Continue one validated stored Session |
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

## Session locations

- macOS: `~/Library/Application Support/dsh/sessions`
- Linux with `XDG_STATE_HOME`: `$XDG_STATE_HOME/dsh/sessions`
- Linux fallback: `~/.local/state/dsh/sessions`

`DSH_SESSION_ROOT` is primarily useful for isolated tests and operator-managed
storage. It must be absolute. Session JSONL contains plaintext conversation and
tool history; see [SECURITY.md](../SECURITY.md) before retaining sensitive work.
