# Architecture

The map of WithNav's codebase: what runs, which module owns what, how data flows, and where new code
goes. Vocabulary is defined in [`CONTEXT.md`](CONTEXT.md); the decisions behind this shape are in
[`docs/adr/`](docs/adr/). Read this before adding a module or a helper: most past duplication came
from agents who did not know the piece already existed.

The runtime map below describes the implementation prepared for **0.5**, not which binaries
are installed on the Captain's machine. See
[ADR 0011](docs/adr/0011-layers-models-and-agent-overrides.md) and the contract section below.

## What runs

```txt
VS Code ── vscode-withnav (TypeScript)
   │   ├─ LSP over stdio ─────────────────┐  edits, saves, diagnostics, inline completion, remarks
   │   └─ HTTP + SSE, bearer token ───┐   │  the conversation with the cognition
   ▼                                  ▼   ▼
nav lsp   (one process, two channels: ADR 0006)
   ├─ navigator/   Language Server, the four layers, editor flows, log, replay
   ├─ server/      session HTTP/SSE API, SQLite conversation history
   ├─ bus/         per-session sequencer, event envelopes, inboxes, the agent runtime task
   ├─ agent/       the coding-agent loop (the cognition runs on it)
   ├─ tools/       eight registry tools: read, write, edit, grep, glob, fetch, bash, ask; each session adds `skill`
   ├─ mcp/         service-owned stdio clients; ready tools frozen into each session registry
   └─ provider/    model access: config, auth, one HTTP client, chat SSE, FIM, System One
```

The single binary, `nav` (`src/main.rs`, `src/app.rs`), is a thin CLI over the same library:
`lsp` (the editor server above, or offline `lsp replay`), `run` (one task in a terminal),
`serve` (the session server alone), `auth`, `tui` (the terminal
chat client, below) and `web` (the browser chat client, below). `nav run` exits 3 when permission is required and 130 when interrupted.

## Modules

Dependencies point down this list. A module never imports one above it.

| Module | Owns | Does not own |
| --- | --- | --- |
| `mcp/` | global stdio MCP servers: configuration under `mcp` in `config.json` (`config.rs`), the shared `McpManager`, bounded discovery and process-group supervision (`connection.rs`), provider-safe `mcp__<server>__<tool>` names and schemas, cancellable bounded calls (`tool.rs`), output redaction (`redaction.rs`) | permission policy (an MCP tool is decided like any other), session persistence, HTTP transport, live refresh |
| `permission/` | the permission model of ADR 0015, shared by the agent loop and `nav permissions`: the SQLite tables in `withnav.db` (`store.rs`), rules and their matching (`rules.rs`), the decision history (`decisions.rs`), skill and flow validations (`validation.rs`), shell splitting and arity prefixes (`shell.rs`), the fixed floor (`floor.rs`) and the decision order (`decide.rs`: a pure `decide` over a `PermissionFlows` trait, plus `DoomTracker`) | running tools, flows, events |
| `flows/` | the flow store (built-in, global and workspace definitions, validation-gated loading, reload marker; `store.rs`), the runtime that evaluates a trigger, aggregates the permission verdicts and runs the compiled-in protected check (`runtime.rs`), the built-in flows (`builtin/`), run records, redaction, the decision client to Instinct | the decision order, which lives in `permission/` |
| `app/` | `nav` command dispatch; `entry.rs` selects stderr-only LSP logging; `lsp.rs` dispatches editor startup and offline replay | language-server wiring and replay reports, which live in `navigator/` |
| `settings.rs` | the user settings file `<config_dir>/config.json`: `load`, atomic `save` that keeps keys it does not know; each client reads its own key (`tui.theme`) | what a setting means |
| `provider/` | `models.json` catalogue (`config.rs`, `load_models`), canonical identities (`model_name.rs`: `ModelName`), pure assignment resolution (`assignments.rs`) and lower-layer clients (`layers.rs`), keys (`auth.rs`), one `ModelClient` per endpoint (`client.rs`: auth header, deadline, cancellation, status errors), the chat SSE decoder (`sse.rs`, `mod.rs`), the FIM and System One wire clients | anything navigator-specific: no `crate::navigator` import |
| `tools/` | the tool trait, registry, path validation (`validate_path`), and eight registry tools; `glob.rs` provides bounded file search, `fetch.rs` provides bounded HTTP(S) GET, `task.rs` starts a background subagent, `nofollow.rs` opens the bound write/edit destination without following links; file tools use `ToolContext`'s writable and read-only roots (`read`/`grep`/`glob` accept both, `write`/`edit` only the writable roots); `bash` starts in the workspace but is not confined to these roots | which tools a session gets; the per-session `skill` tool is installed by `agent/` |
| `agent/` | `AgentActor` (transcript, the pending interaction (an `ask` or a permission request), the permission store, optional role prompt, the rules and skills snapshots), `run_agent` and `resolve_permission` with one `AgentEvent` observer, the permission decision of each tool call (`authorize.rs`: the input of `permission::decide`, the session's permission flows, the decision table and the `PermissionRequest`; the rules, floor and shell splitting live in `src/permission/`), the wire types of a question (`permissions.rs`: `PermissionRequest`, `PermissionDecision`, `Scope`), request building, the context budget (`context.rs`: the estimate by category, `Budget::compute`, `ContextTracker`), compaction (`compaction.rs`: plan, checkpoint, effective history; `compaction/summary.rs`: the summary request and `prepare`), the local instructions (`instructions.rs`: `AGENTS.md`/`CLAUDE.md`/`rules.md` discovery, caps, diagnostics), the skills (`skills.rs`: `SKILL.md` discovery and precedence; `skills/`: frontmatter, the bounded catalog message, the `skill` tool), the agent definitions (`definitions.rs`: built-in `build`/`plan`/`vigie`/`lux` and the `.md` loader with its precedence and reserved names; `selection.rs`: the agent a session runs as, switchable only between turns) | events, persistence |
| `bus/` | ordering: the sequencer gives every event its `seq`; envelopes and the event catalog (`event.rs`); commands and inboxes; the runtime task that drives `run_agent` and publishes events; `runtime/bypass.rs`: the `Control::Bypass` handler (durable `session.bypass` commit, then the live state, then the eligible pending request) | HTTP, storage |
| `session/` | the `Service` (registry/router, `service.rs`; `set_bypass` sets a root and then its live children under the root's bypass gate, which child creation also takes, and a resume appends `session.bypass` off before its runtime starts), child creation and parent policy snapshot, result delivery callbacks and owner permission routing, tree teardown (`lifecycle.rs`), actor construction (`actor.rs`), the SQLite history (`store.rs`, including `child_links`, terminal CAS and completion outbox), the shared permission store, the session's permission flows and the parent's Captain evidence for a child, primary agent selection, per-agent model/variant overrides and editor context | HTTP, SSE, lower-layer assignment ownership |
| `server/` | HTTP routes and SSE with `Last-Event-ID` resume; `AppState` wraps the `Service`; `web.rs`: the embedded browser-client files, workspace listing/hide/unhide and optional session-tree deletion, `GET /v1/intent` | session state, the model layers |
| `workspace/` | what is known of a workspace directory regardless of any session: `git.rs` reads its branch and changed files (`git status -z`, bounded by a deadline, `None` outside a repository); used by the terminal client's header and panel and by `GET /v1/workspaces` | sessions, HTTP |
| `navigator/` | everything the Captain perceives as the navigator, below | session internals |

### Inside `navigator/`

| File | Responsibility |
| --- | --- |
| `lsp/` | the `LanguageServer` adapter and custom `withnav/*` methods (`mod.rs`), wire types (`protocol.rs`), URI to path (`uri.rs`). Thin: it delegates to `runtime`. |
| `runtime/` | the shared state behind the LSP: idle timers, the conversation-turn listener, trigger evaluation (`trigger.rs`), remark and wake delivery (`delivery.rs`, the wake text in `wake.rs`), reflex-facing calls (`inline.rs`) |
| `document.rs` | one `OpenDocument` per uri (text, baseline, version, diagnostics, edit history) and the `snapshot` a trigger reads |
| `reflex.rs` | inline suggestions: position conversion, prefix and suffix caps, debounce, staleness, and the reflex-context cache |
| `metacognition/` | the one `Metacognition` implementation: remark prompt and tool loop with its budget (`mod.rs`), answer parsing (`answer.rs`), the read/grep tools (`tools.rs`), the reflex-context producer and its prompt (`reflex_context.rs`) |
| `replay/` | offline replay of historical decision captures through built-in editor flows (`flows.rs`); no model calls or effects |
| `log.rs` | the JSONL decision log: record types, `PendingTrigger::finish` (the only writer of trigger records) |
| `conversation.rs` | `ConversationFeed`: the active conversation, its digest, the ask guard, wakes, Captain-turn events |
| `intent.rs` | `.withnav/intent.md`: read, section-wise atomic write, and the cognition's `intent` tool |
| `ports.rs` | the model seams (`FimModel`, `Metacognition`, `ReflexContextModel`) and `ModelError` |
| `models.rs` | adapters from the provider clients to those seams |
| `bootstrap.rs` | process wiring: builds each lower layer from its assignment, starts the in-process session server |

### Inside `tui/`

The terminal client of the session server: HTTP for commands, SSE for events, nothing else. It
imports the serialized bus types and no runtime internals. Several conversations can be open at
once; each is shown in the left sidebar and streams in the background.

Subagents are real, read-only child sessions nested under their parent. Parent events reconstruct
their agent, delegated goal, completion and permission state; session lists seed known links without
inventing goals. Lightweight child-stream watches read budget yields even before a transcript is
opened. `alt+s` opens a searchable list using the same popup style, also when the sidebar is hidden
below 80 columns. Opening a child only reads metadata and subscribes to replay plus live events:
it never resumes the child or sends it messages or commands. Its header names its parent, agent
and reported model; the message box is disabled. `alt+w` closes it and returns to its open parent.
Child stop and `alt+c` continue requests are confirmed in a popup; refusals keep the reported state
and show the server's error. Child permissions remain in the parent's conversation and use its
permission endpoint. A completion is one short notice; messages sourced from `user:subagent` are
labelled as untrusted subagent output. Optional delivery fields and `subagent.watchdog` payloads
are read without depending on a newer server event enum. Missing data is not guessed.

| File | Responsibility |
| --- | --- |
| `mod.rs` | `TuiOptions`, `TuiError`, `run` |
| `protocol.rs` | the wire bodies the client sends and receives, and the three message types of the loop: `Action` (what the user asked), `Effect` (I/O the reducer wants), `ClientEvent` (what came back, tagged with a `Generation`) |
| `state.rs` | `State`: the open `Tab`s (each with its conversation projection, session, model, variant, agent, activity, generation, draft, transcript position and unread mark) and what they share (catalog, session list, server commands, workspace); it derefs to the active tab. `UiState` (overlay, scroll, selection, what the variant list is for, sidebar and right column shown); data only |
| `client.rs` | the HTTP client: models, sessions, create/resume, send, stop, the command list and a command run, a permission answer (409 `permission_stale` read as stale), open the event stream; send failures split into rejected and uncertain |
| `config.rs` | Settings HTTP wire types and data-only editing state: six ordered sections, rows, summaries, filtering, staged changes, sequential revision-carrying writes, stale reload and validation errors. `State.config` owns it; `UiState.screen == Screen::Config` routes to Settings instead of the session transcript. `UiState.settings_target` gives the shared model picker its settings key, API filter and title. No server/settings internals or rendering. |
| `stream.rs` | one SSE subscription: decoding, reconnect with backoff, resume after the last seq, cancellation |
| `reducer/mod.rs` | `update`: `ClientEvent` to `State` changes and new `Effect`s; routes each event to the tab holding its generation, marks a background tab unread, drops events no tab holds |
| `reducer/envelope.rs` | `EventEnvelope` to blocks: steps, tools (planned, running, settled), usage, errors, command results and permission prompts; authoritative selection from `model.changed`, with `session.agent_changed` secondary; catalog refresh never overwrites selection. Pending `/model` is correlated to its operation id and ends only on its result/change, replacement, stop, end or tab close. Events and replay never send commands; a deferred target is named only from a matching pending operation or reported model identity. |
| `reducer/command.rs` | `outcome_text`: a command result as the short text the transcript shows, in the client's own words; `tokens` for `42k` / `1M` |
| `reducer/actions.rs` | `handle_action`: `Action` to `UiState`/`State` changes and `Effect`s; opening (at most nine), switching and closing tabs; server commands, agent/model/variant picks: live `/model <gateway/id>` changes the current agent's model (server applies target default variant), refused with a reason while an interaction waits; empty tabs choose an agent and optional overrides for creation |
| `reducer/text.rs` | `strip_controls`, applied to every piece of model and tool text |
| `input.rs` | the `Editor` and `route_key`/`route_paste`: keys to editor edits or `Action`s; the open permission request's `y`/`a`/`n` and deny message; the second enter before text that would cancel it |
| `commands.rs` | the command catalog, the key help and the key to `Action` map; `palette`, the client's and the server's commands in one list; `route`, the server's prefix rule (a slash in the first column, `//text` is the literal `/text`) with the client's own commands kept local and their arguments checked; `variant_choices`, the rows of the variant list |
| `runtime.rs` | `Runtime`: owns state, editor, client and streams; runs an event's effects for the tab it reached; the event loop with its frame tick |
| `runtime/effects.rs` | `Executor`: runs each `Effect` in a task and reports a tagged `ClientEvent`; owns one live stream per tab generation and stops those no tab holds |
| `runtime/tests.rs` | executor and loop tests with no terminal and a dead server |
| `terminal.rs` | raw mode, alternate screen, bracketed paste and mouse capture, restored on exit and on panic |
| `theme.rs` | the eight themes (Tide Dark by default) (`ThemeId`, the `Theme` colour roles), the RGB `blend` for tints, and the semantic styles the views draw with |
| `view/mod.rs` | `draw`: layout of header, sidebar, transcript, right panel, status line, editor, key bar and overlays; the key bar lists the keys that apply now, the prompt's while one waits |
| `view/header.rs` | the header row: `◆ withnav`, workspace, git branch and change count, `▌agent` in its colour, model · variant · context gauge, and an accepted model switch scheduled for after the turn; the path and model give way first |
| `view/panel.rs` | the right panel (36 columns, hidden under 140 unless `alt+r` asks): Agents and role, Context breakdown and last compaction, Skills listed and omitted, and Changes from git; a section with no data shows `—` |
| `view/sidebar.rs` | the left conversation sidebar: number, title, `<agent> · <state>`, unread mark, active tint and tab keys; hidden under 80 columns (`alt+b` also toggles it) |
| `view/transcript.rs` | block layout and drawing: messages by origin, steps, tools, errors, permission prompts, scroll and selection |
| `view/selection.rs`, `runtime/selection.rs` | the shared grapheme wrapping and transcript-only cell map; mouse selection, edge scrolling and bounded OSC 52 clipboard writes; whole-block copying keeps source Markdown and complete tool output |
| `view/permission.rs` | the inline permission prompt: tool and scope, a short preview of the call (a write's first lines, an edit's change, a command, a URL), the answer keys or the outcome |
| `view/tool.rs` | the one-line summary and the expanded output of a tool call |
| `view/markdown.rs` | markdown to styled lines, cached per block revision |
| `view/text.rs` | terminal-column measurement and grapheme-safe truncation and padding |
| `view/status.rs` | the status line of the active tab: activity, connection, delivery warning, notice, session tokens; `context_gauge` and `shorten`, shared with the header |
| `view/pickers.rs` | model, variant, agent, session, command, theme and help overlays; models group by gateway/provider with search across collapsed groups and server-reported windows/variants. Live enter switches in place, `ctrl+n` opens a fresh tab; waiting/refused controls explain why. A long list scrolls to its selected row. `ctrl+f` marks a favorite; favorites (`tui.favorite_models` in `config.json`) are listed first in an always-open section that is also kept in each model's group. |

### The browser client (`nav web`)

`nav web` (`src/app/web.rs`) starts the same private in-process server as `nav tui`
(`src/app/embedded.rs`: preflight on every `--workspace`, loopback only, a per-process bearer token,
graceful stop), prints `WithNav WebUI: http://127.0.0.1:<port>/#token=<token>`, opens it with
`xdg-open`/`open` unless `--no-open`, and runs until Ctrl-C. The token rides in the URL fragment, so the
browser never sends it to a server; the page keeps it and sends it as a bearer header.

The single-page app lives in `webui/`; its build output `webui/dist/` is committed and embedded in the
binary with `include_dir`, so `cargo build` needs no Node. The files are served by `server/web.rs` only
when `ServerConfig.web` is set (`nav web` alone; `serve`, the terminal client and the language server
leave it `None` and answer only the API): `GET /` and any non-`/v1/` path return the file in `dist/`,
else `index.html`. Those paths and `/v1/health` need no token; every other `/v1/*` route still does,
and an unknown `/v1/` path is a 404, never the page. HTML carries a strict Content-Security-Policy,
`nosniff` and `no-referrer`.

Two read endpoints serve the page, both behind the token: `GET /v1/workspaces` lists the launch
workspaces (kept in `WebConfig`, argument order, `launched:true`) and then the workspaces of the
history store (`HistoryStore::workspaces`: top-level conversations only, directories that no longer
exist skipped), each with its git branch and change count; `GET /v1/intent?workspace=` returns the
parsed `.withnav/intent.md`.

## Data flows

Scratch roots default to private `$XDG_RUNTIME_DIR/withnav` (fallback `~/.cache/withnav/scratch`) and
shared `<temp_dir>/withnav`, snapshotted in `permissions.scratch_dirs`. Every component is opened
descriptor-relatively without following links. A write into one carries the root's identity (inode,
mode), opens it again without creation and writes relative to that descriptor. Validation is read-only.

The fixed floor (`permission/floor.rs`) protects WithNav's `config.json`, `withnav.db` and global
flows directory: a write there needs the compiled-in protected check, asked once by the host and
passed to the flows, which never ask it again; anything else asks the Captain. Literal references in
bash are checked too, best effort; bash retains ambient access and cannot be fully confined. Scratch choice is inherited
from the parent's execution snapshot, and its no-follow descriptor/inode validation is repeated.

An allowed write or edit is bound to the canonical file the decision was made for, without
expanding writable roots. Sessions install private scratch storage from `permissions.scratch_dirs`
(ordered absolute paths, private runtime/cache first, shared temp second; `[]` disables); the owned,
non-symlink 0700 directory is a writable root and counts as part of the project, including in child
sessions. A scratch directory that contains, or sits inside, the configuration directory is refused.

**Key to screen (terminal client).** A key reaches `runtime.rs`, which calls `input::route_key`: it
edits the editor or returns an `Action`. `reducer::handle_action` changes the state and returns
`Effect`s. The `Executor` runs each effect: an HTTP call through `client.rs`, or a subscription
through `stream.rs`. Each result comes back on one channel as a `ClientEvent` stamped with the
generation it was started in; `reducer::update` drops events of an older generation, folds the rest
into `State` (envelopes become blocks) and may return more effects. After each batch the loop calls
`view::draw`, which only reads the state. Starting or resuming a conversation in a tab bumps its
generation, so late results of its previous session are ignored. Picking a model for a live
conversation sends `/model <gateway/id>` on that same session; on an empty tab it changes the starting
configuration. A conversation is created on the first message, not before.

**Captain-selected skills.** `/skills` reads `GET /v1/sessions/{id}/skills`: the session snapshot's
catalog-listed names and descriptions, or a disabled reason when its agent disallows `skill`.
The searchable picker fills `/skill <name>` without sending anything. Sending adds a typed
`{type:"skill",name}` content block beside the Captain's text; with no request the text is
`Apply the <name> skill.`. Session admission resolves one listed skill through the skill tool's
bounded loader before accepting anything. The inbox appends its exact output as JSON in a
`<driver_selected_skill encoding="json">` block in the Captain's own user message. JSON escapes
`<`, `>` and `&`, so loaded text cannot close the delimiter. Unknown, omitted,
disabled or unreadable selections refuse the whole message; clients cannot supply loaded content.
The turn's pinned agent policy is checked again after admission: an intervening agent switch can
refuse the whole command before answering an ask, cancelling permission, changing the Captain task,
appending text or persisting child delivery. A durable `command.result` uses its command id as
operation id, causation and correlation; other queued commands proceed.
Policy pinning does not consume a pending ask or clear its permission slot or answer claim:
only accepted input or a matching answer starts interaction consumption. Replay task lookup
excludes correlated whole-message refusals, so resume and new children keep the real Captain task.
`message.received.selected_skill` records `{name,origin:"captain"}` for live and replayed clients.
No synthetic tool exchange or model-visible provenance argument is used. Selected instructions
stay with their Captain message through storage and latest-batch compaction protection. Multiple
queued Captain selections all stay verbatim even with interleaved navigator input. Discovery covers
the consecutive user-message run, but only Captain-provenance messages are protected, carried and
charged as irreducible. Non-Captain input retains its provenance in the unsummarized tail and may
be summarized before a checkpoint. If Captain selections cannot fit, the capacity error prevents the request; no
skill is silently truncated. Event text remains Captain text only for display.
This does not change the event catalog or execute a skill's scripts.

**Permission prompts (terminal client).** `permission.requested` adds a `BlockKind::Permission` to the
tab and its id to `Conversation::open_permissions`; the matching `permission.resolved` closes it and
settles the held tool (denied, or interrupted on a cancel or an expiry). Because both are durable
events, a replay or a reconnect rebuilds the same open requests. While one is open on the tab on
screen and the message is empty, `y`/`a`/`n` become `Action::AnswerPermission`; the reducer posts it once
(`Effect::AnswerPermission`) and the prompt stays until the stream says how it ended; a stale answer
drops it with a notice. Text sent meanwhile cancels the request on the server, so the first enter
only warns.

**Commands and agent controls (terminal client).** Typing `/` on an empty message puts the slash in
the editor and opens the palette, which follows the line: it lists the client's commands and the
server's (`GET /v1/commands`), filtered by the name typed so far, and puts itself away as soon as the
line holds more than a name (a space, an argument, `//`, a name nothing starts with). Enter on a line
starting with `/` goes through `commands::route`: the client's own commands (`/sessions`, `/theme`, `/new`, `/quit`…) act locally,
`//text` is sent as the message `/text`, and every other `/name` becomes `Effect::RunCommand`, a
`POST /v1/sessions/{id}/commands`. The editor is cleared once the server took it; the outcome comes back
on the stream as `command.result` and becomes a `BlockKind::Command`. The build/plan switch (tab on an
empty message) and `/variant` are `/agent` and `/variant` runs; the `/variant` list opens on the answer
to a bare `/variant`, so it shows the conversation's own offer. The tab's agent and variant change only
when the server says so (`model.changed`, command results, the session summary), so a
refusal leaves them as they were; each keeps the seq that last set it, and a summary read at an older
seq does not overwrite it. Before a conversation exists, the agent and the variant picked with
the model are sent in the create request instead.

**Tabs (terminal client).** Every tab has its own generation, never shared, and the executor keeps one
SSE stream per generation. An event is applied to the tab that holds its generation, whether it is on
screen or not (a background tab is then marked unread), and the effects it returns run with that tab's
session. New-conversation actions such as `/new`, `ctrl+n`, and `ctrl+n` in the model picker allocate
or reset a generation; `/sessions` opens or resumes a selected session in a tab. Picking a model for
a live conversation keeps its tab, generation and session and sends `/model <gateway/id>`; on an empty tab,
the model pick only sets its starting model and variant. Closing a tab drops its generation, so the runtime
cancels its stream and its late results are ignored; nothing is sent to the server. Before each action the runtime copies the editor into the active tab's draft; the
reducer puts the next tab's draft back with `Effect::SetEditor`. The context gauge is per tab too: each
conversation keeps its snapshot, its epoch and its retired incarnations, a `context.updated` or a
`FetchContext` answer lands on the tab whose generation it carries (without marking it unread), and the
status line shows the active tab's.

**Permission request to answer.** Before each tool call of an assistant message, `run_loop`
(`run_decided`) checks tool availability for the session and role and the call's arguments
(`preflight`), then makes ONE decision through `agent/authorize.rs`, which calls
`permission::decide` (ADR 0015). The order is fixed: yolo, the fixed floor (`permission/floor.rs`:
credentials ask, WithNav's `config.json`, `withnav.db` and global flows directory need the protected
check, an unresolvable path asks), free reads inside the readable roots, the rules
(`permission/rules.rs`, the most precise match wins; a `deny` stops the call), the doom-loop check
(`DoomTracker`, reset at each `run_agent`), bypass, the permission flows, then the Captain. No rule
means ask; there are no default rules, and a tool the decision does not know (an MCP tool, a role-less
actor's call) is judged like any other: no rule, ask. `bash` lines are split into commands
(`permission/shell.rs`) and each command's `rule_text` is matched; an opaque command matches no
`allow` or `ask` rule and offers no "Always". WithNav's own conversation controls (`task`,
`steer_subagent`, `decide_subagent`) act on the session and its children, whose own calls are decided,
and `intent` only rewrites the sections of `.withnav/intent.md` it owns (no path argument): they are
not decided.

The flows are reached through `PermissionFlows`, implemented by `authorize::SessionFlows` over the
session's `ToolCallFlows`: `flow` is `FlowRuntime::permission_pre_call` with the payload
`rule_effect`, `rule`, `commands {raw, normalized, prefix}`, `inside_project`, `protected`, the
active validated skill and `conversation.captain_messages` (`captain_messages_with_cut`); flows that
gave no verdict of their own apply the rule. `protected_check` is
`FlowRuntime::permission_protected_check`, the compiled-in node alone. Without a flow runtime the rule
applies and the protected check is `Unavailable`. Children get the same flows without their effects.
The built-in `permission-default` flow is enabled. The Captain's evidence is the session's own
Captain messages, the Captain's answers to `ask` (tool results marked `answered_by: captain`,
labelled with their question, never sent to the model) and, for a child, a snapshot of the parent's
Captain messages taken at creation and refreshed when the parent steers it; a delegated prompt or
steering text never counts as Captain evidence. It is supplied separately as
`conversation.delegation` (Parent-provenance transcript messages, stripped of attachments, newest
whole messages within 16 KiB, with a cut flag). The ordinary flow may accept the parent's request;
the protected check never reads delegation. Both the flow and the protected check also get
`conversation.recent` (`recent_with_cut`): the last 4 Captain and agent messages up to the Captain's last one, labelled
with their author, so a short Captain answer is read with the proposal it answers. A skill is decided after its validation, with action `skill` and its
name as resource: a `deny` rule refuses it on both entry points, no rule loads it
(`approved_by: validation`), and a Captain selection counts as the Captain's request. A session
whose permission database cannot be opened starts with `permissions_unavailable` in its status,
shown beside yolo by every client. `Input::store` is explicitly absent: every call, reads included,
gets no rule, the floor applies, and the flow may allow. Bypass is disabled, proposals are empty,
and no decision is recorded (one warning at session creation). Yolo is unchanged. Stored decisions
are redacted (the flows' secret detection) before any field is bounded.

A call that runs carries `approved_by` (`rule`, `flow`, `captain`, `bypass`, `yolo`, or
`validation` for a validated skill no rule covers) and the
`permission_gate` evidence (normally `policy_version: 3`, rule id, flow verdict, floor; yolo
tool-event evidence still carries `policy_version: 2`) on `tool.pre_call` and
`tool.post_call`; a `deny` rule or a flow `deny` settles it as `permission_denied` with `denied_by`.
Every decision is a row of `permission_decisions` in `withnav.db` (`pending`, then `allowed`,
`denied`, `cancelled` on a stop or new message, `expired` on resume), filed under the session and the
turn. A question becomes a `PermissionRequest` with its reason, `requires_captain` (set unless a rule
`ask` or the flow raised it), the "Always" proposals and `always_label` (for example
`Always (project): bash git push *`; project scope for `bash`, file tools, `fetch` and
`external_directory`, global for the rest).

A call that needs a question is not run: it, and the calls behind it, become
`PendingInteraction::Permission` and the turn yields (`RunOutcome::PermissionRequired`). The runtime
publishes `permission.requested` and the turn ends `yielded`. `POST
/v1/sessions/{id}/permissions/{request_id}` reaches `Service::answer_permission`, which checks the
request id against the runtime's status (a child's request is answered through its parent), then sends
a typed `PermissionAnswer` on a channel of its own (never a `Command`). The runtime publishes
`permission.resolved` and calls `resolve_permission`: Once runs the held call; Always first saves
every proposal in one transaction, then runs it, and is refused when nothing can be saved; Reject
settles the held call and every call behind it with the Captain's message. After an allow, each
sibling goes through the whole decision again, floor included, so a saved rule releases it. Legacy
answers still parse: "allow folder" and "allow configuration for the session" count as Once, `deny`
as Reject. An allowed file tool is bound to the canonical pathname the decision was made for and opens it
without following links; when that pathname is no longer a plain file there (a link put in its
place), nothing is written and the call goes through the decision again. The binding is the
pathname, not the inode: a regular file swapped in at the same path is written. A stop or a chat message cancels the request; a resume expires every request that has no
`permission.resolved`, settles its calls as interrupted, and marks its decisions `expired`.

**Bypass.** `BypassState {enabled, seq}` is the actor's ephemeral setting, shared with `RuntimeStatus`;
only `Control::Bypass` changes it, after the `session.bypass` event is durable (`commit_acked`), in the
idle and the running select alike. The decision reads it as its control level: a call that no `deny`
rule, floor or loop stops runs with `approved_by: bypass`, without asking a flow. Turning it on, and a
request published just after, claim the waiting request when `pending_bypass_eligible` says it was
raised by a rule `ask` or a flow (never the floor, a loop or a skill validation) and queue an internal
`AllowOnce` of origin `Bypass`, which decides the call again under bypass and settles the pending
decision row; a Captain answer that already holds the claim wins. `Service::set_bypass` applies the root,
then each active child by `parent:<id>`, under `RuntimeStatus::bypass_gate`; `admit_child` holds the
same gate from registration to the child's first prompt and commits the inherited ON before it.
`nav run --yes` is `set_bypass(user:cli)` before the first prompt; any remaining
`permission.requested` ends the run with exit 3.

**Yolo.** `"yolo": true` in the user settings is the lowest of three control levels: yolo (nothing
checked) < bypass < normal. `Settings::yolo` is read at session start into
`EffectiveExecution.yolo` (children inherit their parent's value; a model switch keeps it) and from there into `AgentActor::yolo` and `SessionSummary.yolo`. `run_decided`
checks it right after `preflight`, so no rule, floor or `tool.pre_call` flow is consulted: `run_yolo`
dispatches the call with `approved_by: yolo` (recorded as such in `permission_decisions`), and no
Instinct request is made. Skill validation is skipped too. The role's write scope
(`validate_write_scope`, Cartographe) is authorization and is not checked either. At dispatch the
host sets `DispatchBinding::yolo` (never from arguments) and a `ToolContext` whose writable root is
`/`: file tools are not confined to the session's roots, no scratch identity (inode, mode) is
attached, and `grep`/`glob` do not refuse a sensitive search root, while still filtering sensitive
entries below it and bounding output. What stays is `preflight` (reserved and malformed arguments,
tool availability for the session and role) and cancellation; a call `preflight` refuses still
carries the `approved_by: yolo` evidence, with an error status.

**Agent limits.** An agent's limits are rules with `agent` scope that the Captain adds (ADR 0015);
WithNav seeds none. Tool availability per agent (`tool_allowed_for_role`) and the
Cartographe write scope (`validate_write_scope`) still apply before the decision.

**Keystroke to suggestion (reflex).** The extension asks `withnav/inlineCompletion` after 150 ms
of pause. `runtime/inline.rs` reads the document and the reflex context, `reflex.rs` builds the FIM
request, `provider/fim.rs` calls Mercury, the answer goes back as ghost text. A newer request
supersedes an older one; a stale version is dropped.

**Pause or save to remark or wake (instinct, metacognition).** An idle timer or `didSave` calls
`runtime::evaluate`. Enabled editor flows exclusively handle the document snapshot, prefilters,
instinct questions and ordered decisions. Built-ins preserve the original state and prompts.
Remarks use anchored metacognition; wakes use typed flow notes to the conversation. No enabled
flow means nothing runs. The decision log retains delivery records; flow records own run traces.

**Chat to cognition.** The extension posts Captain messages to the session server. The bus
sequences them, the runtime task runs `run_agent` with the chosen agent's effective chat model, events stream back over
SSE. A navigator session also feeds the `ConversationFeed`, attached by `NavigatorHooks::started` (`navigator/session_hooks.rs`; a resume replays without announcing old turns): its digest reaches the fast layers'
prompts, and every Captain turn end triggers a `conversation` evaluation on the focused document.
Editor roots have the same root subagent tools as terminal roots. Children have no editor attachment,
do not receive `intent`, and are never registered by `started`. Completion uses the normal durable
outbox and root stream, not the latest-wins navigator wake channel. The digest labels delivered
subagent text as untrusted and never treats it as a Captain turn.

## State on disk

MCP declarations live only in the global `config.json` under `mcp`. Connections, restart
counters, tool inventories and bounded stderr tails are memory-only. Environment references
are resolved only for child launch, never saved to configuration, history or events; declared
environment values and their interpolated components are redacted before output truncation
or stderr eviction (including chunk boundaries). Application subscribers exclude rmcp events
even under verbose user filters because dependency diagnostics can include raw wire payloads.
Wire-name collisions are resolved across the ready inventory in server-id/original-name order,
keeping the first and reporting skipped definitions. No workspace MCP
configuration is loaded. Config changes require a service restart; new sessions snapshot only
currently ready tools, and existing registries stay frozen, including across connection restarts.

WithNav writes the new paths below. During 0.7, absent `.withnav/` directories
fall back to `.navai/` then `.navigator/`; old config/data paths and `navai.db` /
`navai.jsonl` migrate without overwriting new files. `NAVAI_*` remains a warned
fallback for `WITHNAV_*`. The LSP accepts legacy `navigator/*` requests but emits
`withnav/*`; wire readers accept `driver` and writers emit `captain`.
See [ADR 0014](docs/adr/0014-rebrand-withnav.md). Internal names such as
`requires_driver` and `driver_selected_skill` remain technical identifiers, not
user-facing roles.

**Background children.** Admission reserves capacity and snapshots the parent's complete effective
execution configuration and policy. Publication and failed-start cleanup have parent- and
child-scoped ownership records; lifecycle callers wait outside admission, so child deletion cannot
race registration or joining. Child work never takes the lifecycle gate. Parent stop marks and
cancels every live member before joining; parent deletion removes stored descendants too.

Terminal `child_links` CAS records a self-contained `completion_outbox` notice and only its winner
releases capacity. A parent's sequencer commits `subagent.done` and the notice's `delivered` marker
together. `bus/runtime/delivery.rs` invokes the service callback at the next turn boundary, after
earlier transcript snapshots have drained. The callback appends bounded untrusted JSON user
messages in notice order. Each message is sequenced as durable `message.received` and its commit
callback atomically saves the transcript, notice's `consumed` marker and event before emission or
the model call. Its payload keeps `{command_id, source, target, mode, content}` with
`source: "subagent"`, plus `child_session_id`, `agent`, `outcome`; watchdog deliveries add
`watchdog: true` and doubt notices include `verdict`. Content is the same bounded untrusted JSON
text stored in the transcript. A crash before emission therefore replays the committed event
without consuming or delivering the message twice. No new event kind is added (22 total).
It never injects through navigator commands or changes the Captain task. The runtime's existing
control channel carries delivery wakes: idle parents run immediately, busy parents wait until the
boundary, and pending interactions cannot be answered by a result. Stopped parents have no wake;
resume drains durable notices. Child deletion keeps unconsumed notices and their result text.

Child permission events are mirrored by `PersistingEmitter` onto the parent stream with optional
`child_session_id`; the child's typed interaction remains authoritative. Owner-scoped
answers claim and use the child's existing permission channel. A foreign parent cannot answer,
and child `ask` stays unavailable. Startup terminalizes persisted starting/running children through
the same CAS with reason `restart`, queues their notices, then seeds capacity; no tool is replayed.

**Steering and turn interrupts.** `agent/boundary.rs` exposes the single injection seam, called by
`run_steps` before a provider request once the whole previous tool batch has settled. Runtime
steering (`bus/runtime/steering.rs`) applies queued parent instructions with `Parent` provenance,
using identified operations that reserve interrupt input for its replacement turn. The guidance
queue is in memory and can be lost on a crash; results remain durable in the completion outbox.
then the durable delivery callback consumes eligible mid-turn results. Both precede the request's
budget check. A renewable actor turn token is separate from `session_cancel`: an input interrupt
unwinds the current provider/tool work, pairs interrupted calls with results, records the old
turn's interruption, then renews the token without terminalizing the child. Session stop still
cancels the lifetime token and tears down the tree.

`tools/steer_subagent.rs` is installed only alongside root `task`, never in child or metacognition
registries. `Service::steer_child` validates ownership and active child-link state; it queues input
through the runtime's existing control channel, without taking lifecycle. Explicit child interrupts
settle pending permissions through the existing cancellation and durable mirror path. Parked steer
does not answer any interaction. `child_links.delivery` stores `next_turn`, `steer` or `interrupt`;
the self-contained completion outbox retains it after child deletion. Parent result interrupts
fall back to next-turn delivery for pending human interactions; inbox commands and queued controls
are checked before completion-only wakes. `subagent.spawned` and `subagent.done` payloads include
`delivery`; no new event kind is introduced.

**Root watchdog.** `session/subagents/root_watchdog.rs` reuses the child clock, evidence, judge and
consecutive-failure rule. The service installs one task per root runtime. It skips idle and human
waits, checks the turn generation again after each model await, and submits a safe-boundary stop or
doubt hold through `WatchdogPark`. A local mockable `ProgressReviewer` performs one tool-free
metacognition call within 20 seconds; undecidable reviews ask the Captain. Notices reuse
`command.result`; the runtime emits typed `agent.yield` at the final safe-boundary check after
awaited delivery, snapshots settled tools and ends the paused turn. Hold identity is a session
incarnation plus unique hold id. Continue consumes the matching token atomically and sequences
resolution; restart/stop expires it. A Captain message supersedes it and records interruption;
autonomous inputs cannot release it or renew a parked budget. The event catalog remains 22 kinds.
The typed boundary result carries the settled transcript on every watchdog exit and keeps the
remaining steps. Interrupt-delivery checks revalidate the hold after awaiting notice eligibility.
Recovery installs an awaiting-fresh-Captain gate; neither delivery wakes nor delegated provenance
can clear it. Deferred controls accepted in a paused turn receive operation-correlated cancellation.
Captain admission events never clear the recovered gate. The runtime commits the consumed Captain
transcript and a `command.result` checkpoint marker atomically through the existing acknowledged
transcript/event transaction before releasing the gate. Recovery reads that marker, not admission;
an admitted but unconsumed correction therefore requires fresh input after another crash. Accepted
interrupt guidance on a budget-parked child stays queued and is claimed once by authorized Continue
before its first resumed provider request; the guidance itself never renews the budget.
Progress parking filters autonomous-only batches before policy pinning. A Captain batch pins its
role before validating selected skills; held-only or refused batches release that pin without
changing the interaction claim, progress generation, goal or recovery gate. Only applied commands
supersede a watchdog hold and commit fresh input. `consume_turn` starts activity and the watchdog
after that validation, using the updated Captain task, before transcript-persisting turn delivery.
Root `alt+c`
uses the existing Continue route with the root as both owner and target, which cannot answer asks
or permissions. Root turns are unbounded unless `WITHNAV_MAX_STEPS` is set; unavailable instinct
or disabled supervision installs a visible renewable 500-step fallback cap. Children keep their
50-step budget. The root supervisor starts only after publication; runtime exit cancels and joins
it outside admission and registry locks.

**Child watchdog.** `session/subagents/watchdog.rs` owns the monotonic clock seam and progress evidence/question/verdict conversion, using
`provider/systemone.rs::DecisionModel` for the decision model,
bounded evidence, separate `subagent_progress` System One choice and consecutive-failure rule.
`supervisor.rs` owns one service task per published child, cancelled by lifetime stop or terminal
completion. It sleeps a full configured interval between checks, never replays missed ticks,
skips parked/idle children and awaits Instinct outside lifecycle/admission. `bus/runtime/progress.rs`
records only observable text, call/result and step activity; thinking is never evidence.

`outcomes.rs` puts every verdict except the first failed check in the existing durable outbox.
The parent's sequencer emits `subagent.watchdog`; only doubt notices need transcript consumption
and a parent wake. Doubt is an ephemeral `RuntimeStatus.watchdog` hold at the existing safe boundary:
no further provider request until resumed; the complete tool batch still settles. The boundary
wait respects the renewable turn token as well as session stop. Stuck and model stop claim the
terminal reason, cancel and join under child-scoped operation ownership, then commit the terminal
child CAS and bounded partial text before releasing capacity, without taking lifecycle.
`tools/decide_subagent.rs` is installed beside root `task` only;
`decision.rs` enforces owner and watchdog-doubt state, never answers permissions or budgets.
The existing owner continue endpoint also resumes doubt. Recovery terminalizes unfinished links
as before; no supervision state is restored. The event catalog has 24 kinds.

| Path | What | Written by |
| --- | --- | --- |
| `~/.config/withnav/models.json`, `auth.json`, `config.json` | model catalogue, provider keys, assignments and client settings | the Captain/importer (catalogue), `nav auth` (keys), TUI (client settings, not assignments) |
| `~/.config/withnav/navigator/pipeline.toml`, `prompts/*.md` | historical custom pipeline, converted once to global flows | the Captain |
| `~/.config/withnav/agents/*.md` | user agent definitions | the Captain |
| `<workspace>/.withnav/pipeline.toml`, `prompts/*.md` | historical custom pipeline, converted once to workspace flows | the Captain |
| `<workspace>/.withnav/intent.md` | intent, plan, decisions | the cognition and the Captain |
| `<workspace>/.withnav/agents/*.md` | project agent definitions | the Captain |
| `~/.agents/AGENTS.md`, `~/.agents/rules.md`, `~/.agents/skills/` | user rules and skills included in agent sessions | the Captain |
| `~/.local/share/withnav/withnav.db`, `withnav.jsonl`, `tui.log`, `model-catalog/` | conversations, decision log, TUI log and cached provider model lists | `session/store.rs`, `navigator/log.rs`, `tui/`, `provider/catalog.rs` |
| `withnav.db` tables `permission_rules`, `permission_decisions`, `validations` | permission rules (revoked, never deleted), the history of every permission decision (arguments redacted and bounded), skill and flow validations bound to a content hash | `permission/` (the agent loop, `nav permissions`, the Studio and flow store) |
| `~/.config/withnav/flows/`, `<workspace>/.withnav/flows/` | global and workspace flow definitions; they load only once validated at their current content | the Captain, the Studio |
| `~/.config/withnav/config.json` key `mcp` | stdio MCP server declarations | the Captain |

Data and config paths come from `paths.rs` and `provider/auth.rs`. On every start `paths::migrate_legacy`
(`paths/migrate.rs`) moves what the former names left behind to the WithNav dirs: `~/.local/share/navigator/*`
and `~/.local/share/rust-agent/*` (`navigator.db` or `navigator/sessions.db`, with their `-wal` and `-shm`,
`navigator.jsonl`, `tui.log`) to `~/.local/share/withnav/`, and `~/.config/rust-agent/*` (`auth.json`,
`models.json`, `navigator/pipeline.toml`) to `~/.config/withnav/`. A file is never overwritten, a second
start does nothing, and a database moves only when no other process holds it: otherwise it stays where
it is and that run keeps using it.

The permission redesign (ADR 0015) replaced `grants.json`, `skill-grants.json` and `flows-trust.json`
with the tables above. Their content is not imported, and nothing reads or writes those files any more.

That existing path relocation is not automatic 0.5 configuration conversion. Catalogue and
assignment migration is manual. History without provable saved model identity remains readable
but read-only, with resume disabled; it is never silently rebound. Existing session ids are preserved.

## Selection and wire ownership

Catalogue names are
`<gateway>/<upstream_id>`; `ModelName` splits only the first slash and retains the rest verbatim.
`models.json` contains capabilities/API/limits/variants, never assignments. `config.json` owns
`layers`, `agents` and `default_agent` (`build` when absent), with assignments taking precedence
over Markdown `model:` headers. See [the examples](models.example.json) and
[assignment example](config.example.json).

Reflex = one FIM model (editor only), instinct = one System One model, metacognition = one chat
model (prefers `low`, otherwise model default). Cognition runs agents, not a singleton model.
Missing/unknown/API-incompatible lower layers are disabled with reasons and existing conservative
fallbacks: no suggestions, the permission flow applying the rules (no rule asks the Captain),
renewable 500-step supervision cap when instinct is unavailable. Invalid nondefault agent models warn and fall back to the default agent.
An unknown/nonprimary default agent or one without a usable chat model prevents server/LSP startup.

Root selection resolves the `(session, agent)` override, then the agent assignment, then the
default agent's model. Children with an assignment use it; unassigned children inherit the
parent's effective model and variant. `/model` without a variant takes the target model default.
`/variant` distinguishes named, `off` and `inherit`: the latter removes the local variant choice.
Switching agents or resuming restores that agent's durable override, never mutates global config,
and never applies a deferred selection to a different agent. `/new [agent]` starts without old
session overrides. Editor context adds prompt/intent/feed independently of the chosen agent.

| Route | Response/request contract |
| --- | --- |
| `GET /v1/models?workspace=...` | `models` entries: `name`, `gateway`, `provider_group`, `upstream_id`, API, capabilities, variants/default and existing limit/diagnostic fields; top-level `default_agent` |
| `GET /v1/agents?workspace=...&session_id=...` | `default_agent`, all definitions with `id`, description, mode, `assignment`, `effective`, optional `override`, warning; without session id omit override; an unassigned child without parent context has null choices and `inherits_parent: true` |
| `GET /v1/layers?workspace=...` | exactly reflex/instinct/metacognition entries: `layer`, `status` (`usable` or `disabled`), `model`, `variant`, `gateway`, `upstream_id`, `api`, `reason` and optional `variant_diagnostic`; usable validates assignment/API, not future network access |
| `POST /v1/sessions` | `workspace`, optional `editor` (default false), `agent` (default agent), `model`/`variant` overrides; variant-only allowed; response `session_id`; resume uses workspace + resume only and rejects conflicting selection fields |

Catalogue `name` and active `model` are canonical identities; internal provider `model` remains
the upstream id. `provider_group` is the first upstream-id segment, or gateway when unslashed.
Summaries expose no endpoint URL, headers or credentials. Assignment/effective selections include
`source` (`config`, `header`, `default_agent`, `parent`, `override`) and `variant_source` (those
plus `model_default`, `layer_preference`). Override JSON permits optional model: omitted variant
inherits, null is off, a string names a variant. Session summaries/lists retain tree/state fields
and add editor/agent/model/variant/source/variant_source/gateway/upstream_id/current_seq; only
summaries may include the overrides map. Archives expose `read_only` and `resume_disabled_reason`.

`model.changed.previous` and `.new` carry agent/model/gateway/upstream_id/variant/source/
variant_source; reason is `model`, `variant` or `agent`. Bounds/revision/context stay outside those
identities and operation correlation is preserved. `session.agent_changed` is secondary, not a
separate authoritative selection. Context snapshots and `last_usage` expose canonical model,
gateway and upstream_id (usage retains its own identity and step string). Bare `/model` reports
agent/model/variant/source/models; bare `/variant` reports agent/model/variant/offered/variant_source.
`/model TARGET` returns changed/applies/selection or deferred accepted/applies after_turn/agent/model;
`/agent ID` returns changed/previous/agent/selection. All retain operation_id/name/ok/data/error.

Selection commits save agent, model resolution, variant, reasoning floor and per-agent
overrides together with the ordered durable selection events in one acknowledged transaction.
`EffectiveAgentSelection` carries the applied identity; optional `SelectionCheckpoint` in
`session/store.rs` stages transcript and compaction checkpoint for installation only if that
selection transaction succeeds. Publish/install only after commit; a failed write leaves the
previous coherent selection intact. The runtime safe-boundary queue is reused, not a second
control path. Destination prompt, tools and skills are measured without installing the destination
role on the live actor. Compaction for a selection change is staged, not independently saved;
the checkpoint and selection succeed or fail together.

Clients consume these server choices, group models by gateway then upstream `provider_group`, and show layer and agent
sources rather than deriving assignments themselves. Configuration editors and flows belong to
the later 0.6 scope, not this contract.

## Where new code goes

- MCP configuration and environment isolation belong in `mcp/config.rs`; transport supervision
  and bounded paginated discovery in `mcp/connection.rs`; naming, schema hygiene and result
  conversion in `mcp/tool.rs`. The session service owns one manager and `session/actor.rs`
  installs its ready snapshot before role/tool filtering. Shutdown joins supervision tasks and
  kills child process groups. MCP adds no event kind in the stdio delivery.

- Workspace visibility belongs in `session/store.rs` (`hidden_workspaces`, canonical paths, cleared on session creation); its bearer-protected HTTP actions and canonical workspace matching live in `server/web.rs`. Workspace deletion uses `Service::delete`, never direct SQL session deletion.

- Settings HTTP edits live in `server/settings.rs`: shared `settings::update` locks a stable sidecar
  across snapshot read, revision comparison, mutation and atomic save. `server/settings/editor.rs` owns
  editable keys, defaults and scalar validation; `provider/assignments.rs` checks proposed model
  API and offered variants without runtime fallback. Unknown config keys and siblings survive.
  `restart` means after WithNav restarts (conservative for layers). `new_sessions` means next use:
  new sessions, model/agent switches or newly started subagents; running turns keep their values.
  `live` means the changing client applies it at once; other open clients on their next start.

- Canonical identity parsing belongs in `provider/model_name.rs` (`ModelName`), assignment and
  default-agent validation in `provider/assignments.rs`, and shared lower-layer construction in
  `provider/layers.rs`. Do not put these policies in clients or duplicate them in navigator bootstrap.
  Typed assignment access belongs in `settings.rs`; unknown unrelated config keys must survive saves.
- Durable per-agent overrides and selection staging belong in `session/store.rs`
  (`EffectiveAgentSelection`, optional `SelectionCheckpoint`), with service/runtime callbacks
  applying the transaction contract above.
  Wire source fields belong with selection summaries/events, never in provider credentials.
- A chat model's context window (models.json, else the provider's model list, cached in
  `<data dir>/model-catalog/`): `provider/catalog.rs` (`resolve_chat`); the session service calls it
  before building an actor and refuses a model with no window. The result is saved once in
  `sessions.model_resolution` (`session/store.rs`, versioned JSON, no credentials): a resume reads it
  back instead of resolving again, so a live or resumed conversation keeps its bounds; changed
  gateway/upstream/API identity is `ServerError::ModelIdentityChanged`, never a silent move.
  The metacognition resolves through the same function in `navigator/bootstrap.rs`.
  The fetch is single-flight per endpoint, bounded (3 s, 2 MB, 10 000 entries), never follows a
  cross-origin redirect (`ClientCache::get_same_origin`) and keeps stale values on failure; `GET /v1/models`
  (`server/routes.rs`) reports the window source, staleness and discrepancy of each model.
- The context budget (what the next request weighs, and how much of the window it may fill):
  `agent/context.rs`. `estimate` is the one place that sizes a request, by category; `Budget::compute`
  is the one place that applies the window, output reserve, input limit and margin; `ContextTracker`
  owns the snapshot and its revision. `AgentActor::context_breakdown` feeds it, `run_agent` reports
  `AgentEvent::ContextEstimated` after each response and each step's tool results, and the runtime
  (`bus/runtime.rs`) records the provider's usage per step, updates the tracker held in
  `RuntimeStatus` and publishes the live-only `context.updated`. Readers go through
  `Service::context` (`GET /v1/sessions/{id}/context`, `/context`), never the actor. Compaction asks
  `ContextLimits::from_resolution` and `Budget::compute` for the budget and compares the estimate
  against `input_budget`; a client shows the server's estimate and never re-derives it.
- Compaction (a summary standing for the start of a long conversation): `agent/compaction.rs` plans
  it (pure: the protected tail, which old tool outputs become markers, the boundary `through_message`,
  sized with `context::estimate`), `agent/compaction/summary.rs` builds and validates the summary
  request and `prepare` returns a `Checkpoint` against a requested destination budget without saving
  anything. The runtime (`bus/runtime/compact.rs`) is the only caller: `/compact` reaches it through
  `RuntimeStatus::request_compact`, between turns or deferred to the end of a running one, and
  `AgentActor::commit_compaction` persists through the saver the session service supplied
  (`HistoryStore::save_checkpoint`: transcript and the nullable versioned `sessions.checkpoint`
  column in one transaction), then installs. `AgentActor::effective_history` and
  `context_breakdown` are what `run_agent` sends and what the gauge measures. A resume validates the
  stored checkpoint against the transcript and fails clearly when it does not fit. The durable
  `context.compacted` notice (with `reason`: `manual`, `automatic` or `switch`; absent in an old
  event) is explanatory; the stored checkpoint is the recovery source.
- Automatic compaction: before every provider request `run_agent` compares the estimate of the next
  request (`context_breakdown`) with `AgentActor::input_budget`, which the runtime sets at each turn
  start, and at that safe boundary (every tool result of the last step is in, nothing pending)
  returns `RunOutcome::NeedsCompaction { steps_left }` instead of sending. The runtime
  (`compact_when_full` in `bus/runtime.rs`, `compact::run_automatic`) summarizes with the session's
  own model and variant and installs through the same commit path as `/compact`, then `continue_run`
  carries the SAME turn on with `steps_left`: no `turn.end`, no step reset. The plan tries `Aim::Fit`
  below ~0.7 B, then B, and only then `Aim::FitMidTurn`, which gives up the recent-turns protection
  and keeps just the latest exchange so a single long turn can summarize its own earlier steps. A
  plan that cannot fit costs no provider call; the turn then ends with a `compaction` error that
  says `/compact` cannot help and to use a larger window. A selection change uses
  `bus/runtime/model.rs::compact_for_target`: the old model summarizes for the destination's
  budget, role prompt, tools, skills and reasoning projection. The resulting checkpoint stays
  staged until the selection transaction commits. Failure or cancellation before commit leaves
  both the old selection and old checkpoint intact; resume restores the committed pair.
- Model switching (`/model <gateway/id>` inside a conversation): `bus/runtime/model.rs`, run like
  `/compact` through the same control channel (`RuntimeStatus::request_model_switch`), between turns
  or after the running turn, from one ordered queue of deferred controls (`bus/runtime/deferred.rs`;
  adjacent compactions are one). The session service supplies the `ModelSwitch` callbacks
  (`session/service/resolve.rs`: the target resolved like a new session, its default variant;
  `HistoryStore::commit_agent_selection_cancellable`: the selection, overrides, optional
  `SelectionCheckpoint` and ordered events), so the bus never imports the service or the store.
  `Sequencer::commit_batch_acked` sequences the batch, waits for the store's acknowledgement,
  then emits it; a failed write emits nothing and returns the reserved sequence numbers.
  `SelectedModel` (model identity, resolution, revision, variant, offer) is one value behind one
  lock. The commit callback installs the coherent selection, runtime variant and context tracker
  before emission. Selection switching refuses `/agent`, `/variant`, `/compact` and another
  switch while one prepares. The event batch includes any `context.compacted` notice, then
  `model.changed`, then the secondary `session.agent_changed` on an agent change.
  Only after that acknowledgement does the
  runtime swap the actor's provider, model, variant and limits. `AgentActor::reasoning_floor`
  (restored on resume) keeps the earlier reasoning out of the request projection unless the resolved
  identity is the same; the fit check measures that destination projection (`context_breakdown_at`).
- A helper you need probably exists. Look first in: `provider/client.rs` (any HTTP call to a model),
  `provider/config.rs` (`load_models`), `navigator/log.rs` (`excerpt`, `new_ulid_string`),
  `bus/event.rs` (`now_rfc3339`), `navigator/lsp/uri.rs`, `navigator/document.rs` (diffs and
  numbered excerpts), `tools/mod.rs` (`validate_path`).
- A decision model is exposed by `provider/systemone.rs::DecisionModel` (implemented by
  `SystemOneClient`). Compaction pruning (`agent/compaction/prune.rs`) and both watchdogs use
  this same seam; questions and answer interpretation stay with the caller. The standalone
  `instinct-flow` crate remains independent and owns its own protocol client.
- A new model call goes through `ModelClient`. A new navigator layer seam goes in `ports.rs`, with
  its adapter in `models.rs` and its fake in `tests/common/navigator.rs`.
- `nav run` exit status mapping lives in `app.rs`: 0 complete, 1 error, 2 yielded, 3 permission
  required, 130 interrupted.
- A new trigger record field goes in `PendingTrigger`; nothing else writes trigger records.
- Tunable editor behaviour goes in `src/flows/builtin/` or custom flow TOML/templates, not in a second evaluator.
- Permission policy goes in `src/permission/`: a new fixed check in `floor.rs`, a new step of the
  order in `decide.rs` (a pure function: it reads rules and asks `PermissionFlows`, it runs nothing
  and writes nothing), a new table or query in `store.rs`, `rules.rs`, `decisions.rs` or
  `validation.rs`. The agent loop reaches it only through `agent/authorize.rs`. A judgement a
  Captain may want to tune goes in the permission flow (`src/flows/builtin/permission-default.toml`),
  not in Rust. Never add a per-tool bypass: an unknown tool, MCP included, is decided like any other.
- A new MCP capability goes in `src/mcp/`; its tools reach the agent as ordinary registry tools named
  `mcp__<server>__<tool>`, so permissions, events and the context budget need no MCP-specific code.
- Historical `pipeline.toml` parsing belongs only in `src/flows/migrate/legacy.rs`; it has no runtime engine.
- The embedded server both clients start (`EmbeddedServer`, the bearer token, the health check) is in `app/embedded.rs`; do not copy it into a new subcommand.
- A workspace's git state is read by `workspace/git.rs`, for any caller; the server must not import `tui`.
- `src/tui/**` is a server client: HTTP + SSE only. It may share serialized bus event types, but never imports session, agent, or bus runtime internals.
- Tests use `tests/common/`: `server.rs` (configs, servers, sessions, messages), `sse.rs` (scripted
  chat streams), `navigator.rs` (the in-memory LSP client and one scripted fake per model seam). Do
  not copy a harness into a new test file.
- Text that reaches the model besides the history (rules, the skills catalog) is a separate component
  built in `agent/prompt.rs` from a snapshot the session service loads at create and resume; it is
  never appended to the transcript or to the role prompt. Rules discovery lives in
  `agent/instructions.rs`, skills discovery in `agent/skills.rs`; give both the home through
  `RulesRoots`, never read `~` in a test.
- A place the model may read, without granting it any write (the skill roots), goes in
  `ToolContext.readable_roots`, not in `writable_roots`. That only keeps `write` and `edit` out: a
  project skill inside the workspace is writable like any workspace file, and `bash` is unsandboxed. The `skill` tool is registered per session (`AgentActor::set_skills`),
  never in a direct Phase-1 registry.
- The role prompt of the primary agent reaches the model through `AgentActor::role_message`, composed
  with editor-context instructions when attached; the turn reads a snapshot of the selected agent when it starts
  (`RuntimeStatus.selection`), so a tool dispatch can read the agent of its turn from `actor.role`.
  Agent files are discovered with `AgentRoots` (give it the home, never read `~` in a test).
- Keep a production file under ~600 lines. Past that, split by responsibility, not by size.
- Comments name the domain reason and dated evidence; never a ticket or plan id.
