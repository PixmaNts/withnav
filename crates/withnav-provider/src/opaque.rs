//! Provider-opaque reasoning kept on an assistant message.
//!
//! Some providers return reasoning only their own model can read back: a signed
//! thinking block, an encrypted reasoning item, a thought signature. Anthropic
//! rejects a forged or altered thinking signature with a 400, and requires the
//! signed block to come back within a tool-use turn (measured through OmniRoute,
//! 2026-10-20). The assistant message had no place to keep those bytes, so they
//! were lost between two steps of a turn. They are stored here, byte for byte,
//! bound to the model that produced them, so only that model is ever shown them.

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::catalog::{UNPARSEABLE_ENDPOINT, endpoint_without_credentials};

/// Wire name of the Chat Completions dialect in a [`Producer`].
pub const CHAT_COMPLETIONS: &str = "chat_completions";

/// Reasoning data only its producer can read, kept for a replay to that producer.
///
/// Stored inside the transcript JSON as `"opaque"`, absent when there is none, so
/// transcripts and request bodies written before this field existed are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueReasoning {
    pub producer: Producer,
    /// In the order the producer sent them; never empty once stored.
    pub items: Vec<OpaqueItem>,
}

/// The model the bytes are bound to: dialect, endpoint and upstream model id.
///
/// Equal endpoint and model do not prove equal routing behind a gateway, so the
/// identity is the exact triple; anything looser could send a signature to a
/// model that rejects it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Producer {
    /// Dialect of the bytes: `chat_completions`, `anthropic`, `gemini`, `responses`.
    pub wire: String,
    /// Base URL without credentials, query or fragment.
    pub endpoint: String,
    /// Upstream model id sent in the request.
    pub model: String,
}

impl Producer {
    /// The producer of bytes received from `model` at `base_url` over Chat
    /// Completions. The endpoint is stored without credentials, query or
    /// fragment and without a trailing slash, so the same endpoint written two
    /// ways is one identity.
    pub fn chat_completions(base_url: &str, model: &str) -> Self {
        Self {
            wire: CHAT_COMPLETIONS.into(),
            endpoint: endpoint_without_credentials(base_url),
            model: model.into(),
        }
    }

    /// Whether the producer names a real model on a real endpoint. The sentinel for
    /// an endpoint that did not parse identifies nothing, so bytes bound to it
    /// could be replayed to any model.
    pub fn is_valid(&self) -> bool {
        !self.wire.is_empty()
            && !self.endpoint.is_empty()
            && self.endpoint != UNPARSEABLE_ENDPOINT
            && !self.model.is_empty()
    }
}

/// One piece of opaque reasoning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueItem {
    /// Wire-specific, chosen by the adapter that captured it.
    pub kind: String,
    /// JSON text exactly as the provider sent it.
    ///
    /// A `String`, not a raw JSON value: an internally tagged enum buffers its
    /// fields while reading, and a raw value cannot be read back from that buffer
    /// (`invalid type: newtype struct`, verified 2026-10-21). A string keeps key
    /// order, escapes and whitespace, which a signature may cover.
    pub data: String,
}

/// Whether `wire` can render an item of `kind` into a request.
///
/// No dialect writes opaque data yet, so nothing is sendable: an item stored
/// today is kept in the transcript but never reaches a request, which keeps
/// every request body what it was before the field existed. A dialect that
/// learns to replay a kind adds it here, and the replay rule starts applying to
/// it everywhere at once (projection, gauge, provider boundary).
pub fn sendable(wire: &str, kind: &str) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    if kind == TEST_SENDABLE {
        return true;
    }
    let _ = (wire, kind);
    false
}

/// A kind sendable on every wire, so the keep path of the replay rule can be
/// tested while no real dialect renders opaque data. Compiled only for tests.
#[cfg(any(test, feature = "test-support"))]
pub const TEST_SENDABLE: &str = "test.sendable";

/// Applies the replay rule: the reasoning stays only if it is going back to the
/// model that produced it, and only the items `destination`'s wire can render.
///
/// Bytes bound to one model (a signature, an encrypted item) are rejected or
/// misread by another, and a request must never carry them there; so a
/// different wire, endpoint or model, or a destination that identifies nothing,
/// removes everything. Order of the kept items is the producer's. `None` when
/// nothing is left. Removal never turns the data into text.
pub fn retain_for(opaque: &mut Option<OpaqueReasoning>, destination: &Producer) {
    let Some(reasoning) = opaque else {
        return;
    };
    if reasoning.producer != *destination || !destination.is_valid() {
        *opaque = None;
        return;
    }
    reasoning
        .items
        .retain(|item| sendable(&destination.wire, &item.kind));
    if reasoning.items.is_empty() {
        *opaque = None;
    }
}

/// Reads the `opaque` field without ever failing the transcript around it.
///
/// A transcript that fails to parse refuses to resume, and the session is lost
/// to the user; reasoning that cannot be replayed is not worth that. So content
/// problems only drop data, and each drop is logged once per message with counts
/// and reasons, never the payload:
/// - producer missing or invalid: the whole field goes (no safe replay without it);
/// - `items` not an array, or no item left: `None`;
/// - an item that is not an object with a string `kind` and a string `data` that
///   parses as JSON: that item alone goes, siblings and their order stay;
/// - an unknown `kind` is kept, the adapter that reads it decides.
pub fn lenient<'de, D>(deserializer: D) -> Result<Option<OpaqueReasoning>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let Value::Object(mut object) = value else {
        tracing::warn!("dropped opaque reasoning: not an object");
        return Ok(None);
    };
    let producer = object
        .remove("producer")
        .and_then(|value| serde_json::from_value::<Producer>(value).ok())
        .filter(Producer::is_valid);
    let Some(producer) = producer else {
        tracing::warn!("dropped opaque reasoning: producer missing or invalid");
        return Ok(None);
    };
    let Some(Value::Array(raw_items)) = object.remove("items") else {
        tracing::warn!("dropped opaque reasoning: items is not an array");
        return Ok(None);
    };
    let total = raw_items.len();
    let items: Vec<OpaqueItem> = raw_items.into_iter().filter_map(read_item).collect();
    if items.len() < total {
        tracing::warn!(
            dropped = total - items.len(),
            kept = items.len(),
            "dropped invalid opaque reasoning items"
        );
    }
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(OpaqueReasoning { producer, items }))
}

fn read_item(value: Value) -> Option<OpaqueItem> {
    let Value::Object(mut object) = value else {
        return None;
    };
    let Value::String(kind) = object.remove("kind")? else {
        return None;
    };
    let Value::String(data) = object.remove("data")? else {
        return None;
    };
    serde_json::from_str::<IgnoredAny>(&data).ok()?;
    Some(OpaqueItem { kind, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dest() -> Producer {
        Producer::chat_completions("https://gw.example/v1", "model-a")
    }

    fn held(producer: Producer, kinds: &[&str]) -> Option<OpaqueReasoning> {
        Some(OpaqueReasoning {
            producer,
            items: kinds
                .iter()
                .enumerate()
                .map(|(i, kind)| OpaqueItem {
                    kind: (*kind).into(),
                    data: format!("{i}"),
                })
                .collect(),
        })
    }

    fn item_kinds(opaque: &Option<OpaqueReasoning>) -> Vec<&str> {
        opaque
            .as_ref()
            .map(|o| o.items.iter().map(|i| i.kind.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn nothing_is_sendable_but_the_test_kind() {
        assert!(!sendable(CHAT_COMPLETIONS, "thinking"));
        assert!(!sendable("anthropic", "reasoning_details"));
        assert!(sendable(CHAT_COMPLETIONS, TEST_SENDABLE));
        assert!(sendable("anthropic", TEST_SENDABLE));
    }

    #[test]
    fn the_chat_completions_producer_normalizes_its_endpoint() {
        let a = Producer::chat_completions("https://user:pw@gw.example/v1/?k=1#f", "m");
        assert_eq!(a, Producer::chat_completions("https://gw.example/v1", "m"));
        assert_eq!(a.wire, CHAT_COMPLETIONS);
        assert!(a.is_valid());
        assert!(!Producer::chat_completions("not a url", "m").is_valid());
    }

    #[test]
    fn retain_for_keeps_the_sendable_items_of_the_same_producer_in_order() {
        let mut opaque = held(dest(), &[TEST_SENDABLE, "other", "test.sendable", "other"]);
        retain_for(&mut opaque, &dest());
        assert_eq!(item_kinds(&opaque), [TEST_SENDABLE, TEST_SENDABLE]);
        let datas: Vec<_> = opaque.unwrap().items.into_iter().map(|i| i.data).collect();
        assert_eq!(datas, ["0", "2"]);
    }

    #[test]
    fn retain_for_removes_what_the_wire_cannot_render() {
        let mut opaque = held(dest(), &["other", "thinking"]);
        retain_for(&mut opaque, &dest());
        assert!(opaque.is_none());
    }

    #[test]
    fn retain_for_removes_everything_for_another_producer() {
        let other_model = Producer::chat_completions("https://gw.example/v1", "model-b");
        let other_endpoint = Producer::chat_completions("https://other.example/v1", "model-a");
        let other_wire = Producer {
            wire: "anthropic".into(),
            ..dest()
        };
        for other in [other_model, other_endpoint, other_wire] {
            let mut opaque = held(other, &[TEST_SENDABLE]);
            retain_for(&mut opaque, &dest());
            assert!(opaque.is_none());
        }
    }

    #[test]
    fn retain_for_removes_everything_for_an_invalid_destination() {
        let bad = Producer::chat_completions("not a url", "model-a");
        let mut opaque = held(bad.clone(), &[TEST_SENDABLE]);
        retain_for(&mut opaque, &bad);
        assert!(opaque.is_none());
        let empty_model = Producer::chat_completions("https://gw.example/v1", "");
        let mut opaque = held(empty_model.clone(), &[TEST_SENDABLE]);
        retain_for(&mut opaque, &empty_model);
        assert!(opaque.is_none());
    }

    #[test]
    fn retain_for_leaves_none_alone() {
        let mut opaque = None;
        retain_for(&mut opaque, &dest());
        assert!(opaque.is_none());
    }

    #[derive(Debug, Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "lenient")]
        opaque: Option<OpaqueReasoning>,
    }

    fn read(value: Value) -> Option<OpaqueReasoning> {
        serde_json::from_value::<Holder>(json!({ "opaque": value }))
            .expect("lenient never fails on content")
            .opaque
    }

    fn producer() -> Value {
        json!({"wire": "anthropic", "endpoint": "https://gw.example/v1", "model": "claude-x"})
    }

    fn kinds(reasoning: &OpaqueReasoning) -> Vec<&str> {
        reasoning.items.iter().map(|i| i.kind.as_str()).collect()
    }

    #[test]
    fn valid_items_are_kept_in_order() {
        let got = read(json!({"producer": producer(), "items": [
            {"kind": "thinking", "data": "{\"b\":1,\"a\":2}"},
            {"kind": "redacted", "data": "\"xyz\""},
        ]}))
        .unwrap();
        assert_eq!(got.producer.model, "claude-x");
        assert_eq!(kinds(&got), ["thinking", "redacted"]);
        assert_eq!(got.items[0].data, "{\"b\":1,\"a\":2}");
    }

    #[test]
    fn a_bad_item_is_dropped_alone_and_order_is_kept() {
        let got = read(json!({"producer": producer(), "items": [
            {"kind": "one", "data": "1"},
            {"kind": 7, "data": "2"},
            {"kind": "two", "data": "2"},
            "not an object",
            {"kind": "missing data"},
            {"kind": "three", "data": "3"},
        ]}))
        .unwrap();
        assert_eq!(kinds(&got), ["one", "two", "three"]);
    }

    #[test]
    fn data_that_is_not_json_is_dropped() {
        let got = read(json!({"producer": producer(), "items": [
            {"kind": "bad", "data": "{not json"},
            {"kind": "trailing", "data": "1 2"},
            {"kind": "empty", "data": ""},
            {"kind": "not a string", "data": {"a": 1}},
            {"kind": "good", "data": "{\"a\":1}"},
        ]}))
        .unwrap();
        assert_eq!(kinds(&got), ["good"]);
    }

    #[test]
    fn a_missing_or_invalid_producer_drops_the_field() {
        let items = json!([{"kind": "k", "data": "1"}]);
        assert!(read(json!({"items": items})).is_none());
        for bad in [
            json!({"wire": "", "endpoint": "https://e", "model": "m"}),
            json!({"wire": "w", "endpoint": "", "model": "m"}),
            json!({"wire": "w", "endpoint": "https://e", "model": ""}),
            json!({"wire": "w", "endpoint": UNPARSEABLE_ENDPOINT, "model": "m"}),
            json!({"wire": "w", "endpoint": "https://e"}),
            json!("anthropic"),
            Value::Null,
        ] {
            assert!(read(json!({"producer": bad, "items": items})).is_none());
        }
    }

    #[test]
    fn an_unknown_kind_is_kept() {
        let got = read(json!({"producer": producer(), "items": [
            {"kind": "from-the-future", "data": "null"},
        ]}))
        .unwrap();
        assert_eq!(kinds(&got), ["from-the-future"]);
    }

    #[test]
    fn no_item_left_gives_none() {
        assert!(
            read(json!({"producer": producer(), "items": [
                {"kind": "bad", "data": "{"},
                17,
            ]}))
            .is_none()
        );
        assert!(read(json!({"producer": producer(), "items": []})).is_none());
        assert!(read(json!({"producer": producer(), "items": "nope"})).is_none());
        assert!(read(json!({"producer": producer()})).is_none());
    }

    #[test]
    fn a_non_object_gives_none() {
        for value in [
            json!("text"),
            json!(3),
            json!([1, 2]),
            json!(true),
            Value::Null,
        ] {
            assert!(read(value).is_none());
        }
    }

    #[test]
    fn an_absent_field_gives_none() {
        let holder: Holder = serde_json::from_str("{}").unwrap();
        assert!(holder.opaque.is_none());
    }
}
