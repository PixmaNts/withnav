# Architecture

The map of Navai's codebase: what runs, which module owns what, how data flows, and where new code
goes. Vocabulary is defined in [`CONTEXT.md`](CONTEXT.md); the decisions behind this shape are in
[`docs/adr/`](docs/adr/). Read this before adding a module or a helper: most past duplication came
from agents who did not know the piece already existed.

## What runs

```
VS Code ── vscode-navigator (TypeScript)
   │   ├─ LSP over stdio ─────────────────┐  edits, saves, diagnostics, inline completion, remarks
   │   └─ HTTP + SSE, bearer token ───┐   │  the conversation with the cognition
   ▼                                  ▼   ▼
navai-lsp   (one process, two channels: ADR 0006)
   ├─ navigator/   Language Server, the four layers, decision pipeline, log, replay
   ├─ server/      session HTTP/SSE API, SQLite conversation history
   ├─ bus/         per-session sequencer, event envelopes, inboxes, the agent runtime task
   ├─ agent/       the coding-agent loop (the cognition runs on it)
   ├─ tools/       eight registry tools: read, write, edit, grep, glob, fetch, bash, ask; each session adds `skill`
   └─ provider/    model access: config, auth, one HTTP client, chat SSE, FIM, System One
```

A second binary, `navai` (`src/main.rs`, `src/app.rs`), is a thin CLI over the same library:
`run` (one task in a terminal), `serve` (the session server alone), `auth`, and `tui` (the terminal
chat client, below). `navai run` exits 3 when permission is required and 130 when interrupted.

## Modules

Dependencies point down this list. A module never imports one above it.

| Module | Owns | Does not own |
|---|---|---|
| `settings.rs` | the user settings file `<config_dir>/config.json`: `load`, atomic `save` that keeps keys it does not know; each client reads its own key (`tui.theme`) | what a setting means |
| `provider/` | `models.json` aliases (`config.rs`, `load_models`), keys (`auth.rs`), one `ModelClient` per endpoint (`client.rs`: auth header, deadline, cancellation, status errors), the chat SSE decoder (`sse.rs`, `mod.rs`), the FIM and System One wire clients | anything navigator-specific: no `crate::navigator` import |
| `tools/` | the tool trait, registry, path validation (`validate_path`), and eight registry tools; `glob.rs` provides bounded file search, `fetch.rs` provides bounded HTTP(S) GET, `task.rs` starts a background subagent, `nofollow.rs` opens reviewed write/edit destinations without following links; file tools use `ToolContext`'s writable and read-only roots (`read`/`grep`/`glob` accept both, `write`/`edit` only the writable roots); `bash` starts in the workspace but is not confined to these roots | which tools a session gets; the per-session `skill` tool is installed by `agent/` |
| `agent/` | `AgentActor` (transcript, the pending interaction (an `ask` or a permission request), the session's folder grants, optional role prompt, the rules and skills snapshots), `run_agent` and `resolve_permission` with one `AgentEvent` observer, the permission gate (`permissions.rs`: `needs_permission`, `Scope`, `Grants`, `PermissionRequest`, the `plan` exemption), the role guard (`guard.rs`: the `RoleGuard` trait, the `role_conformance` question, `consult`; `session/guard.rs` wires Jev from the `instinct` alias), request building, the context budget (`context.rs`: the estimate by category, `Budget::compute`, `ContextTracker`), compaction (`compaction.rs`: plan, checkpoint, effective history; `compaction/summary.rs`: the summary request and `prepare`), the local instructions (`instructions.rs`: `AGENTS.md`/`CLAUDE.md`/`rules.md` discovery, caps, diagnostics), the skills (`skills.rs`: `SKILL.md` discovery and precedence; `skills/`: frontmatter, the bounded catalog message, the `skill` tool), the agent definitions (`definitions.rs`: built-in `build`/`plan`/`explore`/`review` and the `.md` loader with its precedence and reserved names; `selection.rs`: the agent a session runs as, switchable only between turns) | events, persistence |
| `bus/` | ordering: the sequencer gives every event its `seq`; envelopes and the event catalog (`event.rs`); commands and inboxes; the runtime task that drives `run_agent` and publishes events | HTTP, storage |
| `session/` | the `Service` (registry/router, `service.rs`), child creation and parent policy snapshot, result delivery callbacks and owner permission routing, tree teardown (`lifecycle.rs`), actor construction (`actor.rs`), the SQLite history (`store.rs`, including `child_links`, terminal CAS and completion outbox), primary agent selection, session commands and variants | HTTP, SSE, any profile by name |
| `server/` | HTTP routes and SSE with `Last-Event-ID` resume; `AppState` wraps the `Service` | session state, the model layers |
| `navigator/` | everything the driver perceives as the navigator, below | session internals |

### Inside `navigator/`

| File | Responsibility |
|---|---|
| `lsp/` | the `LanguageServer` adapter and custom `navigator/*` methods (`mod.rs`), wire types (`protocol.rs`), URI to path (`uri.rs`). Thin: it delegates to `runtime`. |
| `runtime/` | the shared state behind the LSP: idle timers, the conversation-turn listener, trigger evaluation (`trigger.rs`), remark and wake delivery (`delivery.rs`, the wake text in `wake.rs`), reflex-facing calls (`inline.rs`) |
| `document.rs` | one `OpenDocument` per uri (text, baseline, version, diagnostics, edit history) and the `snapshot` a trigger reads |
| `reflex.rs` | inline suggestions: position conversion, prefix and suffix caps, debounce, staleness, and the reflex-context cache |
| `metacognition/` | the one `Metacognition` implementation: remark prompt and tool loop with its budget (`mod.rs`), answer parsing (`answer.rs`), the read/grep tools (`tools.rs`), the reflex-context producer and its prompt (`reflex_context.rs`) |
| `pipeline/` | the decision pipeline: TOML model (`mod.rs`), load-time validation and compiled conditions (`validator.rs`), snapshot and file resolution (`snapshot.rs`), `{{var}}` templates (`templates.rs`), evaluation and branch selection (`evaluator.rs`) |
| `replay/` | frozen-trigger replay of the log against a candidate pipeline, reusing the pipeline's prefilters and branch selection (`mod.rs`); log parsing (`log.rs`); the report and its text rendering (`report.rs`) |
| `log.rs` | the JSONL decision log: record types, `PendingTrigger::finish` (the only writer of trigger records) |
| `conversation.rs` | `ConversationFeed`: the active conversation, its digest, the ask guard, wakes, driver-turn events |
| `intent.rs` | `.navigator/intent.md`: read, section-wise atomic write, and the cognition's `intent` tool |
| `ports.rs` | the model seams (`FimModel`, `DecisionModel`, `Metacognition`, `ReflexContextModel`) and `ModelError` |
| `models.rs` | adapters from the provider clients to those seams |
| `bootstrap.rs` | process wiring: builds each layer from its alias, starts the in-process session server |
| `default_pipeline/` | the built-in pipeline TOML and prompts; a project's `.navigator/` overrides them |

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
|---|---|
| `mod.rs` | `TuiOptions`, `TuiError`, `run` |
| `protocol.rs` | the wire bodies the client sends and receives, and the three message types of the loop: `Action` (what the user asked), `Effect` (I/O the reducer wants), `ClientEvent` (what came back, tagged with a `Generation`) |
| `state.rs` | `State`: the open `Tab`s (each with its conversation projection, session, model, variant, agent, activity, generation, draft, transcript position and unread mark) and what they share (catalog, session list, server commands, workspace); it derefs to the active tab. `UiState` (overlay, scroll, selection, what the variant list is for, sidebar and right column shown); data only |
| `git.rs` | the workspace's branch and changed files: `read` runs `git status` (with a timeout, never on the reducer), `parse` reads its output; `None` outside a repository |
| `client.rs` | the HTTP client: models, sessions, create/resume, send, stop, the command list and a command run, a permission answer (409 `permission_stale` read as stale), open the event stream; send failures split into rejected and uncertain |
| `stream.rs` | one SSE subscription: decoding, reconnect with backoff, resume after the last seq, cancellation |
| `reducer/mod.rs` | `update`: `ClientEvent` to `State` changes and new `Effect`s; routes each event to the tab holding its generation, marks a background tab unread, drops events no tab holds |
| `reducer/envelope.rs` | `EventEnvelope` to blocks: steps, tools (planned, running, settled), usage, errors, command results, permission prompts (open until their `permission.resolved`); the tab's agent from `session.agent_changed`, its variant from a `/variant` result, its model, variant and window from `model.changed` (with a one-line notice for a switch; a catalog refresh never overwrites them); the tab's own `/model` waits on the operation id of its HTTP answer, shown as pending once accepted for after the turn and ended only by that operation's `command.result` or `model.changed`, a new switch, a stop, the session's end or the tab's close; events and replays never send a command; most blocks are reconstructed alike, but a deferred switch acceptance names its target only live when matched to this tab's pending operation, or when the event includes the alias (replay otherwise shows generic text) |
| `reducer/command.rs` | `outcome_text`: a command result as the short text the transcript shows, in the client's own words; `tokens` for `42k` / `1M` |
| `reducer/actions.rs` | `handle_action`: `Action` to `UiState`/`State` changes and `Effect`s; opening (at most nine), switching and closing tabs; server commands, the build/plan switch, the model and variant picks: on a live conversation the model list and `/model <alias>` send `/model` only (the server applies the target's default variant), nothing for the alias in use, refused locally with the reason while a question or permission request waits; on an empty tab the model and variant picks set what the conversation starts on |
| `reducer/text.rs` | `strip_controls`, applied to every piece of model and tool text |
| `input.rs` | the `Editor` and `route_key`/`route_paste`: keys to editor edits or `Action`s; the open permission request's `y`/`a`/`n` and deny message; the second enter before text that would cancel it |
| `commands.rs` | the command catalog, the key help and the key to `Action` map; `palette`, the client's and the server's commands in one list; `route`, the server's prefix rule (a slash in the first column, `//text` is the literal `/text`) with the client's own commands kept local and their arguments checked; `variant_choices`, the rows of the variant list |
| `runtime.rs` | `Runtime`: owns state, editor, client and streams; runs an event's effects for the tab it reached; the event loop with its frame tick |
| `runtime/effects.rs` | `Executor`: runs each `Effect` in a task and reports a tagged `ClientEvent`; owns one live stream per tab generation and stops those no tab holds |
| `runtime/tests.rs` | executor and loop tests with no terminal and a dead server |
| `terminal.rs` | raw mode and the alternate screen, restored on exit and on panic |
| `theme.rs` | the four themes (`ThemeId`, the `Theme` colour roles), the RGB `blend` for tints, and the semantic styles the views draw with |
| `view/mod.rs` | `draw`: layout of header, sidebar, transcript, right panel, status line, editor, key bar and overlays; the key bar lists the keys that apply now, the prompt's while one waits |
| `view/header.rs` | the header row: `◆ navai`, workspace, git branch and change count, `▌agent` in its colour, alias · model · variant · context gauge, and an accepted model switch scheduled for after the turn; the path and model give way first |
| `view/panel.rs` | the right panel (36 columns, hidden under 140 unless `alt+r` asks): Agents and role, Context breakdown and last compaction, Skills listed and omitted, and Changes from git; a section with no data shows `—` |
| `view/sidebar.rs` | the left conversation sidebar: number, title, `<agent> · <state>`, unread mark, active tint and tab keys; hidden under 80 columns (`alt+b` also toggles it) |
| `view/transcript.rs` | block layout and drawing: messages by origin, steps, tools, errors, permission prompts, scroll and selection |
| `view/permission.rs` | the inline permission prompt: tool and scope, a short preview of the call (a write's first lines, an edit's change, a command, a URL), the answer keys or the outcome |
| `view/tool.rs` | the one-line summary and the expanded output of a tool call |
| `view/markdown.rs` | markdown to styled lines, cached per block revision |
| `view/text.rs` | terminal-column measurement and grapheme-safe truncation and padding |
| `view/status.rs` | the status line of the active tab: activity, connection, delivery warning, notice, session tokens; `context_gauge` and `shorten`, shared with the header |
| `view/pickers.rs` | the model (alias, model, window, variants; on a live conversation enter switches the model only, `ctrl+n` opens a new tab, with a note saying so, when the switch waits for the turn, or why it is refused), variant, session, command, theme and help overlays: centred, accent border, the screen behind dimmed, a `❯` filter line and the keys at the foot; a long list scrolls to its selected row |

## Data flows

**Key to screen (terminal client).** A key reaches `runtime.rs`, which calls `input::route_key`: it
edits the editor or returns an `Action`. `reducer::handle_action` changes the state and returns
`Effect`s. The `Executor` runs each effect: an HTTP call through `client.rs`, or a subscription
through `stream.rs`. Each result comes back on one channel as a `ClientEvent` stamped with the
generation it was started in; `reducer::update` drops events of an older generation, folds the rest
into `State` (envelopes become blocks) and may return more effects. After each batch the loop calls
`view::draw`, which only reads the state. Starting or resuming a conversation in a tab bumps its
generation, so late results of its previous session are ignored. Picking a model for a live
conversation sends `/model <alias>` on that same session; on an empty tab it changes the starting
configuration. A conversation is created on the first message, not before.

**Driver-selected skills.** `/skills` reads `GET /v1/sessions/{id}/skills`: the session snapshot's
catalog-listed names and descriptions, or a disabled reason when its agent disallows `skill`.
The searchable picker fills `/skill <name> ` without sending anything. Sending adds a typed
`{type:"skill",name}` content block beside the driver's text; with no request the text is
`Apply the <name> skill.`. Session admission resolves one listed skill through the skill tool's
bounded loader before accepting anything. The inbox appends its exact output as JSON in a
`<driver_selected_skill encoding="json">` block in the driver's own user message. JSON escapes
`<`, `>` and `&`, so loaded text cannot close the delimiter. Unknown, omitted,
disabled or unreadable selections refuse the whole message; clients cannot supply loaded content.
The turn's pinned agent policy is checked again after admission: an intervening agent switch can
refuse the whole command before answering an ask, cancelling permission, changing the driver task,
appending text or persisting child delivery. A durable `command.result` uses its command id as
operation id, causation and correlation; other queued commands proceed.
`message.received.selected_skill` records `{name,origin:"driver"}` for live and replayed clients.
No synthetic tool exchange or model-visible provenance argument is used. Selected instructions
stay with their driver message through storage and latest-batch compaction protection. Multiple
queued selections all stay verbatim, or the existing capacity error prevents the request; no
skill is silently truncated. Event text remains driver text only for display.
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
when the server says so (`session.agent_changed`, the `/variant` result, the session summary), so a
refusal leaves them as they were; each keeps the seq that last set it, and a summary read at an older
seq does not overwrite it. Before a conversation exists, the agent and the variant picked with
the model are sent in the create request instead.

**Tabs (terminal client).** Every tab has its own generation, never shared, and the executor keeps one
SSE stream per generation. An event is applied to the tab that holds its generation, whether it is on
screen or not (a background tab is then marked unread), and the effects it returns run with that tab's
session. New-conversation actions such as `/new`, `ctrl+n`, and `ctrl+n` in the model picker allocate
or reset a generation; `/sessions` opens or resumes a selected session in a tab. Picking a model for
a live conversation keeps its tab, generation and session and sends `/model <alias>`; on an empty tab,
the model pick only sets its starting model and variant. Closing a tab drops its generation, so the runtime
cancels its stream and its late results are ignored; nothing is sent to the server. Before each action the runtime copies the editor into the active tab's draft; the
reducer puts the next tab's draft back with `Effect::SetEditor`. The context gauge is per tab too: each
conversation keeps its snapshot, its epoch and its retired incarnations, a `context.updated` or a
`FetchContext` answer lands on the tab whose generation it carries (without marking it unread), and the
status line shows the active tab's.

**Permission request to answer.** Before each tool call of an assistant message, `run_loop` asks
`needs_permission` and the actor's `Grants`. A call that needs approval is not run: it, and the calls
behind it, become `PendingInteraction::Permission` and the turn yields (`RunOutcome::PermissionRequired`).
The runtime publishes `permission.requested` and the turn ends `yielded`. `POST
/v1/sessions/{id}/permissions/{request_id}` reaches `Service::answer_permission`, which checks the
request id against the runtime's status, then sends a typed `PermissionAnswer` on a channel of its own
(never a `Command`). The runtime publishes `permission.resolved` and calls `resolve_permission`:
the held call runs once (or is denied), the siblings go through the gate again, and the turn goes on.
A stop or a chat message cancels the request; a resume expires every request that has no
`permission.resolved` and settles its calls as interrupted.

**Plan exception to the role guard.** For the `plan` agent `gate` does not look at grants. A `write` or
`edit` whose path as written is not a symlink, and whose destination is an existing regular `.md` file
or a missing `.md` file in an existing directory, runs without a request. A symlink path is not
exempt, even if it points to another Markdown file. Every other `write`
or `edit`, and every `bash` or `fetch`, goes to `guard_step`: `guard::consult` sends Jev one
`role_conformance` choice (role prompt, latest user message, workspace, resolved target, tool,
exact arguments, at most 16 KiB; a larger call skips Jev) under the `SystemOneClient` deadline and the
actor's cancellation. Only a valid `conforms` runs the call, and the verdict is recorded as `guard` in
the metadata of its durable `tool.post_call`. Anything else (`violates`, `uncertain`, a malformed
answer, no `instinct` alias or key, a timeout, an oversized call) is an ordinary permission request
whose `reason` says why; the runtime turns an "allow folder" answer for `plan` into allow once. Jev is
advisory, never a sandbox.

**Keystroke to suggestion (reflex).** The extension asks `navigator/inlineCompletion` after 150 ms
of pause. `runtime/inline.rs` reads the document and the reflex context, `reflex.rs` builds the FIM
request, `provider/fim.rs` calls Mercury, the answer goes back as ghost text. A newer request
supersedes an older one; a stale version is dropped.

**Pause or save to remark or wake (instinct, metacognition).** An idle timer or `didSave` calls
`runtime::evaluate`. It takes a document snapshot, runs the pipeline's free prefilters, asks the
instinct the trigger's questions in one System One call, and selects the first branch whose
condition holds. `remark` calls the metacognition with the rendered branch prompt and sends
`navigator/remark`; `wake_cognition` posts a navigator-sourced message through the
`ConversationFeed`. One trigger record is written per evaluation.

**Chat to cognition.** The extension posts driver messages to the session server. The bus
sequences them, the runtime task runs `run_agent` on the `cognition` alias, events stream back over
SSE. A navigator session also feeds the `ConversationFeed`, attached by `NavigatorHooks::started` (`navigator/session_hooks.rs`; a resume replays without announcing old turns): its digest reaches the fast layers'
prompts, and every driver turn end triggers a `conversation` evaluation on the focused document.

## State on disk

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
It never injects through navigator commands or changes the driver task. The runtime's existing
control channel carries delivery wakes: idle parents run immediately, busy parents wait until the
boundary, and pending interactions cannot be answered by a result. Stopped parents have no wake;
resume drains durable notices. Child deletion keeps unconsumed notices and their result text.

Child permission events are mirrored by `PersistingEmitter` onto the parent stream with optional
`child_session_id`; the child's typed interaction and grants remain authoritative. Owner-scoped
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

**Child watchdog.** `session/subagents/watchdog.rs` owns the monotonic clock and judge seams,
bounded evidence, separate `subagent_progress` System One choice and consecutive-failure rule.
`supervisor.rs` owns one service task per published child, cancelled by lifetime stop or terminal
completion. It sleeps a full configured interval between checks, never replays missed ticks,
skips parked/idle children and awaits Jev outside lifecycle/admission. `bus/runtime/progress.rs`
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
as before; no supervision state is restored. The event catalog has 22 kinds.

| Path | What | Written by |
|---|---|---|
| `~/.config/navai/models.json`, `auth.json`, `config.json` | model aliases, provider keys and client settings | the driver, `navai auth`, the TUI |
| `~/.config/navai/navigator/pipeline.toml`, `prompts/*.md` | the user's pipeline and prompts | the driver |
| `~/.config/navai/agents/*.md` | user agent definitions | the driver |
| `<workspace>/.navigator/pipeline.toml`, `prompts/*.md` | the project's pipeline and prompts, overriding the user and built-in defaults | the driver |
| `<workspace>/.navigator/intent.md` | intent, plan, decisions | the cognition and the driver |
| `<workspace>/.navai/agents/*.md` | project agent definitions | the driver |
| `~/.agents/AGENTS.md`, `~/.agents/rules.md`, `~/.agents/skills/` | user rules and skills included in agent sessions | the driver |
| `~/.local/share/navai/navai.db`, `navai.jsonl`, `tui.log`, `model-catalog/` | conversations, decision log, TUI log and cached provider model lists | `session/store.rs`, `navigator/log.rs`, `tui/`, `provider/catalog.rs` |

Data and config paths come from `paths.rs` and `provider/auth.rs`. On every start `paths::migrate_legacy`
(`paths/migrate.rs`) moves what the former names left behind to the Navai dirs: `~/.local/share/navigator/*`
and `~/.local/share/rust-agent/*` (`navigator.db` or `navigator/sessions.db`, with their `-wal` and `-shm`,
`navigator.jsonl`, `tui.log`) to `~/.local/share/navai/`, and `~/.config/rust-agent/*` (`auth.json`,
`models.json`, `navigator/pipeline.toml`) to `~/.config/navai/`. A file is never overwritten, a second
start does nothing, and a database moves only when no other process holds it: otherwise it stays where
it is and that run keeps using it.

## Where new code goes

- A chat alias's context window (models.json, else the provider's model list, cached in
  `<data dir>/model-catalog/`): `provider/catalog.rs` (`resolve_chat`); the session service calls it
  before building an actor and refuses an alias with no window. The result is saved once in
  `sessions.model_resolution` (`session/store.rs`, versioned JSON, no credentials): a resume reads it
  back instead of resolving again, so a live or resumed conversation keeps its bounds, and an alias
  retargeted to another provider, model or api is `ServerError::ModelIdentityChanged`, never a silent move.
  The metacognition resolves through the same function in `navigator/bootstrap.rs`.
  The fetch is single-flight per endpoint, bounded (3 s, 2 MB, 10 000 entries), never follows a
  cross-origin redirect (`ClientCache::get_same_origin`) and keeps stale values on failure; `GET /v1/models`
  (`server/routes.rs`) reports the source, staleness and discrepancy of each alias.
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
  says `/compact` cannot help and to use a larger window. A model switch reuses `summarize` and
  `install` (`bus/runtime/model.rs::compact_for_target`): the old model compacts, aiming at the
  target's budget, in an acknowledged commit that precedes the switch's own. These are two
  commits, not one transaction: if the switch then fails (or a stop cancels it) the conversation
  stays on the old model WITH the new checkpoint, and a resume restores exactly that. The
  checkpoint is sized for the target's budget, not the old model's.
- Model switching (`/model <alias>` inside a conversation): `bus/runtime/model.rs`, run like
  `/compact` through the same control channel (`RuntimeStatus::request_model_switch`), between turns
  or after the running turn, from one ordered queue of deferred controls (`bus/runtime/deferred.rs`;
  adjacent compactions are one). The session service supplies the `ModelSwitch` callbacks
  (`session/service/resolve.rs`: the target resolved like a new session, its default variant;
  `HistoryStore::switch_model`: `sessions.model`, `model_resolution`, the variant, the nullable
  `sessions.reasoning_floor` and the `model.changed` event row in one transaction), so the bus never
  imports the service or the store. The write goes through `Sequencer::commit_acked`: the event is
  sequenced, written and acknowledged by the store, and only then emitted; a failed write emits
  nothing and gives the seq back. `SelectedModel` (alias, resolution, revision, variant, offer) is one
  value behind one lock: the commit callback installs it, together with the runtime's variant and the
  context gauge, before the event is emitted, so a reader sees the old selection or the new one. `AgentSelection::switching`
  refuses `/agent`, `/variant`, `/compact` and another switch while one prepares. After the
  commit callback installs the selection, runtime variant and context tracker; `commit_acked` then
  emits `model.changed` before acknowledging the runtime. Only after that acknowledgement does the
  runtime swap the actor's provider, model, variant and limits. `AgentActor::reasoning_floor`
  (restored on resume) keeps the earlier reasoning out of the request projection unless the resolved
  identity is the same; the fit check measures that destination projection (`context_breakdown_at`).
- A helper you need probably exists. Look first in: `provider/client.rs` (any HTTP call to a model),
  `provider/config.rs` (`load_models`), `navigator/log.rs` (`excerpt`, `new_ulid_string`),
  `bus/event.rs` (`now_rfc3339`), `navigator/lsp/uri.rs`, `navigator/document.rs` (diffs and
  numbered excerpts), `tools/mod.rs` (`validate_path`).
- A new model call goes through `ModelClient`. A new navigator layer seam goes in `ports.rs`, with
  its adapter in `models.rs` and its fake in `tests/common/navigator.rs`.
- `navai run` exit status mapping lives in `app.rs`: 0 complete, 1 error, 2 yielded, 3 permission
  required, 130 interrupted.
- A new trigger record field goes in `PendingTrigger`; nothing else writes trigger records.
- Behaviour that should be tunable goes in the pipeline (TOML and prompts), not in Rust (ADR 0005).
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
  with the profile's instructions; the turn reads a snapshot of the selected agent when it starts
  (`RuntimeStatus.selection`), so a tool dispatch can read the agent of its turn from `actor.role`.
  Agent files are discovered with `AgentRoots` (give it the home, never read `~` in a test).
- Keep a production file under ~600 lines. Past that, split by responsibility, not by size.
- Comments name the domain reason and dated evidence; never a ticket or plan id.
