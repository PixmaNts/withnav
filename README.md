# rust-agent

A personal, minimal-but-complete coding agent in Rust — inspired by `pi` / `oh-my-pi`,
but not constrained by compatibility with any of them. It does **not** fight its operator
(no forced todo-completion), owns its core, and stays understandable.

> **Status:** design phase. See [`docs/SPEC.md`](docs/SPEC.md) for the full v0.1 specification.

## The idea in one paragraph

An agent is a ~200-line loop: send transcript + tools to an LLM, execute the tools it
asks for, feed results back, and either **Continue**, **Done**, or **Yield** (pause for a
human). Everything else — plugins, subagents, a meta-loop co-agent, persistence, MCP tools,
memory (contextd) — is a facet of **one** unifying primitive: an **event bus + multiprocess
actor model**. That single mechanism is what makes an ambitious feature set feasible as a
handful of focused vertical phases.

## Core decisions

- **Rust core, headless, mono-binary.** Server/multi-client: the core exposes an event API
  (SSE via axum). TUI / WebUI / App are all just clients. Write the brain once, many faces.
- **5 native tools only** (`read`, `write`, `edit`, `bash`, `grep`). Everything else via **MCP**.
- **OpenAI-compatible providers only** → the local **OmniRoute** proxy works directly.
- **Plugins = external processes** speaking JSON-RPC over stdio (Python/TS/bash) — never an
  embedded interpreter. Free isolation, poly-language, core stays pure Rust.
- **Mini hook protocol** (`continue` / `modify` / `deny`) over lifecycle events = the plugin
  system, prompt injection, and security gates unified.
- **Non-blocking subagents.** `spawn_agent` returns immediately; results arrive as a
  `subagent.done` event. Never block on the first of N. Never deadlock the conversation.
- **Delivery modes** per message (`next-turn` / `steer` / `interrupt`) for subagents *and* the user.
- **Meta-loop / co-agent** = a light plugin on a cheap model that observes the bus from
  *outside* the main loop → summaries, goal reminders, anti-todo-hacking guard. Makes
  features like "goal" *emerge* instead of being hard-coded.
- **Event-sourced SQLite persistence** = a bus subscriber. Replaying events = resume.
- **Self-awareness (levels 1+2)** comes free: everything is files + hot-reload; the agent
  edits its own config / `agents/*.md` / plugins and the system reloads live.

## Repo layout

```
docs/SPEC.md        full v0.1 specification (authoritative)
refs/               cloned reference projects (inspiration, gitignored)
  pi_agent_rust/    Dicklesworthstone — Rust port of Pi
  rig/              0xPlaygrounds — Rust LLM framework (used by contextd)
```

## Vertical-phase MVP roadmap

- **Phase 1** — single-agent vertical slice: loop + 5 tools + OmniRoute + streaming SSE.
- **Phase 2** — event bus + plugin JSON-RPC protocol + MCP (rmcp) + hooks + server API.
- **Phase 3** — non-blocking subagents + event-sourced SQLite persistence.
- **Phase 4** — co-agent/meta-loop + AGENTS.md/persona loading + Ratatui client.

Strict scope fence: **core + event bus + MCP + server, nothing else in v1.** Every feature
must be expressible through the four foundational primitives (loop, bus, plugin boundary,
server API) or the contract is improved — never a feature-specific subsystem.

## License

TBD.
