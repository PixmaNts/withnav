All notable changes to this project. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, a minor version may break compatibility. New entries go under `Unreleased` as pull
requests land in `develop`.

## [Unreleased]

### Fixed

- **Long conversations compact with relevance pruning and learned message limits.** Instinct
  scores only old, unpinned tool outputs before metacognition writes its rolling summary;
  unavailable scoring leaves the existing path intact. Provider size limits survive new sessions,
  and failed `/compact` commands no longer duplicate their error in VS Code.

- **VS Code answers show text and tool calls in the order they happened.** Each model step
  keeps its own Markdown and reasoning; consecutive tools share a foldable progress group.
  Permission, pause and question cards stay visible, and replayed tool groups start closed.

- **VS Code command results stay concise.** Selection changes no longer dump JSON;
  unknown command data stays available in a closed Details disclosure.
- **File references in VS Code answers open at the referenced line.** Bare and inline-code
  paths become keyboard-accessible links; relative paths try every workspace root,
  and missing or outside-workspace files explain why they cannot open.

- **VS Code answers render GitHub-flavoured Markdown safely.** Tables, quotes, nested
  and task lists, strikethrough and hard breaks render without interpreting raw HTML.
  Streaming redraws are coalesced per animation frame.
- **Permission mode is visible without cluttering your conversation.** Safe mode and
  Bypass active replace the ambiguous toggle labels; mode changes update the header
  without adding transcript lines in the editor, browser or terminal.

- **Cognition and Settings text scales with your editor.** Secondary text scales too,
  and conversation, tool and card content uses the readable theme foreground.
  Sidebar conversations retain the sidebar surface; child editor tabs use the editor surface.
### Added

- **Allow WithNav config updates for the rest of a conversation.** Protected-config
  requests offer a session-only answer in the terminal, editor and browser. Reviews still
  run; credentials, review settings and changed scratch roots still require your answer.

- **Metacognition writes compacted conversation memory in rolling chunks.** Summaries use the
  assigned model's window and fall back to the session model on provider errors; configuration
  can opt out or tune chunk size. Compaction events report the writer and successful passes.

## [0.7.1] - 2026-10-09

WithNav 0.7.1 lets you delete a saved session, with its subagent sessions, from the terminal,
VS Code and the WebUI. The WebUI now fits tablets and phones, and the VS Code panel shows in
colour what waits on you, how subagents are doing and how big a change is. The decision layer is
named Instinct everywhere, and about forty fixes land across tools, the terminal client, the
language server, context budgeting and providers.

### Added

- **Delete saved sessions from the terminal session picker.** Press `ctrl+d`, then confirm
  permanent deletion with the attached subagent count. The list refreshes near the same row,
  and tabs for the deleted session and its subagents close.
- **Delete VS Code conversations from the command palette.** Choose a workspace conversation,
  then confirm permanent deletion including its subagent count. The chat button uses the same
  confirmation; deleted conversations reset to an empty chat and their subagent views close.
- **The WebUI can delete a session, with its subagent sessions.** Each session in the sidebar has
  a delete button (shown on hover or focus; the Delete key works on the row), and the command
  palette has "Delete session…" for the open one. The confirmation lists the subagents that go
  with it and says it cannot be undone; deleting the open session moves to the next one.

### Changed

- **Permission review and supervision now use the Instinct layer name.**
  New events write `instinct` source, model, latency and guard metadata; clients
  still read stored `jev` events and display them as `auto · instinct` until 0.8.
- **The VS Code panel colours what waits on you, how subagents are doing, and how big a change is.**
  Pause cards and pending permissions get the warning colour; a subagent's state reads blue while
  running, amber when it needs you, green when done, red when failed; a wake shows `+N −M` and its
  diff lines in your theme's git colours. All from VS Code theme colours, AA contrast in Dark+,
  Light+ and High Contrast.

### Fixed

- **VS Code's layer protocol now includes variant discovery diagnostics.** The optional server
  field is represented in the extension's types instead of being omitted from its contract.
- **Unexpected VS Code chat message errors are reported instead of escaping the dispatcher.**
  The host boundary now catches synchronous failures as well as the existing asynchronous errors.
- **The example default agent has an explicit context window.** Startup preflight no longer
  needs online catalogue discovery to determine its capacity.
- **Brace glob alternatives can explicitly search normally skipped folders.**
  Patterns such as `{src,target}/**/*.rs` now enter `target`, while broad wildcard
  alternatives still leave hidden and generated folders skipped by default.
- **Model variants cannot use the reserved name `inherit`.** Catalogue validation rejects a
  name that the CLI and commands interpret as selecting the model's default variant.
- **Oversized model catalogue cache files are ignored without reading them whole.** Disk reads
  are bounded to 2 MiB plus one byte, preventing corrupted caches from allocating unbounded memory.
- **Provider error bodies are bounded and cancellable.** Chat and non-streaming failures read
  at most 64 KiB, and Stop interrupts a slow error response rather than waiting for its end.
- **Importing OpenCode models no longer requires an OmniRoute provider.** Direct compatible
  providers can be imported; OmniRoute's catalogue format is attached only when present.
- **Invalid grep expressions return the same error on every host.**
  The ripgrep engine validates expressions before starting its process, returning
  `invalid_regex` just like the built-in fallback rather than a generic command failure.
- **Fetch explains how redirect query parameters are handled.** A redirect to a new
  path without a query drops the original query under standard URL resolution;
  parameters, including credentials, are never implicitly forwarded.
- **Closing a file now frees what inline completion kept for it.**
  The completion service remembered every file it had ever completed in until the server stopped, so
  long editor sessions grew without bound. Closing a file now drops its entry and cancels any
  completion still in flight for it.
- **Every folder of a multi-root VS Code workspace now counts as part of the workspace.**
  The language server only knew the first folder, so the navigator's read and search tools refused
  files in the others. It now knows all of them, follows folders added or removed while it runs,
  and keeps the first folder as the anchor for relative paths, the intent record and flows.
- **The TUI header fits narrow terminals.** In a split pane the agent, model and gauge ran past the
  right edge and were cut off. The row now gives way in order (path, model, variant, branch,
  gauge, pending switch, then `withnav` shrinks to `◆`, then the model_id); the agent stays visible.
- **Pickers stay inside small terminals and stay usable.** On a very short or narrow pane the model,
  command or theme picker drew past the screen edge with overlapping rows. The popup now fits the
  screen; as it shrinks it keeps the highlighted row first, then the filter, then `esc`.
- **Sending a message from VS Code no longer breaks on a conversation id with special characters.**
  The extension built the message URL from the raw id while every other call encoded it, so such an
  id produced a malformed request and a 404.
- **`edit` now refuses an empty `old_string` instead of scrambling the file.** An empty search text
  matches between every character, so `replace_all` interleaved the new text through the whole file
  and a single replace failed with a confusing "ambiguous match". The call is now rejected up front.
- **One unreadable conversation no longer empties the session list.** A session whose stored
  metadata cannot be decoded made `GET /v1/sessions` fail with a 500, so the picker showed none of
  the workspace's healthy conversations. The list now skips it and logs a warning.
- **Inline completion works on files with Windows line endings and at the end of a file.**
  The cursor position was converted to a byte offset assuming one byte per line break, so in a CRLF
  file the code sent to the completion model drifted one byte per line; and a cursor on the empty
  line after a final newline got no completion at all.
- **Remarks survive explanatory JSON before the final answer.** The navigator skips unrelated
  objects in prose and code fences and uses the last answer-shaped object, while still rejecting
  malformed lines and empty remark text.
- **Verbose shell commands drain excess output in larger chunks.**
  Once the output buffer fills, larger discard reads reduce reader overhead without artificial
  delays or closing the pipe. Retained bytes never exceed the buffer cap.
- **Shell cancellation only signals positive process group ids.**
  Child ids are checked before conversion, and termination rejects nonpositive ids so an invalid
  id cannot signal WithNav's own process group.
- **The terminal client no longer fails to load skills or toggle bypass for a session whose id needs
  escaping.** Those two requests put the id in the URL path as-is while every other one escaped it, so
  an id with a space or a slash gave a 404.
- **VS Code keeps watchdog-paused subagents continuable without a root pause token.**
  Removed an unreachable tooltip that incorrectly implied child continuation
  required a token; the server accepts tokenless child continuation.
- **The intent tool can clear an obsolete section with an empty string.** An explicit empty
  update now removes the previous text; omitted sections still retain their contents.
- **Interrupted terminal turns no longer retain unfinished tool arguments.** Turn and session
  boundaries discard streamed fragments so a later turn starts with an empty buffer.
- **"Allow folder" grants from two conversations no longer cancel each other out.** Two sessions
  of one workspace saving a grant at the same moment could each overwrite the other's entry in
  `grants.json`, so a folder you had allowed asked again after a restart. Saves are now serialised
  with a lock file next to it.
- **Metacognition preserves home-relative and platform-absolute tool paths.** Only ordinary
  relative paths are anchored to the workspace; home expansion and workspace confinement remain
  the responsibility of the shared path validator.
- **The WebUI now fits tablets and phones.** Below 1100 px the Intent / Crew / Extensions panel
  opens from a **Details** button in the top bar; below 700 px the page is one column and the
  workspace list opens from the ☰ button. Settings and the Studio work the same way. Esc closes
  a side panel and puts focus back on its button. The desktop layout does not change.
- **Subagents no longer pause merely because instinct is unconfigured.** A watchdog without a
  judge skips advisory checks; configured judges still escalate repeated failures, and the step
  cap remains enforced.
- **Deleting lines now moves the edit point the navigator reports.**
  A pure deletion left the "last edited line" on the previous edit, so remarks and context excerpts
  looked at stale code. The deletion point is now used, and the edit history records it.
- **Creating files avoids resolving the same permission root twice.** Path validation
  reuses its canonical ancestor when that ancestor is an allowed root. Other roots
  are still resolved freshly so changed symlinks cannot retain stale permissions.
- **Relative file destinations can name folders that do not exist yet.**
  Path validation anchors relative destinations to the first allowed root before resolving
  existing ancestors, while still refusing paths that escape the allowed roots.
- **Metacognition call logs retain provider-reported token usage.** Successful chat rounds now
  record input and output counts; tools and responses without usage keep these fields unknown.
- **Code-heavy conversation summaries no longer exceed their cap during escaping.** Angle
  brackets become same-size square brackets, keeping generic types and HTML summaries bounded
  without allowing summary text to close its data wrapper.
- **A model with an 8k context window can be assigned again.** The safety margin was never below
  4096 tokens, so an 8k model with a 4k output limit was refused and one with the default reserve
  kept only a quarter of its window for input. Windows under 32k now use an eighth of their size as
  the floor (1024 tokens at 8k); windows of 32k and above are unchanged.
- **You can switch off a failing model even when the conversation is too big for the new one.**
  `/model` summarises the conversation with the current model before moving to a smaller window, so
  a revoked key or a provider outage left you stuck on the broken model. When the current model
  cannot answer, the model you are switching to now writes the summary. It cannot rescue a
  conversation whose text that cannot be cut down (messages, `ask` answers, loaded skills; only
  other tool outputs can be) is larger than the new model's window: the switch is then refused.
- **An OpenCode agent file named like a former WithNav agent no longer replaces a crew member.**
  `~/.config/opencode/agents/explorer.md` (or `librarian`, `oracle`, `explore`, `review`) configures
  OpenCode's own agent; WithNav read it as the former id of Mousse, Vigie or Lux, which lost its
  built-in prompt. Those files are now skipped. The former ids still work in
  `~/.config/withnav/agents/` and `.withnav/agents/`.

## [0.7.0] - 2026-10-08

Navai is now **WithNav**. Nav is your single point of contact, with a crew behind it: Vigie scouts
and cites its sources, Lux weighs hard decisions, Mousse handles small local edits, and the
Cartographe and the Sondeur come on demand. You are the Captain. The CLI is `nav`, the language
server `withnav-lsp`, the extension `pixmants.withnav`, and Tide Dark is the default theme. Your
config and data move to `withnav` on first start; every old name, directory, variable and agent id
keeps working with a warning until 0.8. This release also brings InstinctFlow: draw the flows the
instinct runs, in the new Studio, and the legacy decision pipeline is gone.

### Rebrand to WithNav

Write new names throughout 0.7; legacy readers warn or give a migration hint.
Removal of the deprecated names is planned for 0.8 ([ADR 0014](docs/adr/0014-rebrand-withnav.md)).

- **Product and crate:** Navai → WithNav (withnav.dev), crate `rust-agent` → `withnav`.
  No old crate alias. The GitHub repository moves to `github.com/PixmaNts/withnav`
  (the old URL redirects); the internal `src/navigator/` module stays.
- **Binaries:** `navai` → `nav` (deprecated `navai` alias prints a notice and runs `nav`);
  `navai-lsp` → `withnav-lsp` (update the configured server path; no old LSP executable alias).
- **Project directory:** `.navai/` / `.navigator/` → `.withnav/`, including `intent.md`,
  `agents/`, `flows/` and legacy `pipeline.toml` / `prompts/`. When `.withnav/` is absent,
  read `.navai/` then `.navigator/`. New Mermaid charts go in `.withnav/charts/`.
- **Config/data directories:** `~/.config/navai/` → `~/.config/withnav/` and
  `~/.local/share/navai/` → `~/.local/share/withnav/`. Move legacy files on first start
  without overwriting new ones; existing older `rust-agent` / `navigator` migrations remain.
- **Scratch paths:** `$XDG_RUNTIME_DIR/navai`, `~/.cache/navai/scratch` and
  `<temp_dir>/navai` → the corresponding `withnav` paths, retaining old-path compatibility in 0.7.
- **Data filenames:** `navai.db` → `withnav.db`, `navai.jsonl` → `withnav.jsonl`;
  move on first start when only the old file exists, keeping database sidecars together.
- **Environment:** `NAVAI_*` → `WITHNAV_*`, with the old variable read and warned about
  when its new counterpart is unset. This includes `NAVAI_CONFIG_DIR`, `NAVAI_SERVER`,
  `NAVAI_SERVER_TOKEN`, `NAVAI_TOKEN`, `NAVAI_WORKSPACES`, `NAVAI_MAX_STEPS`,
  `NAVAI_BASH_GRACE_MS` and `NAVAI_HTML_CONVERTER`, each becoming the same suffix
  under `WITHNAV_`. Provider-specific API-key variables are unchanged.
- **VS Code:** `vscode-navigator/` → `vscode-withnav/`, extension id `pixmants.navai`
  → `pixmants.withnav`, view container `navigator` → `withnav`; install the new extension
  (no duplicate folder or extension-id alias). Settings `navai.*` → `withnav.*`, falling
  back when the new setting is unset. Commands `navigator.*` / `navai.*` → `withnav.*`;
  use new command ids. The previously removed `navai.modelAlias` stays removed.
- **LSP:** `navigator/serverInfo`, `navigator/remark`, `navigator/clear`, `navigator/status`,
  `navigator/feedback`, `navigator/diagnostics`, `navigator/intent`, `navigator/reflexShown`
  and `navigator/inlineCompletion` → the same suffix under `withnav/`. The server accepts
  both prefixes for requests/notifications and sends only `withnav/*`; clients use the new prefix.
- **Human and wire values:** driver → the Captain; `driver` → `captain` in event authors,
  `approved_by`, permission-gate `source`, `origin`, `selected_by` and conversation digest
  authors. Readers accept both; new records write `captain`.
- **Flow action:** `ask_driver` → `ask_captain`; old TOML actions still parse.
- **Agent ids:** `build` and `plan` remain Nav; `explore` / `librarian` → `vigie`,
  `review` / `oracle` → `lux`, `explorer` → `mousse`. Old ids resolve as deprecated aliases
  with warnings. New `cartographe` and `sondeur` are on demand, with no old aliases.
- **Crew and labels:** Nav · The Navigator, Vigie · The Lookout, Lux · The Expert Advisor,
  Mousse · The Lightweight Deckhand, Cartographe · The Cartographer and Sondeur · The Deep Diver.
  Metacognition remarks are Beacons signed by Nav. Layer technical names stay unchanged:
  Reflex · Completion, Instinct · Triage, Metacognition · Beacon, Cognition · Nav and the crew.
  Feature labels are Logbook · Activity history, Voyage · Session, Safe Harbor · Checkpoint.
- **Default theme:** Tide Dark when no theme choice is saved; saved choices remain intact.

### Added

- **InstinctFlow.** Draw flows the instinct runs on WithNav's events: a trigger, decisions
  (noul, choice, score), expressions and actions, wired as a graph. Flows live in
  `.withnav/flows/` (after you trust the workspace) or `~/.config/withnav/flows/`; Save writes the
  file and Reload makes it run. A blocking `tool.pre_call` flow can deny a tool call or ask you,
  never allow one past the normal permission review, and bypass never answers it.
  `message.received` and `turn.end` flows can inject a note or wake the cognition, marked as
  coming from a flow. See [the Captain's guide](docs/instinct-flow.md).
- **The Studio** (`#/studio` in the browser): canvas and TOML views of the same file, an inspector
  per node, Run sample against the instinct, Replay of recorded runs without a model call, and
  24-hour health per flow.
- **`instinct-flow`**, a standalone engine crate and command (`validate`, `run`, `replay`) that
  runs without WithNav, and `nav flows catalogue`, which prints WithNav's triggers, inputs and
  actions.

### Removed

- The legacy navigator decision pipeline and fallback evaluator. Built-in editor flows now own
  triggers exclusively; custom `pipeline.toml` files are converted once with originals retained.
  Offline replay uses flows only (`--flows` remains accepted; `--pipeline` and `--reask` are removed).

## [0.6.1] - 2026-10-07

Navai gets a bypass mode: one session can skip ordinary permission prompts, from the terminal,
VS Code, the browser or `navai run --yes`, while Navai's own configuration, credentials and keys
still ask you. The browser sidebar can now collapse every workspace and hide one, the transcript
uses one column, VS Code's panel uses theme colours, and two permission gaps are closed. No
configuration change is needed to upgrade from 0.6.0.

### Added

- **Bypass ordinary permission prompts for one session.** Turn it on with `/bypass on`, `navai run
  --yes`, or `POST /v1/sessions/{id}/bypass`: `write`, `edit`, `bash` and `fetch` calls that would
  have asked run once instead, after the usual reviews, and the result says `approved_by: bypass`. A
  request already waiting is answered too. Requests for Navai's own configuration, credentials and
  keys still ask you, and `navai run --yes` stops with exit code 3 on one. It is off for every new
  or resumed session, never saved, and a session's subagents follow it. It is not a sandbox: `bash`
  keeps its access to your machine.
- **Bypass in the terminal.** Alt+P (or `/bypass on` / `/bypass off`) turns bypass on or off for the
  open session. While it is on, `⚠ bypass` stays in the header and next to the tab, and each change
  appears in the conversation. Protected requests still ask.
- **Bypass in VS Code.** "Navai: Toggle Bypass Permissions" (or the button in the Cognition panel
  header) turns bypass on for the open session, after a confirmation the first time. While it is on,
  a warning badge stays in the header, and each change appears in the conversation.
- **Bypass in the browser.** The "Bypass" button next to build/plan (or the command palette) turns
  bypass on or off for the open session. While it is on, a warning badge stays on the session in the
  sidebar and in the breadcrumb, and each change appears in the conversation. Protected requests
  still ask.
- **Remove a workspace from the browser list.** The `…` menu of a workspace (or the command palette,
  for the open one) offers **Remove from list**, which keeps its sessions, and **Delete workspace
  sessions…**, which deletes every session of that folder with its subagents after you confirm the
  count. A removed folder stays out of the list after a restart and comes back when you open it
  again with "Open a folder" or start a session there. Folders passed to `navai web` on the command
  line stay in the list while that server runs.

### Changed

- VS Code's Cognition panel uses theme colours to distinguish build and plan, tools, Navigator
  wakes, subagents and permission outcomes; Settings highlights saved changes in the same style.

### Fixed

- Responses no longer stop midway when a model gateway sends `null` instead of an empty list of tool
  calls or response choices.
- **Browser transcripts use one column.** Driver messages and cognition replies share the full
  width, with bylines and a quiet Driver accent instead of right-aligned chat bubbles.
- **Every workspace can collapse in the browser sidebar.** Carets expand without switching
  workspaces, names navigate without reopening collapsed rows, and the browser remembers which
  folders are expanded. Collapse all and Expand all are also available in the command palette.
- **Network commands always get a review.** A bash call that runs `curl`, `wget`, `ssh`, `scp`,
  `sftp`, `rsync`, `nc`, `ncat` or `socat` is now treated as reaching outside the project, so it
  always goes to permission review instead of possibly running unreviewed.
- **A symlinked folder no longer slips past the Markdown exception.** Writing a `.md` file through a
  folder that is a link (for example `notes/` pointing elsewhere) now asks like any other write
  outside its real place; before, only a link on the file itself was caught.

## [0.6.0] - 2026-10-07

Navai gets a browser interface and a Settings screen in every client. `navai web` opens the chat in
your browser; Settings lets you choose the model of each layer and agent, permissions, subagent
limits and the theme from the browser, the terminal (`/config`) or VS Code, all writing the same
`config.json`. No configuration change is needed to upgrade from 0.5.1.

### Added

- **Settings in VS Code.** "Navai: Open Settings" (or the gear in the Cognition view) opens a
  Navai Settings tab laid out like VS Code's own settings editor: the same sections, saved at once
  to `config.json`, with a filter for modified settings and a link to the extension's own
  `navai.*` settings. It still opens when the model or agent list cannot load, offers Open Folder
  when no folder is open, and Retry when the language server is not running.
- **Settings in the terminal.** `/config` (or the palette) opens Settings in the TUI: gateways
  (read-only, `t` tests the connection), the model of each layer and agent and its variant, the
  default agent, automatic review and scratch directories, subagent limits and watchdogs, and the
  theme. Changes wait until ctrl+s, which writes them to `config.json` one by one; if the file
  changed elsewhere it reloads and keeps your unsent changes. Leaving with unsaved changes asks
  write, discard or stay.
- **Settings in the browser.** In `navai web`, the gear in the top bar (or "Open settings" in the
  command palette) opens Settings: gateways (read-only, with a connection test), the model of each
  layer and agent and its variant, the default agent, automatic review and scratch directories,
  subagent limits and watchdogs, and the theme. Each change is checked against the catalogue and
  written to `config.json` at once; a change made elsewhere in the meantime is reloaded instead of
  overwritten. Layer models apply after restarting Navai, the rest to new sessions and subagents.
  API keys are never shown, only where they come from.
- **Favorite models.** In both model pickers, Ctrl+F (or the star in VS Code) marks a model as a
  favorite; favorites are listed first in an always-open section. The terminal and VS Code keep
  separate lists.
- **A chat in your browser.** `navai web` starts Navai on a private local address, prints a link
  and opens it. The page lets you chat in any of the workspaces you pass with `--workspace`
  (repeatable; the current directory by default) and in the others you have worked in before; each
  one shows its git branch and whether it has changes, and a workspace's intent record
  (`.navigator/intent.md`) can be read. Use `--port` to choose the port and `--no-open` to print the
  link without opening it. The link carries a secret that is new on every run and only works on
  this computer; press Ctrl-C to stop.

### Fixed

- **Two Navai windows no longer undo each other's settings.** The terminal's theme and favorites
  saves and every Settings change now take a lock on `config.json`, so a save from one window cannot
  silently put back a value another window has just written.

## [0.5.1] - 2026-10-06

### Fixed

- **Model pickers show the hierarchy.** In the terminal `/model` picker and the VS Code chat picker,
gateways, providers and models are now indented by level. Search, collapse, keyboard navigation
and API filtering are unchanged.
- **Fold or unfold every group at once.** In both model pickers, Alt+A folds every gateway and
provider, and pressing it again unfolds them.

## [0.5.0] - 2026-10-05

Models now have explicit gateway-qualified names, while layers and agents keep their own choices.
This is a **breaking configuration and API change**: migrate existing configuration by hand before
starting this version. It does not convert your configuration automatically or create agent definitions.

### Changed

- **A catalogue, not aliases.** `models.json` describes gateways, models, APIs, capabilities, limits
  and variants. Use names such as `omniroute/combo/gpt-6-astra`: `omniroute` is the gateway and
  `combo/gpt-6-astra` is the exact upstream id. Bare ids and the old `default`, `fast` and `cognition`
  aliases are no longer supported. API selections and context/usage metadata use `model`, `gateway`
  and `upstream_id`, not `alias`.
- **Assignments live in `config.json`.** `layers` selects one reflex (`fim`), instinct (`systemone`)
  and metacognition (`chat`) model; cognition runs agents rather than one singleton model.
  `agents` assigns their models and variants, taking precedence over Markdown `model:` headers.
  `default_agent` is `build` when absent. An invalid default agent or unusable default chat model
  stops server/LSP startup with a clear error; an invalid nondefault assignment warns and falls back.
  Missing or incompatible lower layers are disabled with reasons and retain the existing fallbacks.
- **Editor context is not an agent role.** Editor sessions add the pair-programmer prompt, intent
  tool and navigator conversation feed to the chosen agent. They start on the same default agent
  as terminal sessions; children do not inherit the editor attachment.
- **Model choices stay with each agent in each conversation.** `/model <gateway/id>` changes the
  current agent's model in place; `/agent <id>` restores that agent's saved choice. Model and variant
  overrides survive reopening and never edit global assignments. `/new [agent]` starts fresh without
  the old session's overrides. Unassigned children inherit their parent's effective model and variant;
  children with their own model assignment use it instead.
- **Variants have three choices:** a name, `off` (explicitly no variant), or `inherit` (remove the
  local variant choice). Changing model takes the destination model's default variant. Metacognition
  prefers `low` when offered. The importer gives OmniRoute combo models `low`, `medium` and `high`
  request-body variants, not model-name suffixes, and preserves manually declared variants.
- **Model pickers in the terminal and VS Code group by gateway, then upstream provider.** Groups
  collapse, but search includes models inside collapsed groups. New conversations choose an agent
  first, with an optional model override. The terminal's right panel and VS Code's header tooltip
  show lower-layer assignments and the current agent's choice, including override markers.
  The old `navai.modelAlias` setting is removed.

### Added

- **Readable archives for history whose model identity cannot be proved.** These conversations
  remain available to read, with a reason why resume is disabled; they are never silently attached
  to a different model. Existing session ids are preserved. This is history handling, not automatic
  configuration migration.
- **Assignment metadata for clients:** `GET /v1/agents` reports configured/effective choices and,
  with a session id, local overrides; `GET /v1/layers` reports the three lower layers and disabled
  reasons. Alongside `GET /v1/models`, these expose canonical identities and choice sources, not
  endpoint URLs, headers or credentials.
- **Example `oracle` and `librarian` agents**, with matching catalogue entries and assignments.
  Install their Markdown definitions separately; an assignment alone does not create an agent.

### Fixed

- **Agent changes apply the destination policy consistently:** its prompt, tools, skill availability
  and context budget are measured together. Deferred selections cannot land on a different agent,
  and a refused change leaves the current choice intact.
- **Compaction needed for a selection change is saved atomically with that change**, including the
  agent, model, variant, per-agent overrides and ordered events. A failed save or cancellation before
  commit leaves the previous selection and checkpoint intact; resume restores the committed state.

## [0.4.7] - 2026-10-04

Fixes from trying 0.4.6 in VS Code: subagents work there, reading a file outside the project is
reviewed instead of refused, lists are searchable and compaction shows its progress.

### Fixed

- **VS Code conversations can start subagents.** The agent of a VS Code conversation now has the
  `task` tool (and `steer_subagent`, `decide_subagent`), like the terminal client. Subagents'
  results reach the conversation as before; they never become the navigator's conversation.
- **Reading secrets is never approved on Jev's word.** A read-only call that is risky, such as
  reading a key or a credentials file, now goes to metacognition and then to you. Reading
  `~/.ssh`, `~/.gnupg`, `.env` files, `*.pem` / `*.key`, cloud credentials or Navai's
  `auth.json` with the `read` tool always asks you. `grep` and `glob` skip these files (without
  opening them) and say how many they skipped. For `bash`, this detection is best effort.
- **VS Code lists are searchable.** The model, skill, variant, command and conversation lists open
  with a search field that matches names, model ids and descriptions, shows the number of matches
  ("12 of 300") and is chosen with the arrow keys and Enter. The `/` list filters from the composer
  as you type. Choosing a model never touches your draft.
- **VS Code compaction no longer looks frozen.** `/compact` shows "Compacting the conversation…"
  until it ends or fails. You can keep writing and sending: your messages wait for the end of the
  compaction.

### Changed

- **`read` outside the project is reviewed like `bash`** instead of being refused: if Jev,
  metacognition or you approve it, it reads that one file once. `grep` and `glob` outside the
  project are still refused, with a message pointing to `read` or `bash`.
- A `bash` command that writes into Navai's config directory (`~/.config/navai`) always asks you,
  even with options hidden in quotes, escapes or braces; only plain reads such as `cat`, `ls` or
  `wc` there can be approved automatically.
- "Allow folder" is only offered for `fetch`; read, write, edit and bash requests offer allow once
  or deny.
- Directory `grep` uses ripgrep as before (fast, respects `.gitignore`); without ripgrep, its
  fallback is limited in size and time and can be cancelled. More than 1000 matches is reported
  as a partial result.

## [0.4.6] - 2026-10-04

The VS Code chat gets the terminal client's controls: slash commands, subagents and the context
gauge. The server is unchanged.

### Added

- **VS Code chat: slash commands.** Type `/` to open a list of commands with their descriptions
  (arrow keys, Enter or Tab, Esc, or the mouse). `/model` switches the open conversation's model,
  and the model picker now works on an open conversation too; while the agent is answering, the
  switch waits for the end of the turn and shows as pending. Also `/variant`, `/agent` (the current
  agent is shown near the composer), `/compact`, `/context`, `/stop`, `/new` and `/help`. `/skills`
  lists the conversation's skills; `/skill <name> <request>` sends your request with that skill.
  A command is never sent to the model as text, nor with the editor context; `//text` sends
  `/text` as a message. A refused command or skill keeps your draft.
- **VS Code chat: subagents.** A Subagents section under the conversation lists each subagent with
  its goal and state. **Open** shows a subagent's conversation in its own read-only editor tab,
  without leaving or changing your conversation. **Stop** and **Continue** (out of steps, or paused
  by the watchdog) ask for confirmation first. A subagent's permission requests are still answered
  in the parent conversation.

### Changed

- **VS Code chat: the context meter shows what the next request will send**, like the terminal
  client (`~42k / 1M · 4%`), instead of the last call's usage. Hover or focus it for the breakdown
  (system, rules, skills catalog, tools, messages), the input budget, the output reserve and the
  provider's count for the last request.

### Fixed

- **VS Code chat**: commands, model switches and subagent controls always act on the conversation
  they were started from, including after switching conversations, a failed open, starting a new
  one or a server restart; a sent message always clears your draft, and a control is sent only
  once.

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
