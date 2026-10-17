# WithNav v1 — target architecture

From one 107k-line crate to a Cargo workspace whose boundaries the compiler enforces.
Still **one process, one binary** (`nav`): modular, not distributed.

| Doc | Content |
| --- | --- |
| this file | layers, crates, dependency rule |
| [data-flow.md](data-flow.md) | a Captain message from client to LLM and back, events |
| [pipeline.md](pipeline.md) | the stackable processing chains (pre-call, permission, flow, result…) |
| [types.md](types.md) | ports (traits) and core data structures |
| [providers.md](providers.md) | the four wire families (Chat Completions, Responses, Anthropic, Gemini) behind one port |
| [events-and-hooks.md](events-and-hooks.md) | the 25 events typed, today's interception points, the v1 hook catalog, schema evolution |
| [plugins-and-telemetry.md](plugins-and-telemetry.md) | plugin manifest, lifecycle, stdio protocol; spans, metrics, logs, audit |
| [migration.md](migration.md) | shippable steps, risks, open questions |

Current state for comparison: [`../doc-pré-v1/`](../doc-pré-v1/README.md).

## 1. Layers

```mermaid
flowchart TB
  subgraph clients["Clients (any language, one API)"]
    direction LR
    TUI[TUI] ~~~ WEB[Web] ~~~ VSC[VS Code] ~~~ DESK[Desktop*] ~~~ MOB[Mobile*]
  end

  subgraph edge["Edge adapters"]
    direction LR
    SRV["withnav-server<br/>HTTP + SSE"] ~~~ LSP["withnav-navigator<br/>LSP, reflex, metacognition"]
  end

  subgraph app["Application"]
    RT["withnav-runtime<br/>sessions, sequencer, children"]
    ENG["withnav-engine<br/>turn loop, prompt, compaction,<br/>permission policy, stage runner"]
    FL["withnav-flows<br/>instinct flows"]
  end

  CORE["withnav-core<br/>domain types + ports (traits)"]

  subgraph adapters["Driven adapters (implement core ports)"]
    direction LR
    PRV["withnav-providers<br/>openai · anthropic · …"] ~~~ TL["withnav-tools<br/>native tools"] ~~~ MCP["withnav-mcp"] ~~~ STO["withnav-storage<br/>SQLite, JSONL"] ~~~ PLG["plugins<br/>(features / stdio)"]
  end

  clients -- "withnav-api v1 (typed DTOs)" --> SRV
  VSC -. LSP .-> LSP
  SRV --> RT --> ENG
  LSP --> FL
  ENG --> CORE
  FL --> CORE
  adapters -- implement --> CORE
```

`*` illustrative. `nav` (composition root) is the **only** crate that sees everything: it builds the
adapters and injects them into the runtime.

## 2. Crates and dependency rule

Arrows = `use` (compile-time). Runtime calls go the other way through ports.

```mermaid
flowchart BT
  core[withnav-core]
  api[withnav-api]
  engine[withnav-engine] --> core
  runtime[withnav-runtime] --> engine
  flows[withnav-flows] --> core
  flows --> ifl[instinct-flow]
  navigator[withnav-navigator] --> flows
  providers[withnav-providers] --> core
  tools[withnav-tools] --> core
  mcp[withnav-mcp] --> core
  storage[withnav-storage] --> core
  server[withnav-server] --> runtime
  server --> api
  client[withnav-client] --> api
  tui[withnav-tui] --> client
  nav[nav: composition root] --> server & navigator & providers & tools & mcp & storage & flows
```

Rules:
1. **Adapters never import each other** nor anything above `core`.
2. **`withnav-api` is a leaf**: wire DTOs only, versioned (`v1`), TS types generated from it.
3. **`core` holds values and traits only**: no I/O, no config loading, no orchestration.
4. A component is tested with fakes of the ports it uses (scripted provider, in-memory store,
   fake clock, recording stage). Each port has one contract test suite run on fake *and* real adapter.

| Today | Goes to |
| --- | --- |
| `provider/` | `providers` (wire), `core` (neutral types), `runtime` (assignment/selection) |
| `tools/` | `tools`; `task`, `steer_subagent`, `decide_subagent` → `runtime` |
| `mcp/` | `mcp` (returns a status snapshot; runtime publishes it) |
| `agent/` | `engine` |
| `permission/` | `engine` (policy), `storage` (tables) |
| `bus/`, `session/` | `runtime` (logic), `storage` (`store.rs`) |
| `flows/` | `flows` |
| `navigator/` | `navigator`; bootstrap → `nav` |
| `server/` | `server`; DTOs → `api` |
| `tui/` | `tui` + `client` (HTTP/SSE transport shared with future Rust clients) |
| `bus/event.rs` wire types | `api` |

How today's cycles break:

| Cycle | Fix |
| --- | --- |
| `provider → session/settings` | resolved config is injected; providers never read settings |
| `tools ⇄ agent/session` | session-control tools live in `runtime`, implement `core::Tool` |
| `permission ⇄ agent/bus/flows` | evidence types move to `core`; flows reached through a port |
| `flows → navigator` | flows get intent through a `WorkspacePort`, not `navigator::intent` |
| `mcp → bus` | MCP exposes a status `watch`; runtime turns it into `mcp.status` |
| `session ⇄ server` | server depends on runtime, never the reverse |
