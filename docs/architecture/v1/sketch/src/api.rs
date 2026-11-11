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
        #[serde(flatten)]
        pub event: Event,
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

    /// Wire names are the ones in use today. `Unknown` retains an unrecognized kind and its
    /// payload as a JSON value so older clients can display or forward it. Original JSON
    /// bytes are not preserved; an absent payload is serialized as null.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        MessageReceived(MessageReceived),
        LlmDelta(LlmDelta),
        ToolPreCall(ToolPreCall),
        ToolPostCall(ToolPostCall),
        PermissionRequested(PermissionRequested),
        TurnEnd(TurnEnd),
        Unknown { kind: String, payload: Value },
    }

    /// Hand-written serde: the derived `tag`/`content` shape cannot represent `Unknown`
    /// (a data-carrying `#[serde(other)]` does not exist, and deriving would nest the
    /// retained kind under a literal `"Unknown"` tag). Only an unknown `kind` falls back
    /// to `Unknown`; a known kind with a malformed payload is a hard error, not a fallback.
    impl Serialize for Event {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            let (kind, payload) = match self {
                Event::MessageReceived(v) => ("message.received", serde_json::to_value(v)),
                Event::LlmDelta(v) => ("llm.delta", serde_json::to_value(v)),
                Event::ToolPreCall(v) => ("tool.pre_call", serde_json::to_value(v)),
                Event::ToolPostCall(v) => ("tool.post_call", serde_json::to_value(v)),
                Event::PermissionRequested(v) => ("permission.requested", serde_json::to_value(v)),
                Event::TurnEnd(v) => ("turn.end", serde_json::to_value(v)),
                Event::Unknown { kind, payload } => (kind.as_str(), Ok(payload.clone())),
            };
            let payload = payload.map_err(serde::ser::Error::custom)?;
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry("kind", kind)?;
            map.serialize_entry("payload", &payload)?;
            map.end()
        }
    }

    impl<'de> Deserialize<'de> for Event {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            #[derive(Deserialize)]
            struct Raw {
                kind: String,
                #[serde(default)]
                payload: Value,
            }

            let raw = Raw::deserialize(d)?;
            let bad_payload = |e: serde_json::Error| serde::de::Error::custom(e.to_string());
            match raw.kind.as_str() {
                "message.received" => {
                    Ok(Event::MessageReceived(serde_json::from_value(raw.payload).map_err(bad_payload)?))
                }
                "llm.delta" => Ok(Event::LlmDelta(serde_json::from_value(raw.payload).map_err(bad_payload)?)),
                "tool.pre_call" => {
                    Ok(Event::ToolPreCall(serde_json::from_value(raw.payload).map_err(bad_payload)?))
                }
                "tool.post_call" => {
                    Ok(Event::ToolPostCall(serde_json::from_value(raw.payload).map_err(bad_payload)?))
                }
                "permission.requested" => {
                    Ok(Event::PermissionRequested(serde_json::from_value(raw.payload).map_err(bad_payload)?))
                }
                "turn.end" => Ok(Event::TurnEnd(serde_json::from_value(raw.payload).map_err(bad_payload)?)),
                _ => Ok(Event::Unknown { kind: raw.kind, payload: raw.payload }),
            }
        }
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
    fn an_unknown_kind_is_kept_untouched() {
        let raw = json!({
            "schema": 1, "seq": 9, "session_id": "s1",
            "kind": "plugin.future_thing", "payload": {"anything": [1, 2]}
        });
        let env: EventEnvelope = serde_json::from_value(raw.clone()).unwrap();
        // The original kind and raw payload are retained, not dropped.
        assert_eq!(
            env.event,
            Event::Unknown { kind: "plugin.future_thing".into(), payload: json!({"anything": [1, 2]}) }
        );
        assert_eq!(env.seq, 9);
        // Round-trip preserves this envelope's JSON value, not its original encoded bytes.
        assert_eq!(serde_json::to_value(&env).unwrap(), raw);
    }

    #[test]
    fn a_known_kind_with_a_malformed_payload_is_an_error() {
        let raw = json!({
            "schema": 1, "seq": 10, "session_id": "s1",
            "kind": "message.received", "payload": {"nonsense": true}
        });
        assert!(serde_json::from_value::<EventEnvelope>(raw).is_err());
    }
}
