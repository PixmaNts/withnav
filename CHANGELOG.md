# Changelog

All notable changes to this project. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, a minor version may break compatibility. New entries go under `Unreleased` as merge
requests land in `develop`.

## [Unreleased]

### Added

- **Model catalog**: a chat alias without `context_window` in models.json gets its window from the
  provider's `GET /models` list (`context_length`, else `max_input_tokens`), cached for a day under the
  data dir. An explicit `context_window` wins without any network call; if it differs from what the
  provider reports, the log says so.
- **Project and user rules**: a session reads, once when it is created or resumed, `~/.agents/AGENTS.md`
  (else `~/.claude/CLAUDE.md`), `~/.agents/rules.md`, then in each directory from the Git worktree
  root down to the workspace `AGENTS.md` (else `CLAUDE.md`) and `rules.md`, and sends them to the
  model as one system message after the role prompt. Files over 16 KiB, or beyond 64 KiB in all, are
  cut with a visible marker; the log tells what was loaded, shadowed, truncated or unreadable. Edits
  take effect in the next session or resume.
- **Skills**: a session finds, once when it is created or resumed, the `SKILL.md` files under
  `~/.agents/skills`, `~/.claude/skills` and each project's `.agents/skills`, `.claude/skills` and
  `.opencode/skills` (nested folders included), lists them to the model in a catalog after the rules,
  and lets it load one with the `skill` tool. `read` and `grep` may open the files of a skill; nothing
  can write there. A skill with `disable-model-invocation: true` stays out of the catalog; a broken
  file is logged and the others still load.
- **Terminal client**: `navai tui` starts a private server (or connects to one with `--url`,
  token in `NAVAI_SERVER_TOKEN`) and opens a chat in the terminal.
- Streaming chat with reasoning, tool calls with their duration, and the model with the context
  and session token counts in the status line.
- A model picker (the choice starts a new conversation), a session list to resume a past
  conversation, stop, a command palette and a help screen.
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
