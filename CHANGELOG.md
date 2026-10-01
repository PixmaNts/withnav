# Changelog

All notable changes to this project. Versions follow [Semantic Versioning](https://semver.org/);
before 1.0, a minor version may break compatibility. New entries go under `Unreleased` as merge
requests land in `develop`.

## [Unreleased]

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
