# Changelog

All notable changes to this project. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, a minor version may break compatibility. New entries go under `Unreleased` as merge
requests land in `develop`.

## [Unreleased]

### Added

- **Context budget**: every session keeps an estimate of its next request (UTF-8 bytes / 4 plus a
  fixed framing per message), split into system, rules, skills catalog, tools and messages, against
  its model's window. The input budget is `min(W − R, input limit or W) − M`: R is the alias's
  `max_output_tokens`, else the smallest of 8192, a quarter of the window and the output ceiling the
  provider reports; M is 5% of the window, at least 4096. A window too small to leave any input
  budget is refused when the session is created (422, `impossible_context_budget`), naming
  models.json. `GET /v1/sessions/{id}/context` returns the snapshot (alias, variant, window and its
  source, estimate and breakdown, output reserve, margin, input budget, percent, the provider's count
  for the last request with its step, a revision), `/context` renders it (`~42k / 1,000k · 4% · input
  budget 942k` and the breakdown), and a new `context.updated` event carries it after each provider
  response, each step's tool results and at turn start and end. The event is live-only: it is not
  stored or replayed, so the catalog has 14 kinds; the envelope version, the other kinds and the
  stored history are unchanged. The TUI status line shows the estimate (`ctx ~42k / 1M · 4%`)
  instead of the last prompt count, and keeps the old display against a server that has no endpoint.

- **Session commands**: `GET /v1/commands` lists them; `POST /v1/sessions/{id}/commands` with
  `{name, arguments}` runs one and answers `{operation_id, ok, data?, error?}`. Built in: `/help`,
  `/model` (alias, window and where the window comes from), `/new [alias]` (a fresh session; the old one
  stays) and `/stop`; `/variant` and `/compact` are listed and answer "not available yet" (`/context` is built, below).
  An unknown name is an error that lists the close names, and a command sent while the agent waits for
  an answer to its `ask` is refused, never queued. Each run publishes a durable `command.result` event
  (`operation_id`, `name`, `ok`, `data` of at most 16 KiB, `error`), so the event catalog has 13 kinds;
  the envelope version and the other kinds are unchanged. `/messages` is unchanged.
- **`navai run --prompt "/help"`** runs the command and prints its result; `//text` sends the literal
  message `/text`.
- **`glob` tool**: finds files by a relative pattern (`**/*.rs`) under the workspace or a readable
  `path`, as sorted relative paths. Links are not followed; hidden entries, `node_modules` and `target`
  are skipped unless the pattern names them. At most 200 paths by default (1,000 max), 100,000 entries
  visited and 5 s; the result says when it stopped early. Absolute patterns and `..` are refused.
- **`fetch` tool**: an HTTP(S) GET returned as markdown (default), plain text or raw HTML. Own client with
  a `navai/<version>` User-Agent and no provider credentials; URLs with credentials are refused. Only
  same-origin redirects are followed (at most 10): a redirect to another origin comes back as the target
  URL for a new call. The body is streamed and capped at 5 MiB after decompression, text types only
  (images and binaries are refused), decoded by its charset, output bounded to 64 KiB. HTML is converted in a child process limited to 512 MiB of memory, 10 s of CPU and the call's time budget; if it fails the page comes back as plain text with a note. Errors carry a
  code: `invalid_url`, `timeout`, `http_status`, `unsupported_content`, `too_large`, `network`. Any
  address is reachable, localhost and private networks included.
- **Model catalog**: a chat alias without `context_window` in models.json gets its window from the
  provider's `GET /models` list (`context_length`, else `max_input_tokens`), cached for a day under the
  data dir. An explicit `context_window` wins without any network call; if it differs from what the
  provider reports, the log says so. `GET /v1/models` lists each alias with the resolved window,
  `enabled`, and `disabled_reason` when there is none.
- **Model list fetch is bounded**: one request at a time per provider (concurrent sessions share it), a
  3 s deadline, at most 2 MB and 10 000 entries. A redirect to another origin is not followed, so the API
  key and headers never leave the provider. The cached list is refreshed after 24 hours; if that fails,
  the last known values stay in use, marked stale with their age and the error. A limit the provider
  reports as null or malformed never replaces a good one. A corrupt cache file is ignored and rewritten;
  a data directory that cannot be written keeps the list in memory with one warning. The cache file holds
  limits only, never a key or a header.
- **`GET /v1/models` shows where a window came from**: each alias adds, when known, `window_source`
  (`models.json` or `discovered.*`), `stale`, `stale_age_secs`, `refresh_error`, `max_input_tokens`,
  `reported_output`, and `discrepancy` (`configured` and `discovered`) when a `context_window` override
  differs from what the provider reports. Old fields are unchanged and the TUI still reads older servers.
- **Metacognition window check**: the editor's remarks model is resolved like a chat session before
  its first call. If its alias has no known context window, only the remarks are turned off, with one
  warning and a line in `navigator/status`; completion, the instinct, history and the chat server keep
  working.
- **A conversation keeps its model's limits**: a session saves, when it is created, which provider,
  model and API it runs on and its context window and other limits (`sessions.model_resolution`, with
  where each value came from and no credentials). A resume uses the saved limits, so it works with the
  provider down, and a model list refresh never changes a conversation in progress. A conversation
  from before this change gets them the first time it is resumed. If the alias now points at another
  provider, model or API, the resume stops with an error naming both, instead of continuing on another
  model; the server answers `409` with `"reason": "model_identity_changed"`. A saved value that cannot
  be read stops the resume with a clear error.
- **Project and user rules**: a session reads, once when it is created or resumed, `~/.agents/AGENTS.md`
  (else `~/.claude/CLAUDE.md`), `~/.agents/rules.md`, then in each directory from the Git worktree
  root down to the workspace `AGENTS.md` (else `CLAUDE.md`) and `rules.md`, and sends them to the
  model as one system message after the role prompt. Files over 16 KiB, or beyond 64 KiB in all, are
  cut with a visible marker; the log tells what was loaded, shadowed, truncated or unreadable. Edits
  take effect in the next session or resume.
- **Skills**: a session finds, once when it is created or resumed, the `SKILL.md` files under
  `~/.agents/skills`, `~/.claude/skills` and each project's `.agents/skills`, `.claude/skills` and
  `.opencode/skills` (nested folders included), lists them to the model in a catalog after the rules,
  and lets it load one with the `skill` tool. `read` and `grep` may open the files of a skill, even
  outside the workspace. This adds no write permission: `write` and `edit` keep their roots, a
  project skill inside the workspace stays writable as any workspace file, and `bash` is not
  sandboxed. A skill with `disable-model-invocation: true` stays out of the catalog; a broken
  file is logged and the others still load.
- **Terminal client**: `navai tui` starts a private server (or connects to one with `--url`,
  token in `NAVAI_SERVER_TOKEN`) and opens a chat in the terminal.
- Streaming chat with reasoning, tool calls with their duration, and the model with the context
  and session token counts in the status line.
- A model picker (the choice starts a new conversation), a session list to resume a past
  conversation, stop, a command palette and a help screen.
- **Terminal themes**: Ayu Dark (the default), Ayu Light, Nord Dark and Nord Light. `/theme` in the
  command palette opens a picker that shows each theme as you move through it (`esc` keeps the
  current one); the choice is remembered in `~/.config/navai/config.json` under `tui.theme`.
- `GET /v1/models`: the chat models with tools a new session can use, with their context window.
- **VS Code chat**: a model picker for the cognition (it sets the model of a new conversation; an
  open conversation keeps its own) and a gauge of the context used out of the model's window.
- `duration_ms` in the metadata of `tool.post_call`.

### Changed

- **A chat session needs a known context window.** Creating one for an alias that has none in
  models.json and none reported by the provider is refused (HTTP 422, `reason: "no_context_window"`;
  `navai run` prints it as its `error:` line and exits 1). Set `context_window` for the alias.
- **The project is now called Navai.** The crate and binaries are `navai` and `navai-lsp` (they were
  `rust-agent` and `rust-agent-lsp`). The VS Code extension is `pixmants.navai` "Navai" (the chat
  view is still "Cognition"); its commands are `navai.focusChat`, `navai.markUseful`,
  `navai.markNoise`, `navai.dismiss` and `navai.setIntent`, and `navai.serverPath` defaults to
  `navai-lsp`. Uninstall the old `rust-agent.navigator` extension and the old binaries by hand.
- **Data and config moved**: data to `~/.local/share/navai/` (`navai.db`, `navai.jsonl`,
  `tui.log`), config to `~/.config/navai/` (`auth.json`, `models.json`). The move is automatic on
  the first start of `navai` or `navai-lsp`, from `~/.local/share/navigator/`,
  `~/.local/share/rust-agent/` and `~/.config/rust-agent/`. It never overwrites a file, and a
  database in use by another process stays where it is until a later start.
- Environment variables are `NAVAI_*`. `RUST_AGENT_SERVER_TOKEN` still works as a fallback for
  `NAVAI_SERVER_TOKEN`, with a deprecation warning. The other `RUST_AGENT_*` variables (internal
  tuning and test switches) are renamed without a fallback.
- Extension settings are `navai.*`; a value still set under the old `navigator.*` key is read as a
  fallback until a `navai.*` one is set.

### Fixed

- The navigator no longer holds back its advice while your code is still unfinished: a remark worth
  making is shown even when the edit is mid-way, and a one-line fix counts as a change worth
  looking at. A remark is still shown at most once every 45 seconds, and a stuck or drifting file
  still wakes the chat on its own schedule.
- Saving and then pausing on the same text no longer restarts the remark being written; newer
  text still replaces it and closing the file still cancels it.
- The navigator's view of your recent edits lists each real edit once, with its file and version,
  instead of repeating the same text for every save and pause, and says that it does not know
  whether an error lasted across edits.
- A navigator remark is written faster: it reads at most one extra file before answering (a
  request for help still gets three), and the code it is about is sent once instead of twice.
- The chat assistant answers a guidance question from the relevant code only, without running
  project-wide checks or reopening unrelated work, and treats every navigator wake, including
  "you look stuck", as something to check rather than an order to implement.
- A comment that calls on the navigator is answered once: saving or pausing again with the same
  comment no longer cancels the answer being written, and the same comment does not ask again until
  it is removed and written anew. Such a call also gets one more round of reading files.
- A model that tries to call a tool in its final answer is asked again instead of giving you
  nothing.
- A remark is no longer thrown away because you kept typing: it arrives when the model answers, on
  the line it was about (moved along with your edits), and is dropped only if that line was
  rewritten or the file closed.
- The remark time limit goes from 8 s to 20 s, so slow answers arrive instead of vanishing.
- The navigator wakes the chat when you look stuck, whether the code compiles or not, and no longer
  wakes it or remarks on an edit you are still in the middle of typing; it decides both by judging
  the edit, not by whether it compiles.
- A wake no longer silences the next remark; wakes have their own 120 s pause.
- A remark routed to the cognition but below the wake thresholds is now written instead of dropped.
- A wake message is short and renders properly: a one-line reason with the file and line, the diff
  in a code block (40 lines at most), the intent only for drift, no repeated conversation.
- A comment that calls on the navigator ("help", "aide", "navigator", or a question) now gets an
  answer even when it is a single added line.
- The short context note for ghost-text suggestions waits up to 25 s to refresh, and an empty
  refresh answer keeps the previous note without an error in the log each time.

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
