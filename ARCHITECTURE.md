# Architecture

The map of this codebase: what runs, which module owns what, how data flows, and where new code
goes. Vocabulary is defined in [`CONTEXT.md`](CONTEXT.md); the decisions behind this shape are in
[`docs/adr/`](docs/adr/). Read this before adding a module or a helper: most past duplication came
from agents who did not know the piece already existed.

## What runs

```
VS Code ── vscode-navigator (TypeScript)
   │   ├─ LSP over stdio ─────────────────┐  edits, saves, diagnostics, inline completion, remarks
   │   └─ HTTP + SSE, bearer token ───┐   │  the conversation with the cognition
   ▼                                  ▼   ▼
rust-agent-lsp   (one process, two channels: ADR 0006)
   ├─ navigator/   Language Server, the four layers, decision pipeline, log, replay
   ├─ server/      session HTTP/SSE API, SQLite conversation history
   ├─ bus/         per-session sequencer, event envelopes, inboxes, the agent runtime task
   ├─ agent/       the coding-agent loop (the cognition runs on it)
   ├─ tools/       read, write, edit, grep, bash, ask
   └─ provider/    model access: config, auth, one HTTP client, chat SSE, FIM, System One
```

A second binary, `rust-agent` (`src/main.rs`, `src/app.rs`), is a thin CLI over the same library:
`run` (one task in a terminal), `serve` (the session server alone), `auth`.

## Modules

Dependencies point down this list. A module never imports one above it.

| Module | Owns | Does not own |
|---|---|---|
| `provider/` | `models.json` aliases (`config.rs`, `load_models`), keys (`auth.rs`), one `ModelClient` per endpoint (`client.rs`: auth header, deadline, cancellation, status errors), the chat SSE decoder (`sse.rs`, `mod.rs`), the FIM and System One wire clients | anything navigator-specific: no `crate::navigator` import |
| `tools/` | the tool trait, the registry, path validation (`validate_path`), the six tools | which tools a session gets |
| `agent/` | `AgentActor` (transcript, pending `ask`, optional role prompt), `run_agent` with one `AgentEvent` observer, request building | events, persistence |
| `bus/` | ordering: the sequencer gives every event its `seq`; envelopes and the event catalog (`event.rs`); commands and inboxes; the runtime task that drives `run_agent` and publishes events | HTTP, storage |
| `session/` | the `Service` (registry and router, `service.rs`), teardown, tombstones and their FIFO (`lifecycle.rs`), actor construction (`actor.rs`), the SQLite history (`store.rs`) | HTTP, SSE |
| `server/` | HTTP routes and SSE with `Last-Event-ID` resume; `AppState` wraps the `Service`. `server::session` and `server::store` are temporary re-exports | session state, the model layers |
| `navigator/` | everything the driver perceives as the navigator, below | session internals |

### Inside `navigator/`

| File | Responsibility |
|---|---|
| `lsp/` | the `LanguageServer` adapter and custom `navigator/*` methods (`mod.rs`), wire types (`protocol.rs`), URI to path (`uri.rs`). Thin: it delegates to `runtime`. |
| `runtime/` | the shared state behind the LSP: idle timers, the conversation-turn listener, trigger evaluation (`trigger.rs`), remark and wake delivery (`delivery.rs`), reflex-facing calls (`inline.rs`) |
| `document.rs` | one `OpenDocument` per uri (text, baseline, version, diagnostics, edit history) and the `snapshot` a trigger reads |
| `reflex.rs` | inline suggestions: position conversion, prefix and suffix caps, debounce, staleness, and the reflex-context cache |
| `metacognition.rs` | the one `Metacognition` implementation: remark prompt, read/grep tool loop with its budget, reflex-context producer |
| `pipeline/` | the decision pipeline: TOML model (`mod.rs`), load-time validation and compiled conditions (`validator.rs`), snapshot and file resolution (`snapshot.rs`), `{{var}}` templates (`templates.rs`), evaluation and branch selection (`evaluator.rs`) |
| `replay.rs` | frozen-trigger replay of the log against a candidate pipeline; it reuses the pipeline's prefilters and branch selection |
| `log.rs` | the JSONL decision log: record types, `PendingTrigger::finish` (the only writer of trigger records) |
| `conversation.rs` | `ConversationFeed`: the active conversation, its digest, the ask guard, wakes, driver-turn events |
| `intent.rs` | `.navigator/intent.md`: read, section-wise atomic write, and the cognition's `intent` tool |
| `ports.rs` | the model seams (`FimModel`, `DecisionModel`, `Metacognition`, `ReflexContextModel`) and `ModelError` |
| `models.rs` | adapters from the provider clients to those seams |
| `bootstrap.rs` | process wiring: builds each layer from its alias, starts the in-process session server |
| `default_pipeline/` | the built-in pipeline TOML and prompts; a project's `.navigator/` overrides them |

## Data flows

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
SSE. A navigator session also feeds the `ConversationFeed`: its digest reaches the fast layers'
prompts, and every driver turn end triggers a `conversation` evaluation on the focused document.

## State on disk

| Path | What | Written by |
|---|---|---|
| `~/.config/rust-agent/models.json`, `auth.json` | aliases and keys | the driver, `rust-agent auth` |
| `<workspace>/.navigator/pipeline.toml`, `prompts/*.md` | the project's pipeline | the driver |
| `<workspace>/.navigator/intent.md` | intent, plan, decisions | the cognition and the driver |
| `~/.local/share/rust-agent/navigator.jsonl` | decision log, replay input | `navigator/log.rs` |
| `~/.local/share/rust-agent/navigator/sessions.db` | conversations | `session/store.rs` |

## Where new code goes

- A helper you need probably exists. Look first in: `provider/client.rs` (any HTTP call to a model),
  `provider/config.rs` (`load_models`), `navigator/log.rs` (`excerpt`, `new_ulid_string`),
  `bus/event.rs` (`now_rfc3339`), `navigator/lsp/uri.rs`, `navigator/document.rs` (diffs and
  numbered excerpts), `tools/mod.rs` (`validate_path`).
- A new model call goes through `ModelClient`. A new navigator layer seam goes in `ports.rs`, with
  its adapter in `models.rs` and its fake in `tests/common/navigator.rs`.
- A new trigger record field goes in `PendingTrigger`; nothing else writes trigger records.
- Behaviour that should be tunable goes in the pipeline (TOML and prompts), not in Rust (ADR 0005).
- Tests use `tests/common/`: `server.rs` (configs, servers, sessions, messages), `sse.rs` (scripted
  chat streams), `navigator.rs` (the in-memory LSP client and one scripted fake per model seam). Do
  not copy a harness into a new test file.
- Keep a production file under ~600 lines. Past that, split by responsibility, not by size.
- Comments name the domain reason and dated evidence; never a ticket or plan id.

## Known debt

Tracked in [`.scratch/refactor/plan.md`](.scratch/refactor/plan.md):

- `session/` imports navigator types (feed registration, wake poster, intent tool): the
  host binary should compose them. Planned with a `session::Service` that hides the sequencer.
- The same events live in the bus ring buffer and in SQLite; tombstones duplicate history. Planned:
  SQLite as the single history, after its writes are made trustworthy.
- `navigator/replay.rs` (about 860 lines) and `navigator/metacognition.rs` (about 650) exceed the
  size rule.
- GET and SSE consult only the in-memory registry: a conversation that is resumable from SQLite but
  has no tombstone in this process answers 404 until it is resumed.
