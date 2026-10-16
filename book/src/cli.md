# Command line

`nav` is one binary with several subcommands. `nav --help` lists them, and `nav <command> --help`
shows the flags of one.

| Command | What it does |
| --- | --- |
| [`nav tui`](#nav-tui) | Terminal chat client. |
| [`nav web`](#nav-web) | Browser chat client. |
| [`nav run`](#nav-run) | Run one task and exit. |
| [`nav serve`](#nav-serve) | The session server alone (HTTP and SSE). |
| [`nav auth`](#nav-auth) | Store API keys. |
| [`nav doctor`](#nav-doctor) | Read-only diagnostics. |
| [`nav lsp`](#nav-lsp) | The language server the editor extension launches. |
| [`nav permissions`](#nav-permissions) | Permission rules, decision history, and validations of flows and skills. |
| `nav flows catalogue` | Print the flow schema of this build as JSON. |

Add `--version` to any of them to print the version.

## `nav tui`

```sh
nav [tui] [--workspace <dir>] [--model <gateway/id> | --session <id>] [--models <file> | --url <server>]
```

| Flag | Meaning |
| --- | --- |
| `--workspace <dir>` | Folder the conversation works in. Default: the current directory. |
| `--model <gateway/id>` | Model for a new conversation. Cannot be combined with `--session`. |
| `--session <id>` | Resume a past conversation. Cannot be combined with `--model`. |
| `--models <file>` | Use this `models.json` for the embedded server. Cannot be combined with `--url`. |
| `--url <server>` | Connect to a running server instead of starting one. If it needs a bearer token, set `WITHNAV_SERVER_TOKEN`. Cannot be combined with `--models`. |

The terminal client writes its log to `~/.local/share/withnav/tui.log`, never to the screen.

## `nav web`

```sh
nav web [--workspace <path>]... [--models <file>] [--port <n>] [--no-open]
```

| Flag | Meaning |
| --- | --- |
| `--workspace <path>` | A workspace to offer in the browser. Repeat for several. Default: the current directory. |
| `--models <file>` | Use this `models.json` for the embedded server. |
| `--port <n>` | Loopback port to listen on. Default `0`, which picks a free one. |
| `--no-open` | Print the address without opening the browser. |

## `nav run`

```sh
nav run -p "<task>" [flags]
```

Runs one task in the terminal and exits. If you pass no `-p`, or `-p -`, the prompt is read from
standard input. `nav run` never approves a tool call by itself, unless you pass `--yes`.

| Flag | Meaning |
| --- | --- |
| `-m`, `--model <gateway/id>` | Model to use. |
| `-p`, `--prompt <text>` | The task. A prompt that is a `/command` runs that command. |
| `--workspace <dir>` | Folder to work in. Default: the current directory. |
| `--models <file>` | Use this `models.json`. |
| `--agent <agent>` | Primary agent: `build` (default) or `plan`. |
| `--variant <name\|off>` | A variant the model offers, or `off` for none. Default: the model's default variant. |
| `--show-thinking` | Show the model's reasoning. |
| `--yes` | **Unsafe.** Turn on [bypass](permissions.md#bypass) for the run: ordinary permission prompts run once. Protected ones still stop the run with exit code 3. |

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Done. |
| `1` | Error. |
| `2` | The agent asked a question. |
| `3` | A tool call needs approval (`permission_required`). |
| `130` | Interrupted. |

## `nav serve`

```sh
nav serve [--bind <addr>] [--allow-non-loopback] [--models <file>]
```

Runs the session server alone, with no client: an HTTP API with Server-Sent Events.

| Flag | Meaning |
| --- | --- |
| `--bind <addr>` | Address to listen on. Default `127.0.0.1:7310`. |
| `--allow-non-loopback` | Allow an address that is not loopback. |
| `--models <file>` | Use this `models.json`. |

The server has **no authentication and no TLS**. Keep it on loopback. If you open it to a
network with `--allow-non-loopback`, protecting it is your job: use a private network or a
reverse proxy.

## `nav auth`

```sh
nav auth set <provider>
nav auth list
nav auth remove <provider>
```

Keys are stored in `~/.config/withnav/auth.json` with mode 0600. `set` reads one line from
standard input and does not hide it as you type, so pipe the key in if anyone can see your
screen. `list` prints the providers that have a key. A stored key wins over the environment
variable named by `api_key_env`.

## `nav doctor`

```sh
nav doctor [--offline] [--json] [--workspace <dir>] [--layer <reflex|instinct|metacognition>]
```

Checks your configuration. It is covered in [Troubleshooting](troubleshooting.md#check-your-setup-with-nav-doctor).

## `nav permissions`

```sh
nav permissions list [--all] [--project <dir> | --global | --agent <id>] [--json]
nav permissions add <action> <resource> <allow|ask|deny> [--project <dir> | --global | --agent <id>]
nav permissions revoke <id>
nav permissions history [--limit <n>] [--json]
nav permissions validate flow <name> [--location global|workspace]
nav permissions validations [--json]
nav permissions revoke-validation <id>
```

| Subcommand | What it does |
| --- | --- |
| `list` | The active rules, newest first; `--all` includes revoked ones. |
| `add` | Add a rule. Without a scope flag it applies to the current project. |
| `revoke` | Revoke a rule by id. Rules are kept in the history, never deleted. |
| `history` | The last permission decisions: call, rule, flow verdict, your answer, outcome. |
| `validate flow` | Validate the current content of a global or workspace flow, so it can run. Reload flows to apply. Skills are validated from the conversation, when they are first loaded. |
| `validations` | Every validation of a flow or a skill, with its content hash. |
| `revoke-validation` | Revoke a validation. For a flow, revocation takes effect at the next flow reload; skills require validation again at their next use. |

See [Permissions](permissions.md).

## `nav lsp`

```sh
nav lsp [--models <file>]
nav lsp replay [--log <file>] [--json]
```

The language server for editors. The VS Code extension launches it for you over stdio, so you
rarely run it yourself. `nav lsp replay` replays the built-in editor flows offline against a
decision log (default `~/.local/share/withnav/withnav.jsonl`) and prints a summary, or a JSON
report with `--json`.

## Environment variables

| Variable | Meaning |
| --- | --- |
| `WITHNAV_SERVER_TOKEN` | Bearer token for `nav tui --url`. |
| `WITHNAV_MAX_STEPS` | Cap on the steps of one turn. Unset: no cap for the main agent (a watchdog supervises it) and 50 for subagents. A value that is not a non-negative integer logs a warning and caps the turn at 500 steps (50 for subagents). |
| API key variables | The names you give in `api_key_env` in `models.json`, for example `OPENAI_API_KEY`. |

The old `NAVAI_*` names still work as fallbacks, with a warning. See
[Older installs](troubleshooting.md#older-installs).
