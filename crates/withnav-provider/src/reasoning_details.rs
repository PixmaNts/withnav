//! OpenRouter's `reasoning_details` on the Chat Completions wire.
//!
//! A streaming response carries `choices[].delta.reasoning_details`: arrays of
//! entries (`reasoning.text`, `reasoning.summary`, `reasoning.encrypted`) that a
//! gateway expects back, unchanged, on the assistant message of the next
//! request. Recorded streams (2026-11-09, Anthropic Haiku 4.5, OpenAI gpt-5-nano
//! and DeepSeek R1 through OpenRouter) show how fragments arrive:
//!
//! - an entry is split into many chunks sharing one integer `index`;
//! - `text` (and `summary`) fragments are concatenated in arrival order;
//! - a later chunk at the same index adds fields (`signature` arrives alone in a
//!   last chunk, with no text); `type`, `format` and `id` come from the first
//!   chunk that carries them;
//! - an `encrypted` entry arrives whole, at its own index.
//!
//! Each assembled entry becomes one [`OpaqueItem`] (kind from [`kind_of`], data
//! the entry as compact JSON text). Assembly never fails a turn: anything that
//! is not an array of objects is ignored.
//!
//! The data is the assembled entry, not the raw fragments: string contents
//! (text, signature, encrypted data) are exact, key order is not the gateway's.
//! The stored text is what gets replayed, unchanged.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::opaque::OpaqueItem;

/// Kind of an item whose entry the Chat Completions wire writes back as
/// `"reasoning_details": [...]` on the assistant message.
pub const KIND: &str = "reasoning_details";

/// `format` of entries that come from OpenAI's Responses API through OpenRouter.
const OPENAI_RESPONSES_FORMAT: &str = "openai-responses-v1";

/// Kind of an item for the entry with this `format`.
///
/// Entries in `openai-responses-v1` are kept under their own kind, which no
/// wire sends yet: replaying them returned HTTP 500 through the OmniRoute
/// gateway on 2026-11-09, and it is not known whether the gateway or the
/// payload is at fault. The data stays captured for the day it is verified.
/// Any other format (`anthropic-claude-v1`, `unknown`, none) uses [`KIND`].
pub fn kind_of(entry: &Map<String, Value>) -> String {
    match entry.get("format").and_then(Value::as_str) {
        Some(OPENAI_RESPONSES_FORMAT) => format!("{KIND}.{OPENAI_RESPONSES_FORMAT}"),
        _ => KIND.to_string(),
    }
}

/// Fields whose fragments are concatenated rather than kept from the first chunk.
const CONCATENATED: [&str; 2] = ["text", "summary"];

/// Collects the `reasoning_details` entries of one response.
#[derive(Debug, Default)]
pub struct Assembler {
    /// Entries with an integer `index`, in index order.
    indexed: BTreeMap<i64, Map<String, Value>>,
    /// Entries without one cannot be grouped: each stands alone, in arrival order.
    loose: Vec<Map<String, Value>>,
}

impl Assembler {
    /// Feeds the `reasoning_details` value of one chunk.
    pub fn push(&mut self, details: Value) {
        let Value::Array(entries) = details else {
            return;
        };
        for entry in entries {
            let Value::Object(entry) = entry else {
                continue;
            };
            match entry.get("index").and_then(Value::as_i64) {
                Some(index) => match self.indexed.get_mut(&index) {
                    Some(held) => merge(held, entry),
                    None => {
                        self.indexed.insert(index, entry);
                    }
                },
                None => self.loose.push(entry),
            }
        }
    }

    /// The assembled items: indexed entries by index, then the loose ones.
    pub fn finish(self) -> Vec<OpaqueItem> {
        self.indexed
            .into_values()
            .chain(self.loose)
            .filter_map(|entry| {
                let kind = kind_of(&entry);
                let data = serde_json::to_string(&entry).ok()?;
                Some(OpaqueItem { kind, data })
            })
            .collect()
    }
}

fn merge(held: &mut Map<String, Value>, chunk: Map<String, Value>) {
    for (key, value) in chunk {
        if value.is_null() {
            continue;
        }
        match held.get_mut(&key) {
            Some(Value::String(have)) if CONCATENATED.contains(&key.as_str()) => {
                if let Value::String(more) = &value {
                    have.push_str(more);
                }
            }
            Some(have) if !have.is_null() => {}
            _ => {
                held.insert(key, value);
            }
        }
    }
}

/// `message` with the opaque bytes of `items` blanked out. A gateway that rejects
/// a request may quote it in its error (whole, cut short by a size limit, or
/// partly), and the message travels into client events and the stored history.
///
/// Every signature and encrypted-data value of `items` is replaced wherever it
/// appears, and so is every long run of base64 characters: a partial echo has no
/// known value to look for. Nothing is changed when `items` holds no such value.
pub(crate) fn scrub_echoed<'a>(
    message: String,
    items: impl Iterator<Item = &'a OpaqueItem>,
) -> String {
    // A base64 run this long is a token, not a word or a path segment.
    const RUN: usize = 40;
    const MARK: &str = "[opaque reasoning]";
    let mut message = message;
    let mut found = false;
    for item in items {
        let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(&item.data) else {
            continue;
        };
        for key in ["signature", "data"] {
            if let Some(Value::String(value)) = entry.get(key)
                && !value.is_empty()
            {
                found = true;
                message = message.replace(value.as_str(), MARK);
            }
        }
    }
    if !found {
        return message;
    }
    let is_base64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-');
    let mut out = String::with_capacity(message.len());
    let mut run = String::new();
    for c in message.chars().chain(std::iter::once('\0')) {
        if is_base64(c) {
            run.push(c);
            continue;
        }
        if run.chars().count() >= RUN {
            out.push_str(MARK);
        } else {
            out.push_str(&run);
        }
        run.clear();
        if c != '\0' {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assemble(chunks: &[Value]) -> Vec<Value> {
        let mut assembler = Assembler::default();
        for chunk in chunks {
            assembler.push(chunk.clone());
        }
        assembler
            .finish()
            .iter()
            .map(|item| serde_json::from_str(&item.data).unwrap())
            .collect()
    }

    #[test]
    fn text_fragments_concatenate_and_a_trailing_signature_joins_the_entry() {
        let got = assemble(&[
            json!([{"type":"reasoning.text","text":"a ","format":"anthropic-claude-v1","index":0}]),
            json!([{"type":"reasoning.text","text":"b","format":"anthropic-claude-v1","index":0}]),
            json!([{"type":"reasoning.text","signature":"SIG","format":"anthropic-claude-v1","index":0}]),
        ]);
        assert_eq!(
            got,
            [
                json!({"type":"reasoning.text","text":"a b","signature":"SIG",
                "format":"anthropic-claude-v1","index":0})
            ]
        );
    }

    #[test]
    fn distinct_indexes_stay_apart_in_index_order_whatever_the_arrival() {
        let got = assemble(&[
            json!([{"type":"reasoning.encrypted","data":"E","id":"rs_1","index":1}]),
            json!([{"type":"reasoning.summary","summary":"x","index":0}]),
            json!([{"type":"reasoning.summary","summary":"y","index":0}]),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["summary"], "xy");
        assert_eq!(got[1]["data"], "E");
    }

    #[test]
    fn the_first_value_of_a_field_wins_and_a_null_is_filled() {
        let got = assemble(&[
            json!([{"type":"reasoning.text","id":null,"format":"f1","index":0}]),
            json!([{"type":"reasoning.text","id":"i","format":"f2","index":0}]),
        ]);
        assert_eq!(got[0]["id"], "i");
        assert_eq!(got[0]["format"], "f1");
    }

    #[test]
    fn entries_without_an_index_stand_alone_after_the_indexed_ones() {
        let got = assemble(&[
            json!([{"type":"reasoning.text","text":"one"}]),
            json!([{"type":"reasoning.text","text":"idx","index":0}]),
            json!([{"type":"reasoning.text","text":"two"}]),
        ]);
        let texts: Vec<_> = got.iter().map(|e| e["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["idx", "one", "two"]);
    }

    #[test]
    fn anything_but_an_array_of_objects_is_ignored() {
        let got = assemble(&[
            Value::Null,
            json!("text"),
            json!({"index": 0}),
            json!([1, "x", null, [], {"type":"reasoning.text","text":"ok","index":0}]),
        ]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["text"], "ok");
    }

    #[test]
    fn a_stream_without_details_gives_no_item() {
        assert!(Assembler::default().finish().is_empty());
    }

    #[test]
    fn openai_responses_entries_get_their_own_kind() {
        let mut assembler = Assembler::default();
        assembler.push(json!([
            {"type":"reasoning.summary","summary":"s","format":"openai-responses-v1","index":0},
            {"type":"reasoning.text","text":"t","format":"anthropic-claude-v1","index":1},
            {"type":"reasoning.text","text":"u","format":"unknown","index":2},
            {"type":"reasoning.text","text":"v","index":3},
        ]));
        let kinds: Vec<_> = assembler.finish().into_iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            ["reasoning_details.openai-responses-v1", KIND, KIND, KIND]
        );
    }

    #[test]
    fn an_echoed_signature_or_data_is_scrubbed_from_a_message() {
        let long = "Ab1+/".repeat(10);
        let item = OpaqueItem {
            kind: KIND.into(),
            data: format!(r#"{{"signature":"{long}","text":"read me","data":"short"}}"#),
        };
        let message = format!(r#"bad: {{"signature":"{long}","data":"short"}}"#);
        assert_eq!(
            scrub_echoed(message, [&item].into_iter()),
            r#"bad: {"signature":"[opaque reasoning]","data":"[opaque reasoning]"}"#
        );
    }

    #[test]
    fn a_partial_echo_is_scrubbed_and_ordinary_words_stay() {
        let long = "Ab1+/".repeat(20);
        let item = OpaqueItem {
            kind: KIND.into(),
            data: format!(r#"{{"signature":"{long}"}}"#),
        };
        let cut = &long[..63];
        let message = format!("invalid request at chat/completions: {{\"signature\":\"{cut}");
        let scrubbed = scrub_echoed(message, [&item].into_iter());
        assert_eq!(
            scrubbed,
            r#"invalid request at chat/completions: {"signature":"[opaque reasoning]"#
        );
    }

    #[test]
    fn a_message_is_unchanged_when_nothing_opaque_was_sent() {
        let item = OpaqueItem {
            kind: KIND.into(),
            data: r#"{"type":"reasoning.text","text":"t"}"#.into(),
        };
        let message = format!("long token {}", "x".repeat(80));
        assert_eq!(scrub_echoed(message.clone(), [&item].into_iter()), message);
    }
}
