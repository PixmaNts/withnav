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
| `/goal [<objective>\|criterion <text>\|pause\|resume\|clear]` | Show or set the session goal. See [Session goal](#session-goal). |
| `/remember [scope] <text>` | Save a [memory item](memory.md). |
| `/memory [action]` | List, search, inspect, or revise [memory items](memory.md). |
| `/forget <id> [reason]` | Invalidate the current version of a [memory item](memory.md). |

Type `//text` to send the literal text `/text` as a message.

How `/model`, `/variant`, `/agent` and `/new` behave is explained in
[Editor context and session overrides](session-overrides.md).

## Context and compaction

WithNav estimates the size of the next request and shows how full the model's window is. When a
conversation grows too long, `/compact` summarises the older part. Compaction also runs
automatically before a request that would not fit. By default the Metacognition model writes the
summary (`compaction.writer`; see [Configuration](configuration.md#settings-reference)).

The summary keeps what Nav needs to continue honestly: each piece of verified work with the check
that confirmed it, claims no tool output confirmed, failed attempts and what not to retry, what you
asked for that is not done or not checked yet, and the next check to run.

## The session ledger

Every conversation, subagents included, keeps a ledger of what it did: the turns and how the last
one ended, the files `write` and `edit` changed, the last shell commands with their exit code,
the commands that failed more than once, the last answer, the questions still waiting for you, and
the [session goal](#session-goal).
WithNav rebuilds it from the conversation's history whenever it is read, so it costs no model call
and a stopped conversation has one too. Every list and text is bounded. The terminal client shows
it in the right column; a client reads it at `GET /v1/sessions/{id}/ledger`.

## The session view

The web client's right panel has a **Session** tab, and the terminal client's right column shows the
same three things, read again after each turn, each goal change, each finished goal check and each
`/remember`, `/forget` or `/memory` command:

- **Goal**: the objective (three lines, expandable on the web), its revision and status, and each
  criterion with the status of its last check: supported, contradicted or not checked. On the web,
  hover a criterion for the evidence the status rests on; the panel also says when the goal was last
  checked. After a check of the goal's current revision, the agent's completion claims are listed
  too, as supported, contradicted or unverified (unverified is never false). A check of an older
  revision shows every criterion as not checked.
- **Ledger**: the turns, the files changed (the last three are named), the repeated failures (the
  worst is named) and the open questions.
- **Memory**: the [memory items](memory.md) this session may be given, by title with their scope.
  What is actually sent to the agent in a turn is not shown yet.

Two lines appear in the transcript. `Remembered: "<title>" · <scope> · undo` follows a successful
`/remember`; on the web, **undo** invalidates the item at the version `/remember` stored (and
offers **redo**), and the terminal and editor lines tell you to use `/forget <id>`. A **Goal note**
follows a goal check that queued a note for the agent (see below), marked as an automatic
observation, not a Captain message. The editor shows these two lines only.

## Session goal

`/goal <objective>` records what counts as done for this conversation. `/goal criterion <text>`
adds a success criterion; criteria are numbered in the order you add them. `/goal` alone shows the
goal. Setting a new objective revises it: the criteria stay, and the revision number goes up by one
with each change of the objective or the criteria. `/goal pause` keeps the goal but stops sending
it, `/goal resume` sends it again, and `/goal clear` removes it.

While the goal is active, every request of the conversation carries it as a labelled block, with
the reminder that the goal says what counts as done and does not let the agent continue or act
without you. It is not part of the transcript, so compaction never summarizes it away, and it is
counted under `goal` in the context breakdown. The goal is stored in the conversation's history: it
survives a stop, a restart and a resume. Subagents do not inherit it.

An objective holds at most 4000 characters, a criterion 1000, and a goal at most 30 criteria; longer
text is refused, never cut. `criterion`, `pause`, `resume` and `clear` cannot start an objective.
Only you set the goal: the agent has no tool to change it. The project's lasting intent stays in
`.withnav/intent.md`. The terminal client shows the goal in its right column, before the ledger.

When a turn ends while the goal is active with at least one criterion, and a metacognition model
is assigned, that model checks each criterion against the conversation's ledger: supported (a
command with its exit status, a file change or a test run shows it, and the model quotes that
line; the agent saying so is not enough), contradicted, or not checked. A long goal is sent with
as many criteria as fit in half the check's input, so there is room left for the evidence. The
result is written to `goal-checks.jsonl` in the data
directory. The check never delays a turn: at most one runs at a time per
conversation, at most ten per hour, each within 60 seconds; a slower one is logged as a timeout.

The same call also reads what the agent claims in its last answer ("tests pass", "build passes",
"committed"), at most five claims, and sets each against the ledger: supported or contradicted
only when the model quotes a command or file-change line that was sent, and unverified otherwise.
A claim nothing in the ledger settles is unverified, never reported as false. One case is
reported as contradicted without a quote: the model finds that the claim reports a command run or a
result obtained that the ledger does not record at all ("invented"), and the check was sent every
command and file change of the session. When some were cut to fit, the claim stays unverified,
because the missing line may be one that was cut.

A check that finished is shown to your clients: the server publishes its criteria statuses and
claims as one `command.result` named `goal_check`, and the ledger
(`GET /v1/sessions/{id}/ledger`, field `goal_check`) keeps the last one for the goal's current
revision; a change of the goal clears it. This happens whether or not notes are on, and a check
that was skipped, timed out or did not read publishes nothing. The result is for you, not for the
agent: the check never reads an earlier result as evidence.

By default the agent hears nothing of it. Set `goal_watch.notes` to `true` (see
[Configuration](configuration.md#settings-reference)) to let it tell the agent what it found: when
a criterion is contradicted, or is still not checked after a turn the agent ended on its own, the
agent gets one short note with its next turn, for example `Goal check (revision 3): criterion 2
"Windows CI passes" is contradicted: "turn 4 · exit 101 · cargo test". This is an automatic
observation, not a Captain instruction.` The note waits for your next message (or the next turn
something else starts) and never starts a turn itself. The same findings are noted once per goal
revision; subagents never get a note; the published result says whether a note was sent and quotes
it. Each line of `goal-checks.jsonl` says whether a note was sent
(`note_sent`) and, if not, why (`note_skipped`): `setting_off` (notes are off), `nothing_to_report`
(the check found nothing to tell), `check_skipped` (no check ran: one was still running, or the
hourly limit was reached), `duplicate` (already noted for this goal revision) or
`delivery_refused` (the conversation was stopping; a later check can still send it).

## The terminal client: `nav tui`

`nav tui`, or `nav` alone, starts a private server and opens a chat. To use a server that is
already running, pass `--url`; see the [command line](cli.md#nav-tui).

- **Tabs.** Up to nine conversations, `alt+1` to `alt+9`. `ctrl+n` opens a new one, `alt+w`
  closes the tab and keeps the conversation, `alt+b` hides the tab bar.
- **Header.** Workspace, git branch, agent and model.
- **Right column.** The agent with its model and variant, the context breakdown, the session
  ledger (turns and how the last one ended, the files the agent changed, the commands that keep
  failing), skills (listed,
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
- **Images.** `/attach <path>` adds an image to the next message; dropping an image file on the
  terminal (which pastes its path) does the same. `/paste-image` adds the image on the clipboard,
  read with `wl-paste` or `xclip` (`pngpaste` on macOS). Each image of the conversation, yours or
  a tool's, shows as a numbered line; `/image [n]` opens it in the system viewer (the last one by
  default). Over SSH the clipboard and the viewer are the remote machine's.
- **Themes.** Eight: Ayu, Nord, Tide and Moss, each dark and light. Tide Dark is the default.
  Choose one with `/theme`; it is saved as `tui.theme`.
- **Help.** `f1` shows every shortcut. `ctrl+q` quits, `esc` or `ctrl+c` stops a running turn.

Permission questions appear inline: `o` allows once, `r` rejects (you can add a message), and `a`
(always) appears when a reusable rule can be saved. While a [parallel
review](permissions.md#parallel-review-permissionsreview-on) runs, `h` holds it: the review stops
and the question stays yours.

## The browser client: `nav web`

`nav web` starts WithNav on a private address on your computer, prints a link and opens it. The
link carries a secret that changes on every run and only works on this machine. Press `ctrl+c` to
stop.

The page lets you chat in any workspace you pass with `--workspace` (repeat the flag for several;
the current directory is the default) and in folders you used before. Each shows its git branch
and whether it has changes. The gear in the top bar opens Settings, and the command palette opens
the InstinctFlow Studio.

When a session is blocked on a permission question, the top bar counts the waiting sessions and the
sidebar marks the session with a dot. From the count you can allow browser notifications (one per
blocked turn, closed once it is answered) and mute a session.

## Settings screens

The model of each layer and agent, permissions, subagent limits, watchdogs and the theme can be
changed without editing JSON:

- In the browser: the gear in the top bar.
- In the terminal: `/config`. Changes are written to `config.json` with `ctrl+s`.
- In VS Code: the **WithNav: Open Settings** command, or the gear in the Nav chat.

All three write the same `config.json`. Gateways are read-only there, and API keys are never
shown. Layer models apply after you restart WithNav. The rest applies to new sessions.
