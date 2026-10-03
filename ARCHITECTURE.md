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
   ├─ tools/       read, write, edit, grep, glob, fetch, bash, ask
   └─ provider/    model access: config, auth, one HTTP client, chat SSE, FIM, System One
```

A second binary, `navai` (`src/main.rs`, `src/app.rs`), is a thin CLI over the same library:
`run` (one task in a terminal), `serve` (the session server alone), `auth`, and `tui` (the terminal
chat client, below).

## Modules

Dependencies point down this list. A module never imports one above it.

| Module | Owns | Does not own |
|---|---|---|
| `settings.rs` | the user settings file `<config_dir>/config.json`: `load`, atomic `save` that keeps keys it does not know; each client reads its own key (`tui.theme`) | what a setting means |
| `provider/` | `models.json` aliases (`config.rs`, `load_models`), keys (`auth.rs`), one `ModelClient` per endpoint (`client.rs`: auth header, deadline, cancellation, status errors), the chat SSE decoder (`sse.rs`, `mod.rs`), the FIM and System One wire clients | anything navigator-specific: no `crate::navigator` import |
| `tools/` | the tool trait, the registry, path validation (`validate_path`), the eight tools (`glob.rs`: bounded, link-free file search; `fetch.rs`: HTTP(S) GET with its own credential-free client, same-origin redirects, a 5 MiB streamed cap; HTML is converted by a memory- and CPU-limited child, `navai internal-html-convert`, killed on cancel or timeout); `ToolContext` with the writable roots and the read-only ones (`read`/`grep`/`glob` accept both, `write`/`edit`/`bash` only the first) | which tools a session gets |
| `agent/` | `AgentActor` (transcript, pending `ask`, optional role prompt, the rules and skills snapshots), `run_agent` with one `AgentEvent` observer, request building, the context budget (`context.rs`: the estimate by category, `Budget::compute`, `ContextTracker`), the local instructions (`instructions.rs`: `AGENTS.md`/`CLAUDE.md`/`rules.md` discovery, caps, diagnostics), the skills (`skills.rs`: `SKILL.md` discovery and precedence; `skills/`: frontmatter, the bounded catalog message, the `skill` tool) | events, persistence |
| `bus/` | ordering: the sequencer gives every event its `seq`; envelopes and the event catalog (`event.rs`); commands and inboxes; the runtime task that drives `run_agent` and publishes events | HTTP, storage |
| `session/` | the `Service` (registry and router, `service.rs`), teardown and close (`lifecycle.rs`), actor construction (`actor.rs`), subscriptions, the `SessionHooks` seam (`hooks.rs`: default model, actor setup, a `started` callback with a `SessionSender` that can only post) the SQLite history (`store.rs`), and the session commands (`commands.rs`: the registry and line routing; `run_command.rs`: `Service::list_commands` / `run_command`, each run publishing a `command.result`; `Service::context`), and the session variant (saved in `sessions.variant`, chosen in `service/resolve.rs`, held by the runtime's `RuntimeStatus` and applied at each turn start) | HTTP, SSE, any profile by name |
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
imports the serialized bus types and no runtime internals. Several conversations are open at once,
one per tab; each tab streams its own session in the background.

| File | Responsibility |
|---|---|
| `mod.rs` | `TuiOptions`, `TuiError`, `run` |
| `protocol.rs` | the wire bodies the client sends and receives, and the three message types of the loop: `Action` (what the user asked), `Effect` (I/O the reducer wants), `ClientEvent` (what came back, tagged with a `Generation`) |
| `state.rs` | `State`: the open `Tab`s (each with its conversation projection, session, model, activity, generation, draft, transcript position and unread mark) and what they share (catalog, session list, workspace); it derefs to the active tab. `UiState` (overlay, scroll, selection, sidebar shown); data only |
| `client.rs` | the HTTP client: models, sessions, create/resume, send, stop, open the event stream; send failures split into rejected and uncertain |
| `stream.rs` | one SSE subscription: decoding, reconnect with backoff, resume after the last seq, cancellation |
| `reducer/mod.rs` | `update`: `ClientEvent` to `State` changes and new `Effect`s; routes each event to the tab holding its generation, marks a background tab unread, drops events no tab holds |
| `reducer/envelope.rs` | `EventEnvelope` to blocks: steps, tools (planned, running, settled), usage, errors; live and replay give the same blocks |
| `reducer/actions.rs` | `handle_action`: `Action` to `UiState`/`State` changes and `Effect`s; opening (at most nine), switching and closing tabs |
| `reducer/text.rs` | `strip_controls`, applied to every piece of model and tool text |
| `input.rs` | the `Editor` and `route_key`/`route_paste`: keys to editor edits or `Action`s |
| `commands.rs` | the command catalog, the key help and the key to `Action` map |
| `runtime.rs` | `Runtime`: owns state, editor, client and streams; runs an event's effects for the tab it reached; the event loop with its frame tick |
| `runtime/effects.rs` | `Executor`: runs each `Effect` in a task and reports a tagged `ClientEvent`; owns one live stream per tab generation and stops those no tab holds |
| `runtime/tests.rs` | executor and loop tests with no terminal and a dead server |
| `terminal.rs` | raw mode and the alternate screen, restored on exit and on panic |
| `theme.rs` | the four themes (`ThemeId`, the `Theme` colour roles), the RGB `blend` for tints, and the semantic styles the views draw with |
| `view/mod.rs` | `draw`: layout of sidebar, transcript, editor, status line and overlays |
| `view/sidebar.rs` | the tab sidebar: number, title, `build · <state>`, unread mark, active tint, tab keys; hidden under 80 columns |
| `view/transcript.rs` | block layout and drawing: messages by origin, steps, tools, errors, scroll and selection |
| `view/tool.rs` | the one-line summary and the expanded output of a tool call |
| `view/markdown.rs` | markdown to styled lines, cached per block revision |
| `view/status.rs` | the status line of the active tab: connection, model, activity, context (the server's estimate, else the last prompt count) and session tokens |
| `view/pickers.rs` | the model, session, command, theme and help overlays |

## Data flows

**Key to screen (terminal client).** A key reaches `runtime.rs`, which calls `input::route_key`: it
edits the editor or returns an `Action`. `reducer::handle_action` changes the state and returns
`Effect`s. The `Executor` runs each effect: an HTTP call through `client.rs`, or a subscription
through `stream.rs`. Each result comes back on one channel as a `ClientEvent` stamped with the
generation it was started in; `reducer::update` drops events of an older generation, folds the rest
into `State` (envelopes become blocks) and may return more effects. After each batch the loop calls
`view::draw`, which only reads the state. Picking a model or starting a conversation bumps the
generation, so late results of the old conversation are ignored. A conversation is created on the
first message, not before.

**Tabs (terminal client).** Every tab has its own generation, never shared, and the executor keeps one
SSE stream per generation. An event is applied to the tab that holds its generation, whether it is on
screen or not (a background tab is then marked unread), and the effects it returns run with that tab's
session. Opening a conversation (`/new`, ctrl+n, `/sessions`, a model pick) adds a tab; closing one
drops its generation, so the runtime cancels its stream and its late results are ignored; nothing is
sent to the server. Before each action the runtime copies the editor into the active tab's draft; the
reducer puts the next tab's draft back with `Effect::SetEditor`. The context gauge is per tab too: each
conversation keeps its snapshot, its epoch and its retired incarnations, a `context.updated` or a
`FetchContext` answer lands on the tab whose generation it carries (without marking it unread), and the
status line shows the active tab's.

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

| Path | What | Written by |
|---|---|---|
| `~/.config/navai/models.json`, `auth.json` | aliases and keys | the driver, `navai auth` |
| `<workspace>/.navigator/pipeline.toml`, `prompts/*.md` | the project's pipeline | the driver |
| `<workspace>/.navigator/intent.md` | intent, plan, decisions | the cognition and the driver |
| `~/.local/share/navai/navai.jsonl` | decision log, replay input | `navigator/log.rs` |
| `~/.local/share/navai/navai.db` | conversations | `session/store.rs` |

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
- A helper you need probably exists. Look first in: `provider/client.rs` (any HTTP call to a model),
  `provider/config.rs` (`load_models`), `navigator/log.rs` (`excerpt`, `new_ulid_string`),
  `bus/event.rs` (`now_rfc3339`), `navigator/lsp/uri.rs`, `navigator/document.rs` (diffs and
  numbered excerpts), `tools/mod.rs` (`validate_path`).
- A new model call goes through `ModelClient`. A new navigator layer seam goes in `ports.rs`, with
  its adapter in `models.rs` and its fake in `tests/common/navigator.rs`.
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
- Keep a production file under ~600 lines. Past that, split by responsibility, not by size.
- Comments name the domain reason and dated evidence; never a ticket or plan id.
