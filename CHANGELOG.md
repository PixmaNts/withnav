All notable changes to this project. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, a minor version may break compatibility. New entries go under `Unreleased` as pull
requests land in `develop`.

## [Unreleased]

### Added

- **VS Code chat: follow subagents.** A Subagents section lists each delegated goal and state.
  Open a child's read-only transcript in its own editor tab without leaving the parent chat,
  resuming the child or prompting it. Parent drafts and context stay unchanged while inspecting.
  Stop or continue a parked child with confirmation; refusals stay visible. Child permissions
  remain reviewable in the parent, and delivered results keep their untrusted-subagent label.

- **VS Code chat: slash commands and a command palette.** Type `/` to browse commands with
  descriptions and argument hints, using the arrow keys, Enter, Tab or the mouse. Switch the
  open conversation's model with the picker or `/model` (a running turn finishes first), choose
  its variant or agent, compact context, inspect the context breakdown, select a skill for your
  next request, stop, start fresh or show help. The current agent and a pending model switch
  stay visible near the composer. Command outcomes appear as notices; refusals keep your draft.
  Commands carry no editor context; `//text` sends a literal `/text` message.

### Fixed

- **VS Code composer lifecycle:** first and resumed messages clear their draft after delivery;
  creation failures and skill refusals remain visible. Starting fresh or restarting the server
  abandons the old creation safely. Model switches replace the previous pending choice when
  requested, and an outstanding switch survives a failed attempt to open another conversation.

- **VS Code command controls stay with their conversation.** Skill requests now confirm before
  cancelling a pending permission, can start or resume a conversation, and restore a refused
  request without replacing newer typing. Model switches remain pending regardless of HTTP/event
  arrival order; controls cannot change starting conversations or leak late errors into another
  one. Command discovery refreshes when the server starts or restarts, live variant choices use
  the conversation's offer, reopened conversations retain their agent, and watchdog resolutions
  close their pause controls.
### Changed

- **VS Code chat**: the context meter shows what the next request will send, as the terminal
  client does (`~42k / 1M · 4%`), instead of the last call's usage. Hover or focus it for the
  breakdown (system, rules, skills catalog, tools, messages), the input budget, the output reserve
  and the provider's count for the last request.

## [0.4.5] - 2026-10-04

The VS Code chat can now answer the agent's permission requests and continue a paused agent.

### Fixed

- **VS Code chat: permission requests can be answered.** Before, the chat ignored them: a
  conversation that needed your approval stayed blocked, and typing a message cancelled the
  request. A card now shows the agent, the tool, the full reason and the call (the whole command;
  a write or edit with its path, options such as `replace_all`, and its content up to 4096 bytes),
  with **Allow once** and **Deny** (and an optional message). Subagents' requests appear in their
  parent conversation. Answered, denied and expired requests stay visible when you reopen the
  conversation. Sending a message while a request is open asks you first, since it cancels it.
- **VS Code chat: subagent output is no longer shown as yours.** Only your own messages are shown
  as you; a subagent's result is labelled as untrusted data. A skill you selected shows beside
  your message.
- **VS Code chat: paused agents can be continued.** "Out of steps" offers **Continue**; a watchdog
  pause shows its reason with **Continue** and **Stop**. Refusals show the server's reason, and
  pauses from a run that has ended stay as history, without buttons.
- **VS Code chat: buttons always act on their own conversation**, including after switching
  conversations, a failed open, a server restart or reopening the panel; an answer is never sent
  twice.

### Added

- **VS Code chat**: tool rows show who approved a reviewed call (Jev or metacognition), with the
  reason and Jev's verdicts in an expandable note, like the terminal client.

## [0.4.4] - 2026-10-04

The per-call review now treats your own request as what you want, and the terminal client shows
who approved each call.

### Changed

- **Your request counts in the permission review**: Jev and metacognition receive your latest
  request as your intent, so a write you explicitly asked for (for example a file in your home
  folder) can be approved instead of refused as "untrusted". Its text is never an instruction to
  the reviewers, and claims made by the agent, a subagent or a tool still prove nothing. Navai's
  config directory still always asks you.
- **Incomplete requests never authorize**: your request is sent to the reviewers in full when it
  fits; when it does not, every reviewed `bash`, `write` and `edit` call asks you.
- `git` commands stay reviewed as changes: even `git status` or `git diff` can run programs
  configured in the repository.

### Added

- **Terminal client**: a tool block says who approved a reviewed call (`auto · Jev`,
  `auto · metacognition`, `driver approved`); expanded, it shows the reason, Jev's verdicts and the
  plan role check. Copies include it.
- **Review evidence**: the permission review records Jev's verdicts, the deterministic checks that
  tightened the call, the reviewer models and their response times on the tool and permission
  events, also when you approved the call yourself.

### Fixed

- Four tests that failed now and then on a loaded machine; when `git status` for the Changes panel
  passes its deadline, Navai now ends and reaps the process itself.

## [0.4.3] - 2026-10-04

Fixes from trying 0.4.2's per-call permission review, including a security fix: metacognition could
approve writes into Navai's own configuration.

### Fixed

- **Security**: writes and edits into Navai's config directory (`~/.config/navai`: `auth.json`,
  `models.json`, `config.json`, agent definitions) always ask you; metacognition can no longer
  approve them. This also holds through symlinks and when the directory does not exist yet. A
  `bash` command that names the config directory also asks you (best effort: `bash` is not
  confined).
- **Metacognition review** no longer fails when the model reasons at length before answering: its
  answer budget is 4096 tokens. When it does fail, the permission request says why (timed out,
  unavailable, input too large, malformed answer and how, or output exhausted by reasoning), and a
  short excerpt of a malformed answer goes to the decision log. A reviewer answer with a duplicated
  field is rejected.
- **`write` and `edit` outside the project** are reviewed like `bash` instead of being refused: if
  approved (by metacognition or by you), the call runs once, for that exact file, without opening the
  folder to later calls.
- **Tool errors** always carry a message.
- **Terminal client**: permission requests can be selected and copied like other messages, in full
  (whole reason and command, no borders or truncation; a write's content up to 4096 bytes). While a
  request is open, `y` still allows it: drag with the mouse to copy it; `y` copies an answered one.

### Added

- **Scratch folders**: the agent can write temporary files without asking in two folders available
  in every project: a private one, `$XDG_RUNTIME_DIR/navai` (or `~/.cache/navai/scratch`), used
  first, and the shared `/tmp/navai`. Each must be a real directory owned by you with mode 0700; a
  folder that is replaced, made public or reached through a symlink loses this exemption.
  `permissions.scratch_dirs` in `config.json` sets the list of absolute paths; `[]` turns them off.

## [0.4.2] - 2026-10-04

Every file change and shell command is now reviewed one by one, conversations no longer stop after
50 steps, skills can be picked from a list, and the conversation can be selected and copied.

### Added

- **`/skills`**: opens a searchable list of the conversation's skills. Choosing one fills
  `/skill <name>`; add your request and send: the skill's instructions are loaded into your message
  before the agent's first model request, and the selection shows next to your message, also on
  replay. If the agent cannot use skills, the whole message is refused.
- **Select and copy the conversation**: drag with the mouse to select conversation text, or
  double-click a word; releasing copies it. Wrapping, the sidebar, borders and tool labels stay out
  of the clipboard. A selection across several messages names each sender (`You:`, the agent's name,
  `Navigator:`…), once per run of consecutive messages; a selection inside one message copies only
  its text. With the conversation focused, `y` copies the selection, or the whole selected message
  or tool output; `ctrl+shift+c` works where the terminal sends it, and `ctrl+c` still stops. The
  wheel scrolls the conversation, and the lists of `/model`, `/sessions` and other pickers.
  Copying uses OSC 52, so it works over SSH when the terminal supports it; under tmux set
  `set -g set-clipboard on`. `shift+drag` keeps the terminal's own selection.

### Changed

- **Per-call permission review**: every `bash`, `write` and `edit` call is reviewed before it runs,
  instead of an "allow folder" answer letting every later call through. Jev classifies the call: a
  read-only call, or a low-risk change inside the project, runs; a risky or outside-project change
  needs metacognition's approval; anything else asks you, with the reason. "Allow folder" is gone
  for these tools and the bash/file grants saved by 0.4.1 are ignored; `fetch` keeps its origin
  grants. `permissions.automatic_review: false` in `config.json` asks for every such call; an
  invalid value fails closed. The review is advisory, not a sandbox.
- **No more 50-step limit**: a conversation's turn runs until it ends, is stopped, or the watchdog
  stops it. Every 5 minutes (`agent.watchdog_interval_secs`) Jev checks it is making progress. Stuck:
  the turn stops after its current tools finish, keeping the work done. In doubt: metacognition
  decides; if it hesitates too, the turn pauses and asks you (`alt+c` continues, a new message
  replaces the paused turn). With the watchdog off (`0`) or Jev unavailable, a 500-step cap applies,
  renewed only by you. `NAVAI_MAX_STEPS` still sets an explicit cap. Subagents keep their 50-step
  budget.

### Fixed

- **`grep` tool**: a pattern can no longer be read as a ripgrep option (such as `--pre`, which runs
  a program), and ripgrep configuration files are ignored.

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
