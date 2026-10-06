# Architecture

The map of Navai's codebase: what runs, which module owns what, how data flows, and where new code
goes. Vocabulary is defined in [`CONTEXT.md`](CONTEXT.md); the decisions behind this shape are in
[`docs/adr/`](docs/adr/). Read this before adding a module or a helper: most past duplication came
from agents who did not know the piece already existed.

The runtime map below describes the implementation prepared for **0.5**, not which binaries
are installed on the driver's machine. See
[ADR 0011](docs/adr/0011-layers-models-and-agent-overrides.md) and the contract section below.

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
`run` (one task in a terminal), `serve` (the session server alone), `auth`, `tui` (the terminal
chat client, below) and `web` (the browser chat client, below). `navai run` exits 3 when permission is required and 130 when interrupted.

## Modules

Dependencies point down this list. A module never imports one above it.

| Module | Owns | Does not own |
|---|---|---|
| `settings.rs` | the user settings file `<config_dir>/config.json`: `load`, atomic `save` that keeps keys it does not know; each client reads its own key (`tui.theme`) | what a setting means |
| `provider/` | `models.json` catalogue (`config.rs`, `load_models`), canonical identities (`model_name.rs`: `ModelName`), pure assignment resolution (`assignments.rs`) and lower-layer clients (`layers.rs`), keys (`auth.rs`), one `ModelClient` per endpoint (`client.rs`: auth header, deadline, cancellation, status errors), the chat SSE decoder (`sse.rs`, `mod.rs`), the FIM and System One wire clients | anything navigator-specific: no `crate::navigator` import |
| `tools/` | the tool trait, registry, path validation (`validate_path`), and eight registry tools; `glob.rs` provides bounded file search, `fetch.rs` provides bounded HTTP(S) GET, `task.rs` starts a background subagent, `nofollow.rs` opens reviewed write/edit destinations without following links; file tools use `ToolContext`'s writable and read-only roots (`read`/`grep`/`glob` accept both, `write`/`edit` only the writable roots); `bash` starts in the workspace but is not confined to these roots | which tools a session gets; the per-session `skill` tool is installed by `agent/` |
| `agent/` | `AgentActor` (transcript, the pending interaction (an `ask` or a permission request), the session's folder grants, optional role prompt, the rules and skills snapshots), `run_agent` and `resolve_permission` with one `AgentEvent` observer, the permission gate (`permissions.rs`: `needs_permission`, `Scope`, `Grants`, `PermissionRequest`, the `plan` exemption), the role guard (`guard.rs`: the `RoleGuard` trait, the `role_conformance` question, `consult`; `session/guard.rs` wires Jev from the instinct layer assignment), request building, the context budget (`context.rs`: the estimate by category, `Budget::compute`, `ContextTracker`), compaction (`compaction.rs`: plan, checkpoint, effective history; `compaction/summary.rs`: the summary request and `prepare`), the local instructions (`instructions.rs`: `AGENTS.md`/`CLAUDE.md`/`rules.md` discovery, caps, diagnostics), the skills (`skills.rs`: `SKILL.md` discovery and precedence; `skills/`: frontmatter, the bounded catalog message, the `skill` tool), the agent definitions (`definitions.rs`: built-in `build`/`plan`/`explore`/`review` and the `.md` loader with its precedence and reserved names; `selection.rs`: the agent a session runs as, switchable only between turns) | events, persistence |
| `bus/` | ordering: the sequencer gives every event its `seq`; envelopes and the event catalog (`event.rs`); commands and inboxes; the runtime task that drives `run_agent` and publishes events | HTTP, storage |
| `session/` | the `Service` (registry/router, `service.rs`), child creation and parent policy snapshot, result delivery callbacks and owner permission routing, tree teardown (`lifecycle.rs`), actor construction (`actor.rs`), the SQLite history (`store.rs`, including `child_links`, terminal CAS and completion outbox), primary agent selection, per-agent model/variant overrides and editor context | HTTP, SSE, lower-layer assignment ownership |
| `server/` | HTTP routes and SSE with `Last-Event-ID` resume; `AppState` wraps the `Service`; `web.rs`: the embedded browser-client files, `GET /v1/workspaces`, `GET /v1/intent` | session state, the model layers |
| `workspace/` | what is known of a workspace directory regardless of any session: `git.rs` reads its branch and changed files (`git status -z`, bounded by a deadline, `None` outside a repository); used by the terminal client's header and panel and by `GET /v1/workspaces` | sessions, HTTP |
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
| `bootstrap.rs` | process wiring: builds each lower layer from its assignment, starts the in-process session server |
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
| `theme.rs` | the four themes (`ThemeId`, the `Theme` colour roles), the RGB `blend` for tints, and the semantic styles the views draw with |
| `view/mod.rs` | `draw`: layout of header, sidebar, transcript, right panel, status line, editor, key bar and overlays; the key bar lists the keys that apply now, the prompt's while one waits |
| `view/header.rs` | the header row: `◆ navai`, workspace, git branch and change count, `▌agent` in its colour, model · variant · context gauge, and an accepted model switch scheduled for after the turn; the path and model give way first |
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

### The browser client (`navai web`)

`navai web` (`src/app/web.rs`) starts the same private in-process server as `navai tui`
(`src/app/embedded.rs`: preflight on every `--workspace`, loopback only, a per-process bearer token,
graceful stop), prints `Navai WebUI: http://127.0.0.1:<port>/#token=<token>`, opens it with
`xdg-open`/`open` unless `--no-open`, and runs until Ctrl-C. The token rides in the URL fragment, so the
browser never sends it to a server; the page keeps it and sends it as a bearer header.

The single-page app lives in `webui/`; its build output `webui/dist/` is committed and embedded in the
binary with `include_dir`, so `cargo build` needs no Node. The files are served by `server/web.rs` only
when `ServerConfig.web` is set (`navai web` alone; `serve`, the terminal client and the language server
leave it `None` and answer only the API): `GET /` and any non-`/v1/` path return the file in `dist/`,
else `index.html`. Those paths and `/v1/health` need no token; every other `/v1/*` route still does,
and an unknown `/v1/` path is a 404, never the page. HTML carries a strict Content-Security-Policy,
`nosniff` and `no-referrer`.

Two read endpoints serve the page, both behind the token: `GET /v1/workspaces` lists the launch
workspaces (kept in `WebConfig`, argument order, `launched:true`) and then the workspaces of the
history store (`HistoryStore::workspaces`: top-level conversations only, directories that no longer
exist skipped), each with its git branch and change count; `GET /v1/intent?workspace=` returns the
parsed `.navigator/intent.md`.

## Data flows

Scratch roots default to private `$XDG_RUNTIME_DIR/navai` (fallback `~/.cache/navai/scratch`) and
shared `<temp_dir>/navai`, snapshotted in `permissions.scratch_dirs`. Every component is opened
descriptor-relatively without following links. Native execution carries the reviewed root identity,
opens it again without creation and writes relative to that descriptor. Validation is read-only.

The actor carries an explicit protected config root. Resolved native write/edit targets inside it
ask the driver before instinct, metacognition or scratch exemptions. Literal config references in
bash ask too; bash retains ambient access and cannot be fully confined. Scratch choice is inherited
from the parent's execution snapshot, and its no-follow descriptor/inode validation is repeated.

Permission review includes outside-project write/edit destinations, binding approvals to one canonical
file without expanding writable roots. Sessions install private scratch storage from
`permissions.scratch_dirs` (ordered absolute paths, private runtime/cache first, shared temp second; `[]` disables); the owned, non-symlink
0700 directory is a writable root and counts as project-local in review, including child sessions.

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
Policy pinning does not consume a pending ask or clear its permission slot or answer claim:
only accepted input or a matching answer starts interaction consumption. Replay task lookup
excludes correlated whole-message refusals, so resume and new children keep the real driver task.
`message.received.selected_skill` records `{name,origin:"driver"}` for live and replayed clients.
No synthetic tool exchange or model-visible provenance argument is used. Selected instructions
stay with their driver message through storage and latest-batch compaction protection. Multiple
queued driver selections all stay verbatim even with interleaved navigator input. Discovery covers
the consecutive user-message run, but only Driver-provenance messages are protected, carried and
charged as irreducible. Non-driver input retains its provenance in the unsummarized tail and may
be summarized before a checkpoint. If driver selections cannot fit, the capacity error prevents the request; no
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

**Permission request to answer.** Before each tool call of an assistant message, `run_loop` asks
`needs_permission`, which validates the call and its paths. Every `bash`, `write` and `edit` then goes
through review, as do native single-file `read` calls outside readable roots or targeting credentials.
`grep`/`glob` outside readable roots are refused; inside walks skip sensitive entries before opening
them and report a count. Directory grep uses ripgrep's normal ignore/hidden rules with case-insensitive
pre-open exclusions generated from the same sensitive-name rules as the gate. Only when ripgrep is
missing, an `ignore::WalkBuilder` fallback runs on a blocking worker with cancellation, deadline,
entry/depth/per-file/total-byte/match limits and partial-result notices.
Inside-root ordinary reads remain ungated but bound. Native reads bind one canonical target, re-resolve
before dispatch and hold a no-follow descriptor; they never expand readable roots. Deterministic
credential/key flags on native reads require metacognition and then the driver even after model approval. Reading
ordinary Navai config follows this policy; config mutation stays driver-only.
Bash is not confined and sensitive-file detection there is best effort, not a guarantee against
arbitrary commands. Literal supported file operands are canonicalized; ambiguous sensitive
expansions tighten risk. Native sensitivity is deterministic. In-root search descendant reopen
races remain a documented follow-up, not grounds to permit outside walks.
Every gated call goes to `permission_gate`: conservative deterministic checks (redirections, mutating and risky command
families, canonical destinations) only tighten, then Jev answers `call_effect`, `call_scope` and
`call_risk` in one bounded request (plus `role_conformance` for `plan`). A read-only call, or a
low-risk change inside the project, runs only if it is not risky. A risky call or outside-project change goes to a synchronous,
tool-free metacognition review (20 s); only `approve` runs it. Anything else (reject, unsure, a
malformed answer, a timeout, an oversized call, no reviewer) becomes a permission request. The
verdicts are recorded as `permission_gate` on `tool.pre_call`, `tool.post_call` and
`permission.requested`. Only `fetch` keeps reusable origin grants (`Grants`); saved bash/file folder
grants are never used. `permissions.automatic_review: false` asks for every such call; children
inherit their parent's value. The review is advisory, never a sandbox (ADR 0010).

A call that needs approval is not run: it, and the calls behind it, become
`PendingInteraction::Permission` and the turn yields (`RunOutcome::PermissionRequired`).
The runtime publishes `permission.requested` and the turn ends `yielded`. `POST
/v1/sessions/{id}/permissions/{request_id}` reaches `Service::answer_permission`, which checks the
request id against the runtime's status, then sends a typed `PermissionAnswer` on a channel of its own
(never a `Command`). The runtime publishes `permission.resolved` and calls `resolve_permission`:
the held call runs once (or is denied) after fresh validation, a changed destination is reviewed
again, the siblings go through the gate again, and the turn goes on. For `bash`, `write` and `edit`
an "allow folder" answer counts as allow once. A stop or a chat message cancels the request; a
resume expires every request that has no `permission.resolved` and settles its calls as interrupted.

**Plan exception to the role guard.** For the `plan` agent, a `write` or `edit` whose path as
written is not a symlink, and whose destination is an existing regular `.md` file or a missing `.md`
file in an existing directory, needs no `role_conformance` answer; it still goes through the
permission review. Every other `write` or `edit`, and every `bash`, asks Jev `role_conformance` in the
same batched request (role prompt, the driver's latest message, workspace, resolved target, tool,
exact arguments, at most 16 KiB); a `fetch` asks it alone. Only a valid `conforms` lets the call
continue, and the verdict is recorded as `guard` in the metadata of its durable `tool.post_call`.
Anything else (`violates`, `uncertain`, a malformed answer, no usable instinct assignment or key, a timeout, an
oversized call) is an ordinary permission request whose `reason` says why. Jev is advisory, never a
sandbox.

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
sequences them, the runtime task runs `run_agent` with the chosen agent's effective chat model, events stream back over
SSE. A navigator session also feeds the `ConversationFeed`, attached by `NavigatorHooks::started` (`navigator/session_hooks.rs`; a resume replays without announcing old turns): its digest reaches the fast layers'
prompts, and every driver turn end triggers a `conversation` evaluation on the focused document.
Editor roots have the same root subagent tools as terminal roots. Children have no editor attachment,
do not receive `intent`, and are never registered by `started`. Completion uses the normal durable
outbox and root stream, not the latest-wins navigator wake channel. The digest labels delivered
subagent text as untrusted and never treats it as a driver turn.

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

**Root watchdog.** `session/subagents/root_watchdog.rs` reuses the child clock, evidence, judge and
consecutive-failure rule. The service installs one task per root runtime. It skips idle and human
waits, checks the turn generation again after each model await, and submits a safe-boundary stop or
doubt hold through `WatchdogPark`. A local mockable `ProgressReviewer` performs one tool-free
metacognition call within 20 seconds; undecidable reviews ask the driver. Notices reuse
`command.result`; the runtime emits typed `agent.yield` at the final safe-boundary check after
awaited delivery, snapshots settled tools and ends the paused turn. Hold identity is a session
incarnation plus unique hold id. Continue consumes the matching token atomically and sequences
resolution; restart/stop expires it. A driver message supersedes it and records interruption;
autonomous inputs cannot release it or renew a parked budget. The event catalog remains 22 kinds.
The typed boundary result carries the settled transcript on every watchdog exit and keeps the
remaining steps. Interrupt-delivery checks revalidate the hold after awaiting notice eligibility.
Recovery installs an awaiting-fresh-driver gate; neither delivery wakes nor delegated provenance
can clear it. Deferred controls accepted in a paused turn receive operation-correlated cancellation.
Driver admission events never clear the recovered gate. The runtime commits the consumed driver
transcript and a `command.result` checkpoint marker atomically through the existing acknowledged
transcript/event transaction before releasing the gate. Recovery reads that marker, not admission;
an admitted but unconsumed correction therefore requires fresh input after another crash. Accepted
interrupt guidance on a budget-parked child stays queued and is claimed once by authorized Continue
before its first resumed provider request; the guidance itself never renews the budget.
Progress parking filters autonomous-only batches before policy pinning. A driver batch pins its
role before validating selected skills; held-only or refused batches release that pin without
changing the interaction claim, progress generation, goal or recovery gate. Only applied commands
supersede a watchdog hold and commit fresh input. `consume_turn` starts activity and the watchdog
after that validation, using the updated driver task, before transcript-persisting turn delivery.
Root `alt+c`
uses the existing Continue route with the root as both owner and target, which cannot answer asks
or permissions. Root turns are unbounded unless `NAVAI_MAX_STEPS` is set; unavailable instinct
or disabled supervision installs a visible renewable 500-step fallback cap. Children keep their
50-step budget. The root supervisor starts only after publication; runtime exit cancels and joins
it outside admission and registry locks.

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
| `~/.config/navai/models.json`, `auth.json`, `config.json` | model catalogue, provider keys, assignments and client settings | the driver/importer (catalogue), `navai auth` (keys), TUI (client settings, not assignments) |
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
fallbacks: no suggestions, driver permission review, renewable 500-step supervision cap when
instinct is unavailable. Invalid nondefault agent models warn and fall back to the default agent.
An unknown/nonprimary default agent or one without a usable chat model prevents server/LSP startup.

Root selection resolves the `(session, agent)` override, then the agent assignment, then the
default agent's model. Children with an assignment use it; unassigned children inherit the
parent's effective model and variant. `/model` without a variant takes the target model default.
`/variant` distinguishes named, `off` and `inherit`: the latter removes the local variant choice.
Switching agents or resuming restores that agent's durable override, never mutates global config,
and never applies a deferred selection to a different agent. `/new [agent]` starts without old
session overrides. Editor context adds prompt/intent/feed independently of the chosen agent.

| Route | Response/request contract |
|---|---|
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

- Settings HTTP edits live in `server/settings.rs`: shared `settings::update` locks a stable sidecar
  across snapshot read, revision comparison, mutation and atomic save. `server/settings/editor.rs` owns
  editable keys, defaults and scalar validation; `provider/assignments.rs` checks proposed model
  API and offered variants without runtime fallback. Unknown config keys and siblings survive.
  `restart` means after Navai restarts (conservative for layers). `new_sessions` means next use:
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
- A new model call goes through `ModelClient`. A new navigator layer seam goes in `ports.rs`, with
  its adapter in `models.rs` and its fake in `tests/common/navigator.rs`.
- `navai run` exit status mapping lives in `app.rs`: 0 complete, 1 error, 2 yielded, 3 permission
  required, 130 interrupted.
- A new trigger record field goes in `PendingTrigger`; nothing else writes trigger records.
- Behaviour that should be tunable goes in the pipeline (TOML and prompts), not in Rust (ADR 0005).
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
