//! Frozen `EventEnvelope` shape (docs/PLAN.md §7.1) and the 12-kind `EventKind`
//! catalog (ticket 01).
//!
//! Reserved-but-deferred kind names (NOT in the enum, never emitted this slice):
//! `prompt.pre_llm`, `tool.progress`, `context.pre_compact` (Phase 3),
//! `subagent.spawned`, `subagent.done` (Phase 3), `hook.decision`, `plugin.error`
//! (Phase-2b plugins). Emitting them is impossible: no enum variant exists.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::provider::{AssistantMessage, StreamDelta, Usage};
use crate::tools::ToolStatus;

/// Frozen envelope schema version (PLAN §7.1: reserved = 1).
pub const SCHEMA_VERSION: u32 = 1;

/// The single root actor id for this slice (all events are published by it).
pub const ROOT_ACTOR_ID: &str = "agent:main";

/// The catalog emits EXACTLY 12 kinds (ticket 01 / Oracle O1): 2 session +
/// message + 2 turn + 2 llm + 2 tool + 2 agent + task = 12, never 13.
pub const EVENT_KIND_COUNT: usize = 12;

/// Durable kinds are retained in the reconnect ring; only `llm.delta` is
/// live-only (ticket 01 durability marking). Kept with the catalog it guards.
const fn is_durable(kind: EventKind) -> bool {
    !matches!(kind, EventKind::LlmDelta)
}

/// DRIFT GUARD (compile-authoritative): `ALL` is DERIVED from
/// [`EventKind::all_variants`], an exhaustive match whose single or-pattern
/// arms name EVERY variant with no `_` wildcard — so adding a 13th variant
/// without editing that match is error[E0004] (non-exhaustive patterns) and
/// the crate does not compile. The array's type `[EventKind; 12]` plus the
/// `const _` assert below additionally pin the frozen catalog size, so the
/// list itself cannot silently grow or shrink either. A runtime name/order
/// test still guards the wire spellings, but completeness is enforced HERE,
/// at compile time.
impl EventKind {
    const fn all_variants() -> [Self; EVENT_KIND_COUNT] {
        match Self::SessionStart {
            Self::SessionStart
            | Self::SessionEnd
            | Self::MessageReceived
            | Self::TurnStart
            | Self::TurnEnd
            | Self::LlmDelta
            | Self::LlmResponse
            | Self::ToolPreCall
            | Self::ToolPostCall
            | Self::AgentState
            | Self::AgentYield
            | Self::TaskDone => [
                Self::SessionStart,
                Self::SessionEnd,
                Self::MessageReceived,
                Self::TurnStart,
                Self::TurnEnd,
                Self::LlmDelta,
                Self::LlmResponse,
                Self::ToolPreCall,
                Self::ToolPostCall,
                Self::AgentState,
                Self::AgentYield,
                Self::TaskDone,
            ],
        }
    }

    /// The 12 emitted kinds, in catalog order. Todo 3 iterates this to seed
    /// the ring/fan-out.
    pub const ALL: [EventKind; EVENT_KIND_COUNT] = Self::all_variants();
}

const _: () = assert!(EventKind::ALL.len() == EVENT_KIND_COUNT);

/// Typed catalog of the 12 emitted event kinds; the wire name is the dotted
/// string (serialized verbatim, e.g. `"llm.delta"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    #[serde(rename = "session.start")]
    SessionStart,
    #[serde(rename = "session.end")]
    SessionEnd,
    #[serde(rename = "message.received")]
    MessageReceived,
    #[serde(rename = "turn.start")]
    TurnStart,
    #[serde(rename = "turn.end")]
    TurnEnd,
    #[serde(rename = "llm.delta")]
    LlmDelta,
    #[serde(rename = "llm.response")]
    LlmResponse,
    #[serde(rename = "tool.pre_call")]
    ToolPreCall,
    #[serde(rename = "tool.post_call")]
    ToolPostCall,
    #[serde(rename = "agent.state")]
    AgentState,
    #[serde(rename = "agent.yield")]
    AgentYield,
    #[serde(rename = "task.done")]
    TaskDone,
}

impl EventKind {
    /// Wire name for `EventEnvelope.kind` (a frozen dotted `String`).
    pub const fn as_str(self) -> &'static str {
        match self {
            EventKind::SessionStart => "session.start",
            EventKind::SessionEnd => "session.end",
            EventKind::MessageReceived => "message.received",
            EventKind::TurnStart => "turn.start",
            EventKind::TurnEnd => "turn.end",
            EventKind::LlmDelta => "llm.delta",
            EventKind::LlmResponse => "llm.response",
            EventKind::ToolPreCall => "tool.pre_call",
            EventKind::ToolPostCall => "tool.post_call",
            EventKind::AgentState => "agent.state",
            EventKind::AgentYield => "agent.yield",
            EventKind::TaskDone => "task.done",
        }
    }

    /// Parses a wire `kind` string; `None` for unknown/deferred names.
    pub fn from_wire(kind: &str) -> Option<Self> {
        match kind {
            "session.start" => Some(EventKind::SessionStart),
            "session.end" => Some(EventKind::SessionEnd),
            "message.received" => Some(EventKind::MessageReceived),
            "turn.start" => Some(EventKind::TurnStart),
            "turn.end" => Some(EventKind::TurnEnd),
            "llm.delta" => Some(EventKind::LlmDelta),
            "llm.response" => Some(EventKind::LlmResponse),
            "tool.pre_call" => Some(EventKind::ToolPreCall),
            "tool.post_call" => Some(EventKind::ToolPostCall),
            "agent.state" => Some(EventKind::AgentState),
            "agent.yield" => Some(EventKind::AgentYield),
            "task.done" => Some(EventKind::TaskDone),
            _ => None,
        }
    }

    /// Durable = retained in the reconnect ring (all but `llm.delta`).
    pub const fn is_durable(self) -> bool {
        is_durable(self)
    }
}

/// Frozen envelope (PLAN §7.1 / SPEC §10.1). Immutable after sequencing; the
/// sequencer task is the only code that assigns `seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub event_id: Ulid,
    pub seq: u64,
    pub session_id: Ulid,
    pub actor_id: String,
    pub kind: String,
    pub occurred_at: String,
    pub causation_id: Option<Ulid>,
    pub correlation_id: Option<Ulid>,
    pub payload: Value,
}

impl EventEnvelope {
    /// Stamps a fully-populated envelope at emit time. Called only by the
    /// per-session sequencer task (single `seq` assignment point).
    pub fn new(session_id: Ulid, seq: u64, kind: EventKind, payload: Value) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            event_id: Ulid::new(),
            seq,
            session_id,
            actor_id: ROOT_ACTOR_ID.to_string(),
            kind: kind.as_str().to_string(),
            occurred_at: now_rfc3339(),
            causation_id: None,
            correlation_id: None,
            payload,
        }
    }

    /// Sets `causation_id` (the triggering command/event id).
    pub fn with_causation_id(mut self, id: Ulid) -> Self {
        self.causation_id = Some(id);
        self
    }

    /// Sets `correlation_id` (the task id, stable across yield/resume).
    pub fn with_correlation_id(mut self, id: Ulid) -> Self {
        self.correlation_id = Some(id);
        self
    }
}

/// Wire vocabulary of `TurnEnd.outcome` (ticket 01: distinct from the internal
/// loop type; `StepBudgetExhausted` never leaks — it maps to `yielded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndOutcome {
    Done,
    Yielded,
    Interrupted,
    Error,
}

/// Bounded typed error summary on `TurnEnd` (Oracle P1-4): enough for an
/// event-only client to reproduce the CLI error line; no raw secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnError {
    pub code: String,
    pub message: String,
}

/// Frozen tool source on `ToolPreCall` (only native tools exist this slice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolSource {
    #[serde(rename = "native")]
    Native,
}

/// Frozen terminal task status (`task.done` fires once per task, on Done only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    #[serde(rename = "done")]
    Done,
}

// --- The 12 frozen payload structs (ticket 01), serialized inside
// --- `EventEnvelope.payload`. Optional fields use skip-if-none (matches the
// --- Phase-1 `ToolResult` convention).

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStart {
    pub workspace: String,
    pub root_actor_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEnd {
    pub reason: String,
    pub final_seq: u64,
}

/// `mode` wire vocab: "next-turn" | "steer" | "interrupt" (recorded only; the
/// typed `DeliveryMode` lands with the command router, todo 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageReceived {
    pub command_id: Ulid,
    pub source: String,
    pub target: String,
    pub mode: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStart {
    pub turn_id: Ulid,
    pub actor_id: String,
    pub task_id: Ulid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnEnd {
    pub turn_id: Ulid,
    pub actor_id: String,
    pub outcome: TurnEndOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<TurnError>,
}

/// `delta` maps the frozen Phase-1 `StreamDelta` (no serde derives upstream)
/// onto the wire with a `delta_type` variant tag (ticket 01).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmDelta {
    pub turn_id: Ulid,
    pub step_id: Ulid,
    #[serde(
        serialize_with = "serialize_stream_delta",
        deserialize_with = "deserialize_stream_delta"
    )]
    pub delta: StreamDelta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmResponse {
    pub turn_id: Ulid,
    pub step_id: Ulid,
    pub message: AssistantMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPreCall {
    pub tool_call_id: String,
    pub name: String,
    pub source: ToolSource,
    pub args: Value,
    pub actor_id: String,
}

/// Reuses the `ToolResult` wire shape for status/output/metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPostCall {
    pub tool_call_id: String,
    pub name: String,
    pub status: ToolStatus,
    pub output: String,
    pub metadata: Value,
}

/// `from`/`to` wire vocab: "idle" | "running" | "yielded".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentStateChanged {
    pub actor_id: String,
    pub from: String,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Reuses the `HumanRequest` fields (question/kind/details) with turn context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentYield {
    pub actor_id: String,
    pub turn_id: Ulid,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDone {
    pub task_id: Ulid,
    pub actor_id: String,
    pub result_message_id: Ulid,
    pub status: TaskStatus,
}

// --- StreamDelta <-> JSON adapter (delta_type tag) ---

fn serialize_stream_delta<S: serde::Serializer>(
    delta: &StreamDelta,
    s: S,
) -> Result<S::Ok, S::Error> {
    let value = match delta {
        StreamDelta::Text(text) => serde_json::json!({ "delta_type": "text", "text": text }),
        StreamDelta::Thinking(text) => {
            serde_json::json!({ "delta_type": "thinking", "text": text })
        }
        StreamDelta::ToolCallStarted { index, name } => serde_json::json!({
            "delta_type": "tool_call_started",
            "index": index,
            "name": name,
        }),
        StreamDelta::ToolCallArgs { index, fragment } => serde_json::json!({
            "delta_type": "tool_call_args",
            "index": index,
            "fragment": fragment,
        }),
    };
    value.serialize(s)
}

fn deserialize_stream_delta<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<StreamDelta, D::Error> {
    let v = Value::deserialize(d)?;
    let tag = v.get("delta_type").and_then(Value::as_str);
    match tag {
        Some("text") => parse_str_field(&v, "text")
            .map(StreamDelta::Text)
            .ok_or_else(de_error),
        Some("thinking") => parse_str_field(&v, "text")
            .map(StreamDelta::Thinking)
            .ok_or_else(de_error),
        Some("tool_call_started") => {
            let index = match v.get("index").and_then(Value::as_u64) {
                Some(i) => usize::try_from(i).map_err(|_| de_error())?,
                None => return Err(de_error()),
            };
            match parse_str_field(&v, "name") {
                Some(name) => Ok(StreamDelta::ToolCallStarted { index, name }),
                None => Err(de_error()),
            }
        }
        Some("tool_call_args") => {
            let index = match v.get("index").and_then(Value::as_u64) {
                Some(i) => usize::try_from(i).map_err(|_| de_error())?,
                None => return Err(de_error()),
            };
            match parse_str_field(&v, "fragment") {
                Some(fragment) => Ok(StreamDelta::ToolCallArgs { index, fragment }),
                None => Err(de_error()),
            }
        }
        _ => Err(de_error()),
    }
}

fn parse_str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn de_error<E: serde::de::Error>() -> E {
    serde::de::Error::custom("invalid llm.delta stream delta payload")
}

// --- RFC 3339 timestamp (no chrono dependency; UTC only) ---

/// Current UTC time as RFC 3339 with millisecond precision (`...Z`).
/// Also used by the history store for its row timestamps.
pub(crate) fn now_rfc3339() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // Duration since the epoch is always present on supported platforms.
        .unwrap_or_default();
    rfc3339_from_unix(d.as_secs(), d.subsec_millis())
}

fn rfc3339_from_unix(secs: u64, millis: u32) -> String {
    let (year, month, day, hour, minute, second) = civil_from_unix(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Days-to-civil conversion (Howard Hinnant's algorithm), UTC only.
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        (rem / 3_600) as u32,
        (rem % 3_600 / 60) as u32,
        (rem % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FROZEN_KIND_NAMES: [&str; 12] = [
        "session.start",
        "session.end",
        "message.received",
        "turn.start",
        "turn.end",
        "llm.delta",
        "llm.response",
        "tool.pre_call",
        "tool.post_call",
        "agent.state",
        "agent.yield",
        "task.done",
    ];

    #[test]
    fn event_kind_catalog_is_exactly_the_frozen_twelve() {
        let kinds = EventKind::ALL;
        assert_eq!(EVENT_KIND_COUNT, 12);
        assert_eq!(kinds.len(), 12);
        let names: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
        assert_eq!(names, FROZEN_KIND_NAMES);
        // Wire names are unique and parse back.
        for (kind, name) in kinds.iter().zip(FROZEN_KIND_NAMES) {
            assert_eq!(EventKind::from_wire(name), Some(*kind));
            assert_eq!(serde_json::to_string(kind).unwrap(), format!("\"{name}\""));
        }
        // Deferred names never construct a kind.
        assert_eq!(EventKind::from_wire("prompt.pre_llm"), None);
        assert_eq!(EventKind::from_wire("plugin.error"), None);
    }

    #[test]
    fn event_kind_durability_marks_only_llm_delta_live_only() {
        let durable: Vec<bool> = EventKind::ALL.iter().map(|k| k.is_durable()).collect();
        assert_eq!(durable.iter().filter(|d| !**d).count(), 1);
        assert!(!EventKind::LlmDelta.is_durable());
        assert!(EventKind::TurnEnd.is_durable());
        assert!(EventKind::SessionStart.is_durable());
    }

    #[test]
    fn envelope_roundtrips_through_serde_json_unchanged() {
        let session_id = Ulid::new();
        let task_id = Ulid::new();
        let cause_id = Ulid::new();
        let payload = serde_json::to_value(LlmResponse {
            turn_id: Ulid::new(),
            step_id: Ulid::new(),
            message: AssistantMessage {
                text: "doné — ✓".into(),
                thinking: "thought".into(),
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    args: json!({"path": "/a"}),
                    args_valid: true,
                }],
                finish_reason: Some("stop".into()),
                usage: Some(Usage {
                    prompt_tokens: 12,
                    completion_tokens: 34,
                    total_tokens: 46,
                }),
            },
            usage: Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 34,
                total_tokens: 46,
            }),
            finish_reason: Some("stop".into()),
        })
        .unwrap();

        let envelope = EventEnvelope::new(session_id, 7, EventKind::LlmResponse, payload)
            .with_causation_id(cause_id)
            .with_correlation_id(task_id);

        let wire = serde_json::to_string(&envelope).unwrap();
        let back: EventEnvelope = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, envelope);
        assert_eq!(back.schema_version, 1);
        assert_eq!(back.actor_id, "agent:main");
        assert_eq!(back.kind, "llm.response");
    }

    #[test]
    fn all_payload_structs_roundtrip_through_serde_json() {
        let cases: Vec<(&str, Value)> = vec![
            (
                "session.start",
                serde_json::to_value(&SessionStart {
                    workspace: "/w".into(),
                    root_actor_id: ROOT_ACTOR_ID.into(),
                })
                .unwrap(),
            ),
            (
                "session.end",
                serde_json::to_value(&SessionEnd {
                    reason: "stop".into(),
                    final_seq: 41,
                })
                .unwrap(),
            ),
            (
                "message.received",
                serde_json::to_value(&MessageReceived {
                    command_id: Ulid::new(),
                    source: "user:http".into(),
                    target: ROOT_ACTOR_ID.into(),
                    mode: "next-turn".into(),
                    content: "héllo".into(),
                })
                .unwrap(),
            ),
            (
                "turn.start",
                serde_json::to_value(&TurnStart {
                    turn_id: Ulid::new(),
                    actor_id: ROOT_ACTOR_ID.into(),
                    task_id: Ulid::new(),
                })
                .unwrap(),
            ),
            (
                "turn.end",
                serde_json::to_value(&TurnEnd {
                    turn_id: Ulid::new(),
                    actor_id: ROOT_ACTOR_ID.into(),
                    outcome: TurnEndOutcome::Yielded,
                    usage: None,
                    duration_ms: 250,
                    error: Some(TurnError {
                        code: "provider".into(),
                        message: "boom".into(),
                    }),
                })
                .unwrap(),
            ),
            (
                "llm.delta",
                serde_json::to_value(&LlmDelta {
                    turn_id: Ulid::new(),
                    step_id: Ulid::new(),
                    delta: StreamDelta::ToolCallArgs {
                        index: 2,
                        fragment: "{\"pa".into(),
                    },
                })
                .unwrap(),
            ),
            (
                "llm.response",
                serde_json::to_value(&LlmResponse {
                    turn_id: Ulid::new(),
                    step_id: Ulid::new(),
                    message: AssistantMessage::default(),
                    usage: None,
                    finish_reason: None,
                })
                .unwrap(),
            ),
            (
                "tool.pre_call",
                serde_json::to_value(&ToolPreCall {
                    tool_call_id: "call_1".into(),
                    name: "read".into(),
                    source: ToolSource::Native,
                    args: json!({"path": "/a"}),
                    actor_id: ROOT_ACTOR_ID.into(),
                })
                .unwrap(),
            ),
            (
                "tool.post_call",
                serde_json::to_value(&ToolPostCall {
                    tool_call_id: "call_1".into(),
                    name: "read".into(),
                    status: crate::tools::ToolStatus::Ok,
                    output: "contents".into(),
                    metadata: Value::Null,
                })
                .unwrap(),
            ),
            (
                "agent.state",
                serde_json::to_value(&AgentStateChanged {
                    actor_id: ROOT_ACTOR_ID.into(),
                    from: "running".into(),
                    to: "yielded".into(),
                    reason: None,
                })
                .unwrap(),
            ),
            (
                "agent.yield",
                serde_json::to_value(&AgentYield {
                    actor_id: ROOT_ACTOR_ID.into(),
                    turn_id: Ulid::new(),
                    question: "proceed?".into(),
                    kind: Some("budget".into()),
                    details: Some(json!({"steps": 50})),
                })
                .unwrap(),
            ),
            (
                "task.done",
                serde_json::to_value(&TaskDone {
                    task_id: Ulid::new(),
                    actor_id: ROOT_ACTOR_ID.into(),
                    result_message_id: Ulid::new(),
                    status: TaskStatus::Done,
                })
                .unwrap(),
            ),
        ];
        assert_eq!(cases.len(), EVENT_KIND_COUNT);

        for (kind, payload) in cases {
            let wire = serde_json::to_string(&payload).unwrap();
            let back: Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(back, payload, "payload roundtrip failed for {kind}");
        }
    }

    #[test]
    fn llm_delta_stream_delta_wire_uses_delta_type_tag() {
        let delta = LlmDelta {
            turn_id: Ulid::new(),
            step_id: Ulid::new(),
            delta: StreamDelta::Text("héllo ✓".into()),
        };
        let v = serde_json::to_value(&delta).unwrap();
        assert_eq!(v["delta"]["delta_type"], "text");
        assert_eq!(v["delta"]["text"], "héllo ✓");
        let back: LlmDelta = serde_json::from_value(v).unwrap();
        assert_eq!(back, delta);

        let started = LlmDelta {
            turn_id: delta.turn_id,
            step_id: delta.step_id,
            delta: StreamDelta::ToolCallStarted {
                index: 0,
                name: "bash".into(),
            },
        };
        let v = serde_json::to_value(&started).unwrap();
        assert_eq!(v["delta"]["delta_type"], "tool_call_started");
        assert_eq!(v["delta"]["name"], "bash");
        let back: LlmDelta = serde_json::from_value(v).unwrap();
        assert_eq!(back, started);

        // Unknown / malformed delta payloads are rejected, not coerced.
        let bad: Result<LlmDelta, _> = serde_json::from_value(json!({
            "turn_id": delta.turn_id.to_string(),
            "step_id": delta.step_id.to_string(),
            "delta": {"delta_type": "prompt.pre_llm"},
        }));
        assert!(bad.is_err());
    }

    #[test]
    fn rfc3339_timestamps_are_utc_with_millis() {
        assert_eq!(rfc3339_from_unix(0, 0), "1970-01-01T00:00:00.000Z");
        // 2026-09-04T00:00:00Z == 1_788_480_000
        assert_eq!(
            rfc3339_from_unix(1_788_480_000, 42),
            "2026-09-04T00:00:00.042Z"
        );
        assert_eq!(
            rfc3339_from_unix(951_782_400, 0),
            "2000-02-29T00:00:00.000Z"
        );
    }

    /// Manual QA observable: one fully-populated envelope as JSON (paste into
    /// the todo report as the human-readable frozen shape).
    #[test]
    fn manual_qa_print_fully_populated_envelope() {
        let session_id = Ulid::new();
        let task_id = Ulid::new();
        let command_id = Ulid::new();
        let payload = serde_json::to_value(&LlmDelta {
            turn_id: Ulid::new(),
            step_id: Ulid::new(),
            delta: StreamDelta::ToolCallStarted {
                index: 0,
                name: "read".into(),
            },
        })
        .unwrap();
        let envelope = EventEnvelope::new(session_id, 3, EventKind::LlmDelta, payload)
            .with_causation_id(command_id)
            .with_correlation_id(task_id);
        eprintln!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    }
}
