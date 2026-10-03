//! Delivered outbox text is also a durable message on the owner's stream.
use serde_json::{json, Value};
use ulid::Ulid;

use crate::bus::event::{EventKind, ROOT_ACTOR_ID};
use crate::bus::sequencer::Candidate;

pub(crate) fn candidate(notice: Ulid, payload: &Value, content: &str) -> Candidate {
    let mut message = json!({
        "command_id": notice,
        "source": "subagent",
        "target": ROOT_ACTOR_ID,
        "mode": match payload["delivery"].as_str() {
            Some("steer") => "steer", Some("interrupt") => "interrupt", _ => "next-turn",
        },
        "content": content,
        "child_session_id": payload["child_session_id"],
        "agent": payload["agent"],
        "outcome": payload["outcome"],
    });
    if payload["notice_kind"] == "watchdog" || payload["reason"] == "watchdog" {
        message["watchdog"] = json!(true);
        if let Some(verdict) = payload.get("verdict") {
            message["verdict"] = verdict.clone();
        }
    }
    Candidate::new(EventKind::MessageReceived, message)
}
