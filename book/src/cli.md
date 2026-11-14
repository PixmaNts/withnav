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
| `--models <file>` | The `models.json` the WithNav server must use. If a server is already running with another one, `nav tui` stops with a message telling you to run `nav serve stop`. Cannot be combined with `--url`. |
| `--url <server>` | Connect to that server instead of the shared one. If it needs a bearer token, set `WITHNAV_SERVER_TOKEN`. Cannot be combined with `--models`. |

`nav tui` connects to the WithNav server (see [`nav serve`](#nav-serve)), starting it in the
background if none runs. Every client of your user (VS Code, the browser client, `nav run`, other
terminals) uses that same server, so a conversation started in one is listed and usable in the
others. Quitting `nav tui` only disconnects; the server keeps running.

The terminal client writes its log to `~/.local/share/withnav/tui.log`, never to the screen.

## `nav web`

```sh
nav web [--workspace <path>]... [--models <file>] [--port <n>] [--no-open]
```

| Flag | Meaning |
| --- | --- |
| `--workspace <path>` | A workspace to offer in the browser. Repeat for several. Default: the current directory. |
| `--models <file>` | The `models.json` the WithNav server must use (as for `nav tui`). |
| `--port <n>` | Refused unless `0`: the shared server picks its port; set it with `nav serve --port`. |
| `--no-open` | Print the address without opening the browser. |

`nav web` starts the WithNav server if needed, asks it for a pairing code, opens
`http://127.0.0.1:<port>/#pair=<code>` and exits. The page trades the code once, within 60
seconds, for its own browser credential (valid 12 hours, kept for that tab only); the server's own
token never appears in the address. Run `nav web` again to open another tab or after the
credential expires.

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
nav serve [--port <n>] [--models <file>]
nav serve status
nav serve stop [--force]
```

The WithNav server: the one process that owns your conversation database
(`~/.local/share/withnav/withnav.db`) and serves every client: the editor's language server,
`nav tui`, `nav web` and `nav run`. You rarely start it yourself: the first client starts it in
the background and the others connect to it. It keeps running when clients leave; stop it with
`nav serve stop`.

| Flag | Meaning |
| --- | --- |
| `--port <n>` | Loopback port to listen on. Default `0`, which picks a free one. |
| `--models <file>` | Use this `models.json` instead of the workspace's or the config dir's. |

| Subcommand | Meaning |
| --- | --- |
| `status` | Whether a server runs, its process id, version and port. |
| `stop` | Stops the server once no turn is running; `--force` stops the running turns too. Works with a server of another WithNav version. |

The server listens on `127.0.0.1` only and always requires a token. It writes its port and token
to `~/.local/share/withnav/server/owner.json`, readable by you only (mode `0600` in a `0700`
directory); clients read it there and check, before sending the token, that the server answering
is the one that wrote the file. Requests through `HTTP_PROXY`/`ALL_PROXY` and requests with a
foreign `Host` or `Origin` header are refused. Only one server runs per database: a second one
stops with a message. The server also refuses to start while an older WithNav (an embedded
terminal or editor server from before this version) still has the database open; quit it first.
If the data directory is deleted, the server stops on its own.

Its log is `~/.local/share/withnav/server/server.log`.

Upgrading WithNav: stop the server first (`nav serve stop`), then install; the next client starts
the new version. A client of another version refuses to join a running server and says so. Do not
run an older `nav` against the same data directory while the new server runs: older versions do
not know the owner lock. The data directory may be reached through a symbolic link (both paths
find the same server), but `withnav.db` itself must not be a symbolic link, and hard links or a
data directory shared over a network file system are not supported.

On Windows the server and its clients use the same design; the token file is protected by an
access list limited to you, SYSTEM and Administrators. If Windows forbids starting it in the
background (a job object without breakaway), the client says so: start `nav serve` yourself.

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
rarely run it yourself. It runs inside the WithNav server: `nav lsp` connects to it (starting it
if needed) and passes the editor's messages through, so several editors share the server and its
conversations, and closing an editor leaves the server running. If the server stops, `nav lsp`
exits and the editor restarts it. `nav lsp replay` replays the built-in editor flows offline against a
decision log (default `~/.local/share/withnav/withnav.jsonl`) and prints a summary, or a JSON
report with `--json`.

## Environment variables

| Variable | Meaning |
| --- | --- |
| `WITHNAV_SERVER_TOKEN` | Bearer token for `nav tui --url` (not needed for the shared server). |
| `WITHNAV_MAX_STEPS` | Cap on the steps of one turn. Unset: no cap for the main agent (a watchdog supervises it) and 50 for subagents. A value that is not a non-negative integer logs a warning and caps the turn at 500 steps (50 for subagents). |
| API key variables | The names you give in `api_key_env` in `models.json`, for example `OPENAI_API_KEY`. |

The old `NAVAI_*` names still work as fallbacks, with a warning. See
[Older installs](troubleshooting.md#older-installs).
