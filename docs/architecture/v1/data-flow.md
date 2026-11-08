# Data flow

## 1. Captain message → LLM → client

Every box is a crate; every arrow crosses a typed boundary. The stage chains (`⟦…⟧`) are described in
[pipeline.md](pipeline.md).

```mermaid
sequenceDiagram
  autonumber
  participant C as Client (TUI/Web/…)
  participant S as server
  participant R as runtime (session + sequencer)
  participant E as engine (turn loop)
  participant P as ModelProvider (port)
  participant T as Tool (port)
  participant St as Store (port)

  C->>S: POST /v1/sessions/{id}/messages · api::v1::SubmitMessage
  S->>R: runtime::Command::Message (authenticated origin = Captain)
  R->>St: commit(message.received) → seq
  R-->>C: SSE EventEnvelope{seq, kind, payload}
  R->>E: run_turn(TurnContext)
  E->>E: ⟦turn.input⟧ skills, instructions, steering
  loop each step
    E->>E: ⟦model.request⟧ then host budget + compaction
    E->>P: chat(ChatRequest, OperationContext)
    P-->>E: Stream<ModelEvent> (Text, Thinking, ToolCall*, Usage, Done)
    E-->>R: TurnEvent::ModelDelta (observed by ⟦model.stream⟧)
    R-->>C: SSE llm.delta
    E->>E: ⟦model.response⟧
    opt tool calls (serial, in order)
      E->>E: ⟦tool.prepare⟧ → ⟦tool.authorize⟧
      alt Run(ExecutionPermit)
        E->>T: execute(ToolArguments, ToolContext)
        T-->>E: ToolResult
        E->>E: ⟦tool.result⟧
      else Ask(PermissionRequest)
        E-->>R: Yield(PendingInteraction)
        R->>St: commit(permission.requested)
        R-->>C: SSE permission.requested
        C->>S: POST …/permissions/{rid} · Once / Always / Reject
        S->>R: PermissionAnswer → resume the held call
      else Deny
        E->>E: settle as permission_denied
      end
      R-->>C: SSE tool.pre_call / tool.post_call
    end
  end
  R->>St: commit(transcript + turn.end) (one transaction)
  R-->>C: SSE turn.end
  R->>R: ⟦event.out⟧ observers: telemetry, plugins (read-only)
```

Invariants carried over from today:
- the runtime's sequencer is the **only** `seq` allocator; durable events are committed before they are sent;
- a client reconnects with `Last-Event-ID` and gets replay then live;
- replay rebuilds projections; it **never** re-runs tools or hooks;
- a Captain question is a yielded `PendingInteraction`, never an async stage waiting on a UI.

## 2. Three kinds of "signal", three semantics

```mermaid
flowchart LR
  subgraph op["inside an operation"]
    H["Stages / hooks<br/>may transform or halt<br/>(typed patch)"]
  end
  subgraph fact["after commit"]
    EV["Domain events<br/>EventEnvelope, seq, durable or live<br/>→ clients, plugins (read-only)"]
  end
  subgraph diag["diagnostics"]
    TEL["Telemetry<br/>tracing spans + StageObservation<br/>→ log / OTLP, best effort"]
  end
  H -- "outcome" --> EV
  H -- "evidence" --> TEL
  EV -- "correlation ids" --> TEL
```

| | Stage / hook | Domain event | Telemetry |
| --- | --- | --- | --- |
| When | during the operation | after commit | any time |
| Can change things | yes, typed patch / halt | no | no |
| Ordered, replayable | per chain | yes (`seq`) | no |
| Failure | per-chain policy | — | dropped, never blocks |
| Audit records kept as-is | — | — | `permission_decisions`, `flow_trace`, navigator JSONL (product evidence, not sampled logs) |

Telemetry: `tracing` spans `turn → model_call → tool_call → stage`, ids = session/turn/call. OpenTelemetry
exporter is an optional subscriber in `nav`; the engine has no exporter dependency.

## 3. Navigator (editor) path

```mermaid
flowchart LR
  ED[VS Code] -- "LSP didChange / didSave / inlineCompletion" --> NV[navigator]
  NV -- "FimProvider" --> FIM[(FIM model)]
  NV -- "flows: editor triggers" --> FL[flows] -- "DecisionProvider" --> S1[(System One)]
  NV -- "ConversationPort (implemented by runtime)" --> RT[runtime]
  RT -- "turn events" --> NV
```

The navigator never imports the runtime: it receives a `ConversationPort` built in `nav`.
