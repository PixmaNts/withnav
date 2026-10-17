# WithNav 0.8 — current state

Diagrams of the code as it stands (0.8.1, single crate `withnav` + `crates/instinct-flow`).
Details: [`ARCHITECTURE.md`](../ARCHITECTURE.md), decisions: [`docs/adr/`](../docs/adr/).

## 1. Processes and layers

```mermaid
flowchart TB
  subgraph Clients
    TUI["nav tui<br/>src/tui"]
    WEB["nav web<br/>webui/ (embedded)"]
    VSC["vscode-withnav"]
  end

  subgraph nav["nav (one binary, one crate)"]
    APP["app/ (CLI)"]
    SRV["server/<br/>HTTP + SSE"]
    NAV["navigator/<br/>LSP, reflex, instinct, metacognition"]
    SES["session/<br/>Service, store (SQLite)"]
    BUS["bus/<br/>sequencer, runtime task"]
    AGT["agent/<br/>loop, prompt, compaction, skills"]
    PERM["permission/<br/>floor, rules, decide"]
    FLOW["flows/<br/>flow runtime"]
    TOOLS["tools/"]
    MCP["mcp/"]
    PROV["provider/<br/>OpenAI-compatible only"]
  end

  IF["crates/instinct-flow"]
  LLM[("Chat / FIM / System One<br/>endpoints")]
  DB[("withnav.db<br/>withnav.jsonl")]
  MCPS[["MCP servers (stdio)"]]

  TUI & WEB -- "HTTP + SSE" --> SRV
  VSC -- "HTTP + SSE" --> SRV
  VSC -- "LSP stdio" --> NAV
  SRV --> SES --> BUS --> AGT
  AGT --> PERM --> FLOW --> IF
  AGT --> TOOLS
  TOOLS --> MCP --> MCPS
  AGT & NAV & FLOW --> PROV --> LLM
  SES & PERM --> DB
```

## 2. Real dependencies between modules (non-test `crate::` imports)

The documented rule ("a module never imports one above it") is not enforced: everything sits in one crate.

```mermaid
flowchart LR
  provider --> session & settings
  tools --> agent & session & permission & provider
  agent --> bus & permission & provider & tools
  bus --> agent & session & permission & provider & tools
  permission --> agent & bus & session & flows & provider & tools
  flows --> agent & navigator & session & permission & provider & tools
  mcp --> bus & tools & settings
  session --> agent & bus & flows & mcp & permission & server & tools
  navigator --> agent & bus & flows & server & session & permission
  server --> session & navigator & agent & bus & flows & mcp
```

Cycles: `agent⇄bus`, `agent⇄tools`, `agent⇄permission`, `permission⇄flows`, `flows⇄navigator`,
`session⇄server`, `provider→session` (for one constant). Only `workspace/` is a leaf.

## 3. A Captain message: TUI → LLM → TUI

```mermaid
sequenceDiagram
  autonumber
  participant T as TUI (reducer, client.rs)
  participant S as server/routes.rs
  participant SV as session::Service
  participant Q as bus::Sequencer
  participant R as bus::runtime task
  participant A as agent::run_agent
  participant P as provider::ModelClient
  participant L as LLM

  T->>S: POST /v1/sessions/{id}/messages (SubmitRequest)
  S->>SV: message::accept
  SV->>Q: accept_command → seq, commit
  Q-->>T: SSE message.received
  Q->>R: inbox
  R->>A: run_agent(&mut AgentActor, observe)
  loop each step
    A->>A: boundary (steering, subagent delivery)
    A->>A: context budget, compaction, build_request
    A->>P: chat(ChatRequest)
    P->>L: POST chat/completions (stream)
    L-->>P: SSE chunks
    P-->>A: StreamDelta (Text, Thinking, ToolCall*)
    A-->>R: AgentEvent
    R->>Q: publish → EventEnvelope{seq}
    Q-->>T: SSE llm.delta / llm.response
    opt tool_calls
      A->>A: tool branch (section 4)
      Q-->>T: SSE tool.pre_call / tool.post_call
    end
  end
  R->>Q: turn.end
  Q-->>T: SSE turn.end → reducer → view::draw
```

## 4. A tool call: today's decision chain

Order as implemented in `permission/decide.rs::decide_with` (it differs slightly from the prose in
`ARCHITECTURE.md`: a written deny wins before the floor, bypass skips the doom-loop question);
flows are reached via the `PermissionFlows` trait.

```mermaid
flowchart TD
  C["ToolCall {id, name, args}"] --> PF{preflight<br/>schema, role, availability}
  PF -- error --> ERR[tool error]
  PF --> Y{yolo?}
  Y -- yes --> RUN
  Y -- no --> RU{written deny?<br/>rules, most precise wins}
  RU -- deny --> DENY[permission_denied]
  RU --> FL{fixed floor<br/>credentials, unresolvable path}
  FL -- yes --> ASK
  FL --> PC{protected target?<br/>config / db / flows}
  PC -- "check failed" --> ASK
  PC --> DL{doom loop<br/>3 identical calls}
  DL -- "loop, no bypass" --> ASK
  DL --> FR{free read /<br/>temp write?}
  FR -- yes --> RUN
  FR -- no --> BP{bypass?}
  BP -- yes --> RUN
  BP -- no --> FW{permission flows<br/>instinct / System One}
  FW -- allow --> RUN
  FW -- deny --> DENY
  FW -- "apply rule: allow" --> RUN
  FW -- "ask / apply rule: ask, none" --> ASK["PermissionRequest<br/>turn yields"]
  ASK -- "Once / Always" --> RUN
  ASK -- Reject --> DENY
  RUN["Tool::call(args, cancel, ToolContext)"] --> RES["ChatMessage::Tool → transcript"]
```

## 5. Key types today

| Type | File | Fields / role |
| --- | --- | --- |
| `ChatMessage` | `provider/openai.rs` | `System`, `User{provenance}`, `Assistant{tool_calls}`, `Tool{tool_call_id, answered_by}`: both transcript **and** OpenAI wire format |
| `ToolCall` | `provider/mod.rs` | `id, name, args: Value, args_valid` |
| `StreamDelta` | `provider/mod.rs` | `Text`, `Thinking`, `ToolCallStarted`, `ToolCallArgs` |
| `Tool` | `tools/mod.rs` | `name()`, `schema() -> Value`, `call(args, cancel, &ToolContext)` |
| `permission::Input` / `Decision` | `permission/decide.rs` | `Control`, call, commands, floor, store, agent… → `Verdict{Run, Deny, AskCaptain}` + `Trace` |
| `EventEnvelope` | `bus/event.rs` | `schema, event_id, seq, session_id, actor_id, kind, at, causation, correlation, payload: Value` |
| `EventKind` | `bus/event.rs` | 25 kinds: `session.*`, `message.received`, `turn.*`, `llm.*`, `tool.*`, `permission.*`, `context.*`, `subagent.*`, `model.changed`, `mcp.status`… |
| `ModelConfig` / `ApiKind` | `provider/config.rs` | `Chat`, `Fim`, `SystemOne`: one wire family (OpenAI-compatible) |

## 6. What works, what hurts

**Works (keep):** single server API for every client · sequencer + `seq` + SSE replay · pure
`decide` over a trait · flows as data · durable child delivery · `crates/instinct-flow` extracted.

**Hurts:**
- **Cycles** (section 2): nothing is testable alone; any change recompiles everything.
- **God files**: `bus/runtime.rs` 3.6k lines, `session/store.rs`, `session/service.rs`, `agent/loop.rs` ~3.5k each.
- **One provider format**: `ChatMessage` is both the domain transcript and the OpenAI wire type.
- **Hooks are ad-hoc**: `tool.pre_call` flow trigger, `ToolCallFlows`, boundary, `ConversationFeed`,
  delivery each have their own call site. No common chain, no plugin entry point.
- **Payloads are `serde_json::Value`** (event payload, tool args/schema): contracts live in prose, not types.
- **Wire types duplicated by hand** in `tui/protocol.rs`, `webui/src`, `vscode-withnav/src`.
- **Telemetry**: ~90 `tracing` calls, a JSONL decision log, `flow_trace` in SQLite; no metrics, no spans per turn/tool.
