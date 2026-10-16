# Using Nav

You can talk to Nav from four places. They all use the same agents, the same configuration and
the same permission rules.

| Where | Start it | Best for |
|---|---|---|
| VS Code | Install the [extension](vscode.md) | Daily work. It adds remarks and ghost-text suggestions to the editor, and hosts the Nav chat. |
| Terminal | `nav` or `nav tui` | A full chat client in your terminal. |
| Browser | `nav web` | A chat in your browser, with the Settings screen and the [InstinctFlow](instinctflow.md) Studio. |
| One task | `nav run -p "<task>"` | Scripts and quick questions. |

The [command line](cli.md) page lists every command and flag.

## Pick an agent

A conversation runs as one **primary agent**. You can switch between turns.

- `build` is the default. It implements, verifies and reports.
- `plan` researches and writes Markdown plans. It does not change code.

In the terminal client, press `tab` on an empty message to switch between them. Everywhere,
`/agent <id>` does the same, and `/new [agent]` starts a fresh conversation on a given agent. Nav
can also hand work to the crew (`vigie`, `lux`, `mousse` and so on) as **subagents**. See
[Agents](agents.md).

## Slash commands

Type `/` in any chat. A palette lists the commands. These work in every client:

| Command | What it does |
|---|---|
| `/help` | List the commands. |
| `/model [<gateway/id>]` | Show the current model, or switch to another one. |
| `/variant [<name\|off\|inherit>]` | Show or set the model variant. |
| `/agent [<id>]` | Show or change the primary agent. |
| `/new [agent]` | Start a fresh conversation. The old one is kept. |
| `/stop` | Stop the session. |
| `/context` | Show how the context window is used. |
| `/compact [focus]` | Summarise the older part of a long conversation. |
| `/bypass [on\|off]` | Skip ordinary permission prompts. See [Permissions](permissions.md). |

Type `//text` to send the literal text `/text` as a message.

How `/model`, `/variant`, `/agent` and `/new` behave is explained in
[Editor context and session overrides](session-overrides.md).

## Context and compaction

WithNav estimates the size of the next request and shows how full the model's window is. When a
conversation grows too long, `/compact` summarises the older part. Compaction also runs
automatically before a request that would not fit. By default the Metacognition model writes the
summary (`compaction.writer`; see [Configuration](configuration.md#settings-reference)).

## The terminal client: `nav tui`

`nav tui`, or `nav` alone, starts a private server and opens a chat. To use a server that is
already running, pass `--url`; see the [command line](cli.md#nav-tui).

- **Tabs.** Up to nine conversations, `alt+1` to `alt+9`. `ctrl+n` opens a new one, `alt+w`
  closes the tab and keeps the conversation, `alt+b` hides the tab bar.
- **Header.** Workspace, git branch, agent and model.
- **Right column.** The agent with its model and variant, the context breakdown, skills (listed,
  and loaded by the model) and the MCP servers (a dot each: green ready, orange starting, red
  failed), the layers (green ready, red unavailable, Nav orange while it runs) and git changes.
  `alt+r` shows or hides it. The status line also sums up the MCP servers, for example `mcp 2/3`.
- **Copy mode.** Clicking the transcript gives it the focus to select and copy; `ctrl+shift+c`
  copies the selection or the selected block. Typing a letter or a digit goes back to the editor.
- **Command palette.** `ctrl+p` or `/`. It also holds `/config` (edit settings), `/sessions`,
  `/skills`, `/theme` and `/quit`.
- **Models.** `f2` opens the model picker, with variants. `ctrl+f` marks a favourite.
- **Sessions.** `ctrl+o` lists past conversations to resume.
- **Subagents.** `alt+s` opens a subagent's read-only transcript. `alt+c` continues one that ran
  out of steps.
- **Bypass.** `alt+p` turns [bypass](permissions.md#bypass) on or off.
- **Themes.** Eight: Ayu, Nord, Tide and Moss, each dark and light. Tide Dark is the default.
  Choose one with `/theme`; it is saved as `tui.theme`.
- **Help.** `f1` shows every shortcut. `ctrl+q` quits, `esc` or `ctrl+c` stops a running turn.

Permission questions appear inline: `o` allows once, `r` rejects (you can add a message), and `a`
(always) appears when a reusable rule can be saved.

## The browser client: `nav web`

`nav web` starts WithNav on a private address on your computer, prints a link and opens it. The
link carries a secret that changes on every run and only works on this machine. Press `ctrl+c` to
stop.

The page lets you chat in any workspace you pass with `--workspace` (repeat the flag for several;
the current directory is the default) and in folders you used before. Each shows its git branch
and whether it has changes. The gear in the top bar opens Settings, and the command palette opens
the InstinctFlow Studio.

## Settings screens

The model of each layer and agent, permissions, subagent limits, watchdogs and the theme can be
changed without editing JSON:

- In the browser: the gear in the top bar.
- In the terminal: `/config`. Changes are written to `config.json` with `ctrl+s`.
- In VS Code: the **WithNav: Open Settings** command, or the gear in the Nav chat.

All three write the same `config.json`. Gateways are read-only there, and API keys are never
shown. Layer models apply after you restart WithNav. The rest applies to new sessions.
