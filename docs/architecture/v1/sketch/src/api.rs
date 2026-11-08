//! Stands in for the `withnav-client-api` crate: the typed wire contract. No domain type appears here.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod v1 {
    use super::*;

    /// Same JSON as the untyped envelope of today: `kind` and `payload` are flattened in.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct EventEnvelope {
        pub schema: u16,
        pub seq: u64,
        pub session_id: String,
        #[serde(flatten, deserialize_with = "lenient")]
        pub event: Event,
    }

    /// `#[serde(other)]` only accepts an unknown kind that carries no `payload`, and every real
    /// kind carries one. Retrying without the payload turns an unknown kind into `Unknown`,
    /// while a known kind with a broken payload still fails (the retry fails too).
    fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Event, D::Error> {
        let mut raw = Value::deserialize(d)?;
        match Event::deserialize(&raw) {
            Ok(event) => Ok(event),
            Err(first) => {
                if let Some(map) = raw.as_object_mut() {
                    map.remove("payload");
                }
                Event::deserialize(raw).map_err(|_| serde::de::Error::custom(first))
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct MessageReceived {
        pub message_id: String,
        pub text: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct LlmDelta {
        pub text: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ToolPreCall {
        pub call_id: String,
        pub name: String,
        pub args: Value,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ToolPostCall {
        pub call_id: String,
        pub status: String,
        pub output: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct PermissionRequested {
        pub request_id: String,
        pub tool: String,
        pub reason: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct TurnEnd {
        pub finish: String,
    }

    /// Wire names are the ones in use today. `Unknown` lets a client built before a new kind
    /// existed keep reading the stream; it never serialises a payload back.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "kind", content = "payload")]
    pub enum Event {
        #[serde(rename = "message.received")]
        MessageReceived(MessageReceived),
        #[serde(rename = "llm.delta")]
        LlmDelta(LlmDelta),
        #[serde(rename = "tool.pre_call")]
        ToolPreCall(ToolPreCall),
        #[serde(rename = "tool.post_call")]
        ToolPostCall(ToolPostCall),
        #[serde(rename = "permission.requested")]
        PermissionRequested(PermissionRequested),
        #[serde(rename = "turn.end")]
        TurnEnd(TurnEnd),
        #[serde(other)]
        Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::v1::*;
    use serde_json::json;

    fn envelope(event: Event) -> EventEnvelope {
        EventEnvelope { schema: 1, seq: 7, session_id: "s1".into(), event }
    }

    #[test]
    fn events_round_trip_with_the_wire_names() {
        let cases = [
            (
                Event::MessageReceived(MessageReceived { message_id: "m1".into(), text: "hi".into() }),
                "message.received",
            ),
            (Event::LlmDelta(LlmDelta { text: "x".into() }), "llm.delta"),
            (
                Event::ToolPreCall(ToolPreCall {
                    call_id: "c".into(),
                    name: "echo".into(),
                    args: json!({"a": 1}),
                }),
                "tool.pre_call",
            ),
            (
                Event::ToolPostCall(ToolPostCall {
                    call_id: "c".into(),
                    status: "ok".into(),
                    output: "o".into(),
                }),
                "tool.post_call",
            ),
            (
                Event::PermissionRequested(PermissionRequested {
                    request_id: "r".into(),
                    tool: "echo".into(),
                    reason: "why".into(),
                }),
                "permission.requested",
            ),
            (Event::TurnEnd(TurnEnd { finish: "stop".into() }), "turn.end"),
        ];
        for (event, kind) in cases {
            let env = envelope(event);
            let json = serde_json::to_value(&env).unwrap();
            assert_eq!(json["kind"], kind);
            assert_eq!(json["seq"], 7);
            assert!(json["payload"].is_object());
            assert_eq!(serde_json::from_value::<EventEnvelope>(json).unwrap(), env);
        }
    }

    #[test]
    fn an_unknown_kind_decodes_to_unknown() {
        let raw = json!({
            "schema": 1, "seq": 9, "session_id": "s1",
            "kind": "plugin.future_thing", "payload": {"anything": [1, 2]}
        });
        let env: EventEnvelope = serde_json::from_value(raw).unwrap();
        assert_eq!(env.event, Event::Unknown);
        assert_eq!(env.seq, 9);
    }
}
