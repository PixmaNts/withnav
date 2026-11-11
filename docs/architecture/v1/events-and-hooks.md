# Events and hooks

"Today" = source read on 2026-10-17. Crate names omit `withnav-`.

## 1. Event catalog

25 kinds (`src/bus/event.rs:145`), 21 durable (D), 4 live-only (L). Every envelope carries
`payload: Value` today, even when built from a struct. v1 gives each kind a typed payload, **same JSON**.

Typing today: `T` typed struct · `V` ad hoc JSON · `T+V` struct extended with JSON.
Every row is emitted by `runtime` (the only `seq` allocator), some on behalf of `engine` (`R←E`).

| Kind | D/L | From | Today | v1 payload |
| --- | --- | --- | --- | --- |
| `session.start` | D | R | T | `SessionStart {workspace, root_actor_id}` |
| `session.end` | D | R | V | `SessionEnd {reason, final_seq}` |
| `session.bypass` | D | R | V | `SessionBypass {enabled, by}` |
| `session.agent_changed` | D | R | T/V | `SessionAgentChanged {agent, previous}` (nullable today, String in the struct) |
| `message.received` | D | R | T/V | `MessageReceived {command_id, source, target, mode, content, answer?, selected_skill?, delivery?}` |
| `turn.start` | D | R | T | `TurnStart {turn_id, actor_id, task_id}` |
| `turn.end` | D | R←E | T | `TurnEnd {turn_id, outcome, usage?, duration_ms, error?}` |
| `llm.delta` | L | R←E | T | `LlmDelta {turn_id, step_id, delta: ModelEvent}` |
| `llm.response` | D | R←E | T | `LlmResponse {turn_id, step_id, message, usage?, finish}` |
| `tool.pre_call` | D | R←E | T+V | `ToolPreCall {tool_call_id, name, source, args, permission_gate?: PermissionEvidence}` |
| `tool.post_call` | D | R←E | T | `ToolPostCall {tool_call_id, name, status, output, host: HostEvidence, meta: ToolMeta}` |
| `agent.state` | D | R | T | `AgentStateChanged {actor_id, from, to, reason?}` |
| `agent.yield` | D | R←E | T | `AgentYield` enum: `Ask \| Budget \| Watchdog{pause_token, evidence}` (wire `kind/details` kept) |
| `task.done` | D | R | T | `TaskDone {task_id, result_message_id}` |
| `command.result` | D | R | T/V | `CommandResult {operation_id, name, ok, data: CommandData, error?}` (typed per command, raw fallback) |
| `context.updated` | L | R←E | T | `ContextSnapshot {model, window, breakdown, input_budget, percent, …}` |
| `context.compacting` | L | R←E | V | `ContextCompacting {pass, total}` |
| `context.compacted` | D | R←E | T/V | `ContextCompacted {generation, through_message, estimate_before/after, written_by?, passes}` |
| `permission.requested` | D | R←E | T+V | `PermissionRequested {request_id, tool_call_id, tool, arguments, scope, reason, always_label?, requires_captain, child_session_id?, permission_gate?}` |
| `permission.resolved` | D | R | T+V | `PermissionResolved {request_id, resolution: Once\|Always{saved}\|Reject{message}, approved_by?}` |
| `model.changed` | D | R | T+V | `ModelChanged {operation_id?, revision, reason, previous?, new: Selection, window, input_budget}` |
| `subagent.spawned` | D | R | T+V | `SubagentSpawned {parent, child, agent, goal, tool_call_id, delivery}` |
| `subagent.done` | D | R | T+V | `SubagentDone {parent, child, agent, outcome, reason?, notification_id, delivery}` |
| `subagent.watchdog` | D | R | V | `SubagentWatchdog {notice_kind, child, verdict, reason, checked_at, evidence}` (richer than today's struct) |
| `mcp.status` | L | R←mcp | T | `McpStatus {incarnation, revision, servers}` |

Decisions:
- `tool.pre_call` the **event** ≠ `tool.pre_call` the **flow trigger** (same name today, different
  moments). The trigger becomes `tool.authorize.flows`; the event keeps its wire name.
- No merge of `task.done` / `turn.end` (task finished ≠ one turn ended), nor `model.changed` /
  `session.agent_changed` on the wire (the latter stays as a compatibility notice).
- `permission_gate` evidence is versioned nested JSON today (`agent/authorize.rs:579`): type it
  without flattening it.
- No generic "plugin event" kind: plugins observe, they do not emit domain events.

## 2. Ordering guarantee (one change)

Today `PersistingEmitter` enqueues the storage write **then** emits (`session/store.rs:2074`).
v1: a durable event is visible to clients and observers **only after its commit is acknowledged**
(the acknowledged path already exists, `bus/sequencer.rs:451`). Live events are sequenced, not stored.

## 3. Today's interception points → v1

| Today | v1 |
| --- | --- |
| `ToolCallFlows::permission_pre_call` + protected check (`agent/flow_hook.rs:130`, `flows/runtime.rs:2059`) | `tool.authorize.flows` (built-in only); protected check stays a host step |
| Message-trigger flows before admission (`session/message.rs:76`, `session/flows.rs:84`) | host admission, timing unchanged; only context assembly moves to `turn.input.context` |
| Turn-end flow listener (`session/flows.rs:15`) | host lifecycle listener; effects are typed proposals, not an `event.out` side effect |
| Safe boundary: steering, child delivery, watchdog (`agent/boundary.rs:28`, `bus/runtime.rs:947`) | fixed runtime boundary before `model.request`, not a stage |
| Navigator actor customization (`navigator/session_hooks.rs:121`) | built-in stage in `model.request.context` |
| `NavigatorHooks::started` + `ConversationFeed` (`navigator/session_hooks.rs:101`, `navigator/conversation.rs:258`) | built-in `event.out` observer + `ConversationPort` |
| Editor triggers idle / save / diagnostics (`navigator/runtime/mod.rs:227`) | stay navigator-owned (debounce, freshness, feedback loops) |
| Compaction gate (`bus/runtime/compact.rs:490`) | host step after `model.request`; progress observable |
| Watchdogs (`session/subagents/supervisor.rs:68`, `root_watchdog.rs:255`) | runtime-owned; judgments through `DecisionProvider` |
| Captain skill admission (`session/service.rs:1392`) | `turn.input.context` may *propose* a skill; host validates the hash and policy |
| MCP status bridge (`session/mcp.rs:61`) | runtime-owned; observable |

Pattern: what **decides** or **orders** (admission, boundary, compaction, watchdog, settlement)
stays in the host; what **adds, rewrites or reviews** becomes a slot.

## 4. v1 hook catalog

B built-in · C compiled plugin · X external plugin.

| Slot | Input → patch | May | On failure | Who |
| --- | --- | --- | --- | --- |
| `turn.input.context` | `TurnInput → InputPatch` | append attributed context, propose a skill | abort input, command kept | B C X |
| `model.request.context` | `RequestDraft → RequestPatch` | add context, narrow tools, bounded options | no provider call | B C X |
| `model.stream.observe` | `&ModelEvent` | observe | dropped, never blocks | B C X |
| `model.response.plan` | `AssistantTurn → ResponsePatch` | annotate, drop/rename proposed calls (settled explicitly) | no dispatch | B C X |
| `tool.prepare.rewrite` | `ToolDraft → ToolPatch` | rewrite name/args; id immutable; preflight re-run | call settled as failed | B C X |
| `tool.authorize.flows` | `PermissionView → FlowAnswer` | allow / ask / deny / apply rule (ADR 0015) | apply the rule | B only |
| `tool.authorize.review` | `PermissionView → Restriction` | no opinion / ask / deny, **never allow** | ask the Captain | B C X |
| `tool.result.present` | `ToolResultView → ResultPatch` | redact, truncate, summarise what the model sees | output withheld, never re-executed | B C X |
| `event.out.observe` | `&EventEnvelope` | observe committed facts | dropped / observer detached | B C X |

Cancellation is never turned into a fallback. Yolo skips both `tool.authorize.*` slots.

## 5. Schema evolution

Wire envelope keys stay: `schema_version, event_id, seq, session_id, actor_id, kind, occurred_at,
causation_id, correlation_id, payload`.

| Change | Rule |
| --- | --- |
| add a kind | payload type + D/L class + fixture + generated TS; unknown kinds never trigger actions |
| add a field | optional or defaulted (old stored events); unknown fields ignored |
| rename | old spelling kept in `/v1` + compatibility decoder; a real rename is `/v2` |
| remove | stop reading first; keep the replay decoder; never reuse a name |
| change meaning/type | breaking: new version, even if the JSON still parses |

Clients keep unknown kinds **with** their raw payload (not a unit `#[serde(other)]`), advance the
cursor past them, and never infer an approval from them.
