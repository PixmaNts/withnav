All notable changes to this project. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, a minor version may break compatibility. New entries go under `Unreleased` as pull
requests land in `develop`.

## [Unreleased]

### Added

- **Terminal client**: `/skills` opens a searchable list of this conversation's skills. Choose one to fill `/skill <name>`, add your request, and send to load its instructions before the agent starts thinking.

## [0.4.1] - 2026-10-04

Fixes from daily use of 0.4.0, mostly in the terminal client.

### Changed

- **Folder grants persist**: an "allow folder" answer is saved per workspace in
  `<data dir>/grants.json` and applies to later sessions in that workspace. Subagents keep theirs in
  memory. Delete the workspace's entry in that file to revoke.

### Fixed

- **Terminal client**: running out of agent steps shows "Out of steps · send a message to
  continue", no longer a question awaiting an answer.
- **Terminal client**: a multiline draft grows to 20 lines, then scrolls with the cursor, and
  leaves room for the conversation on a small terminal.
- **Terminal client**: arrow keys and Home/End move the cursor in a draft, including between lines.
- **Terminal client**: a 1M context window no longer shows as `1000k`.
- **Terminal client**: moving or resizing the terminal window no longer closes the client with
  "cannot draw the screen: The cursor position could not be read within a normal duration". The
  repaint after a resize no longer asks the terminal for the cursor position.

## [0.4.0] - 2026-10-04

Background subagents: a conversation's agent can delegate work to other agents that run on their
own, report back, can be steered or interrupted, and are watched by Jev. The terminal client shows
them under their conversation.

### Added

- **Subagents**: a conversation's agent can start a subagent with the `task` tool
  (`task(agent, prompt, delivery?)`, for agents whose mode is `subagent` or `all`, such as `explore`
  and `review`). The subagent is a separate conversation that runs in the background; the tool
  answers at once with its id. At most 4 run per conversation (`subagents.max_concurrent` in
  `config.json`). A subagent gets the workspace, rules and skills but not the parent's history,
  runs on the parent's model and variant unless its definition names one, never gets `ask` or
  `task`, and cannot write when it or its parent is read-only or `plan`.
- **Results back to the parent**: a finished subagent's result is added once to the parent
  conversation as a bounded message marked as untrusted subagent output, and published on the
  parent's stream as `message.received` with `source: "subagent"`. By default it arrives at the
  parent's next turn; `delivery: "steer"` adds it to the running turn at its next safe point, and
  `delivery: "interrupt"` restarts the parent's turn on it. A parent waiting for your answer is
  never interrupted, and your own queued messages always come first. An idle parent starts a turn
  as soon as a subagent finishes. Results survive a restart and the deletion of the subagent.
- **Steering a subagent**: the parent's agent can use `steer_subagent(child_id, message, mode)`.
  `steer` adds guidance to the subagent's running turn before its next model request, after all
  the tool results of its current step; `interrupt` cancels that turn and starts a new one with the
  message, keeping the same subagent.
- **Jev watchdog**: every 5 minutes (`subagents.watchdog_interval_secs`, 0 turns it off) Jev checks
  each running subagent from its goal, how long it has been quiet, its last tool call and its step
  count. `stuck` stops it and hands its partial work to the parent; `doubt` pauses it and asks the
  parent's agent, which answers with `decide_subagent(child_id, continue|stop)`. Jev failing to
  answer twice in a row counts as doubt.
- **You stay in control**: a subagent's permission requests appear in the parent conversation and
  are answered there; approvals stay with that subagent. You can read and stop a subagent but not
  write to it. `POST /v1/sessions/{parent}/children/{child}/continue` gives a subagent that ran out
  of steps one more budget, or releases a watchdog pause. Stopping a conversation stops its
  subagents; deleting it deletes them.
- **Subagents in the terminal client**: subagents are listed under their conversation with their
  goal and state (running, waiting for permission, out of steps, in doubt, done, failed, stopped).
  `alt+s` opens a searchable list, also below 80 columns; `enter` opens a subagent's conversation
  read-only, `alt+w` goes back. Stop and `alt+c` (continue) ask for confirmation. A finished
  subagent shows as one line in the parent conversation.
- **Events**: `subagent.spawned`, `subagent.done` and `subagent.watchdog` on the parent's stream;
  the event catalog has 22 kinds. Session lists show `parent_session_id`, `child_status` and
  `promptable: false` for subagents.

## [0.3.0] - 2026-10-03

Navai becomes a terminal coding agent as well as a navigator: a chat client with tabs, tool
permissions you answer from the terminal, a guarded `plan` agent, model switching inside a
conversation, and automatic compaction. Background subagents are planned for 0.4.0.

### Added

- **Terminal chat client**: `navai tui` starts a private server, or `--url` connects to a running
  server (set `NAVAI_SERVER_TOKEN` when it requires a bearer token). Open up to nine conversations;
  each keeps its draft and scroll position and continues streaming in the background. The left
  sidebar lists conversations and unread activity; the right panel shows agent, context, skills and
  changed files on terminals at least 140 columns wide (`alt+r` toggles it). The sidebar is hidden
  below 80 columns. `alt+1`–`alt+9` switch tabs, `alt+w` closes one, `ctrl+n` starts a conversation,
  `ctrl+o` opens the session list, `alt+b` hides or shows the sidebar, `f2` picks a model, `tab` on
  an empty message switches build/plan,
  `shift+tab` changes focus, and `ctrl+q` quits.
- **Session commands**: `/help`, `/model [alias]`, `/new [alias]`, `/stop`, `/context`,
  `/variant [name|off]`, `/compact [focus]` and `/agent [id]`. The TUI command palette combines
  these with local commands. The event catalog has 19 kinds. A line beginning `//` sends a literal
  slash, so `//help` sends `/help`.
- **Model switching and variants**: `/model <alias>` switches the current conversation; `/new <alias>`
  starts another. A switch uses the target model's default variant and waits until the current turn
  ends. Model lists and `f2` switch the live conversation; `ctrl+n` in the model picker starts a new
  conversation on that model. `/variant` lists the session's choices; `/variant <name>` selects one,
  and `/variant off` clears it.
- **Automatic compaction**: when the next request will not fit the input budget, Navai summarizes
  the older conversation and continues the same turn. If even the required history cannot fit, the
  turn ends with an error recommending a model with a larger context window. `/compact [focus]` also
  requests compaction manually. Compaction is not run while an answer or permission decision is
  pending.
- **Tool permissions**: the eight registry tools are `read`, `write`, `edit`, `bash`, `grep`, `glob`,
  `fetch` and `ask`; sessions also provide a `skill` tool to load a listed skill. `navai run` does not
  approve tool permissions automatically: it prints `permission_required: <tool> <scope>` and exits
  with code 3. `--yes` approves each request once. Ctrl-C interrupts a run and exits with code 130.
- **Plan agent guard**: `plan` can write Markdown files without a permission prompt. Other writes,
  edits, shell commands and fetches are checked against its role; if the check cannot confirm the
  action, Navai asks for permission. The guard is advisory, not a sandbox.
- **Model catalog**: when a model alias has no configured context window, Navai can discover one from
  the provider's model list and cache it under the data directory. `GET /v1/models` reports the
  resolved window and its source.
- **Workspace tools**: `glob` searches for files without following links, with a default limit of
  200 results (up to 1,000). `fetch` retrieves HTTP(S) text or HTML, caps the downloaded body at 5 MiB,
  and follows only same-origin redirects.
- **Terminal themes**: Ayu Dark, Ayu Light, Nord Dark and Nord Light. `/theme` opens the picker; the
  choice is saved in `~/.config/navai/config.json`.

### Changed

- **State locations**: configuration is stored in `~/.config/navai/`; data is stored in
  `~/.local/share/navai/`. Navai migrates supported files from the former `navigator` and
  `rust-agent` directories on first start without overwriting existing files.
- **Project identity**: the crate and binaries are named `navai` and `navai-lsp`; the VS Code
  extension is Navai (`pixmants.navai`). Environment variables use the `NAVAI_*` prefix.

### Fixed

- A permission answer is tied to the checked destination. If a tool call resolves to a different
  destination when it runs, Navai asks again rather than using the earlier approval.
- A stopped or interrupted permission request is settled without running its held tool call.
- The terminal client keeps each open conversation's model, variant and context display in its own
  tab, and ignores late events from a conversation that has been closed.

## [0.2.0] - 2026-10-01

First usable release: the navigator, a pair-programming assistant in VS Code. The driver keeps the
keyboard; four layers of models watch, complete, judge and discuss.

### Added

- **Language server and VS Code extension** (`rust-agent-lsp`, `vscode-navigator/`): one process,
  LSP over stdio for the editor, a local HTTP/SSE server with a bearer token for the chat.
- **Reflex**: fill-in-the-middle ghost-text suggestions (Inception Mercury Edit 2), with a short
  context note kept current by the metacognition.
- **Instinct**: a decision pipeline in TOML and Markdown prompts, asking typed questions to a
  System One model (Jev). Project `.navigator/` overrides the user and built-in pipelines; hot
  reload on save.
- **Metacognition**: short end-of-line remarks from a fast model, with read-only `read` and `grep`
  tools under a budget, aware of the conversation, the intent record and recent edits.
- **Cognition**: a chat panel on a frontier model with the full coding-agent toolset; it guides by
  default and does the work on explicit delegation. Editor context (focused file, cursor,
  selection) travels with each message.
- **Wake**: the instinct can bring a contradiction between the code and the conversation or the
  intent record to the cognition, as a navigator-sourced message that never answers a pending
  `ask` in the driver's place.
- **Conversation triggers**: a driver turn in the chat re-checks the focused file against what was
  just agreed.
- **Intent record** (`.navigator/intent.md`), written by the cognition as decisions settle.
- **Verdicts** on remarks (useful, noise, dismissed, followed) and a JSONL decision log.
- **Replay** (`rust-agent-lsp replay`): run a candidate pipeline over the logged triggers and
  compare its decisions with the rated remarks.
- **Conversation history** in SQLite: every conversation per project, resumable under the same id,
  pending `ask` included; list, reopen and delete from the panel.

### Changed

- Data lives in `~/.local/share/navigator/` (`navigator.db`, `navigator.jsonl`), migrated from
  `~/.local/share/rust-agent/` on first start. Configuration stays in `~/.config/rust-agent/`.
- SQLite is the only history: a reconnect or a resume replays the store, then follows live events,
  with no cap on conversation length.
- Codebase restructured: `session/` service, `navigator/` split by layer, one shared model client,
  a shared test harness. See `ARCHITECTURE.md`.

### Removed

- The WebSocket transport and the placeholder `tui` command.

## [0.1.0] - 2026-09-04

The coding-agent core: the agent loop with five tools and `ask`, OpenAI-compatible streaming
providers, the event bus and the local session server.
