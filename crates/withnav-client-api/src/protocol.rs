//! Bodies of the session server's HTTP API, as a client sends and reads them.

use serde::{Deserialize, Serialize};

/// Wire capability: missing or future APIs must not become conversation choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
    Chat,
    Fim,
    SystemOne,
    #[default]
    #[serde(other)]
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    #[serde(default)]
    pub api: ApiKind,
    pub gateway: String,
    pub provider_group: String,
    pub upstream_id: String,
    /// Absent from an older server: read as no tool support.
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_parallel_tool_calls: Option<bool>,
    pub context_window: Option<u64>,
    /// Added by the catalog: absent in a payload from an older server, and
    /// then not written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// Where `context_window` came from: `models.json` or a `discovered.*` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_source: Option<String>,
    /// The provider's model list could not be refreshed and the discovered
    /// values shown are past their 24 h TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_age_secs: Option<u64>,
    /// Short, secret-free reason of the failed refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// An explicit `context_window` that differs from the provider's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discrepancy: Option<Discrepancy>,
    /// Whether the model takes images; absent when unknown or from an older server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
    /// Where `images` came from: `models.json`, `discovered.vision` or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images_source: Option<String>,
    /// The variant names the model_id offers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// The variant a new conversation starts on when none is chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_variant: Option<String>,
    /// The configured `default_variant`, when it names a variant that is not
    /// on offer now (a tier the provider does not report, or could not be
    /// asked for); `refresh_error` says why when the lookup failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_variant_unavailable: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Discrepancy {
    pub configured: u64,
    pub discovered: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsBody {
    pub models: Vec<ModelEntry>,
    pub default_agent: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Selection {
    pub model: String,
    pub variant: Option<String>,
    pub source: String,
    pub variant_source: String,
}
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentEntry {
    pub id: String,
    pub description: String,
    pub mode: String,
    pub assignment: Option<Selection>,
    pub effective: Option<Selection>,
    pub warning: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentsBody {
    pub default_agent: String,
    pub agents: Vec<AgentEntry>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerEntry {
    pub layer: String,
    pub status: String,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayersBody {
    pub layers: Vec<LayerEntry>,
}

// Local wire types keep this client independent of the server's catalog cutover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastUsage {
    pub model: String,
    pub gateway: String,
    pub upstream_id: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub step: ulid::Ulid,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub model: String,
    pub gateway: String,
    pub upstream_id: String,
    pub variant: Option<String>,
    pub window: u64,
    pub window_source: String,
    pub estimate_total: u64,
    pub breakdown: crate::event::Breakdown,
    pub output_reserve: u64,
    pub margin: u64,
    pub input_budget: u64,
    pub percent: u64,
    pub last_usage: Option<LastUsage>,
    #[serde(default)]
    pub skills: crate::event::SkillCounts,
    #[serde(default)]
    pub rule_files: u64,
    pub incarnation: ulid::Ulid,
    pub revision: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub agent: String,
    #[serde(flatten)]
    pub selection: Selection,
}
#[derive(Debug, Clone, Deserialize)]
pub struct ModelChanged {
    pub previous: Option<ModelIdentity>,
    pub new: ModelIdentity,
    pub window: u64,
    pub reason: String,
    pub operation_id: Option<ulid::Ulid>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListEntry {
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub permissions_unavailable: Option<String>,
    #[serde(default)]
    pub bypass: bool,
    #[serde(default)]
    pub bypass_seq: u64,
    #[serde(default)]
    pub incarnation: Option<String>,
    #[serde(default)]
    pub editor: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub resume_disabled_reason: Option<String>,
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Number of Captain messages.
    pub message_count: u64,
    /// The primary agent; absent from an older server.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub variant: Option<String>,
    /// Where the model choice came from; `null` when the session stored none.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub variant_source: Option<String>,
    #[serde(default)]
    pub gateway: String,
    #[serde(default)]
    pub upstream_id: String,
    #[serde(default)]
    pub current_seq: u64,
    /// True when a live, active session with this id exists in the server process.
    pub live: bool,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_status: Option<String>,
    #[serde(default = "promptable_default", skip_serializing_if = "is_promptable")]
    pub promptable: bool,
}

fn promptable_default() -> bool {
    true
}
fn is_promptable(value: &bool) -> bool {
    *value
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListBody {
    pub sessions: Vec<SessionListEntry>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// The primary agent a new conversation starts as; the server's default (build) when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The variant a new conversation starts on: absent for the model_id's default, `null` for none,
    /// else a name the model_id offers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<Option<String>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub root_actor_id: String,
    pub actor_state: String,
}
/// `GET /v1/sessions/{id}` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Captain setting `yolo`: the session runs with no permission control.
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub permissions_unavailable: Option<String>,
    #[serde(default)]
    pub bypass: bool,
    #[serde(default)]
    pub bypass_seq: u64,
    #[serde(default)]
    pub incarnation: Option<String>,
    #[serde(default)]
    pub editor: bool,
    pub session_id: String,
    pub actor_state: String,
    pub workspace: String,
    pub model: String,
    /// Where the model choice came from; `null` (or absent) when the session
    /// stored no selection provenance.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub variant_source: Option<String>,
    #[serde(default)]
    pub gateway: String,
    #[serde(default)]
    pub upstream_id: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub resume_disabled_reason: Option<String>,
    /// The primary agent; absent from an older server.
    #[serde(default)]
    pub agent: Option<String>,
    /// The model variant; `None` for none or an older server.
    #[serde(default)]
    pub variant: Option<String>,
    pub current_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default = "promptable_default", skip_serializing_if = "is_promptable")]
    pub promptable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_status: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<crate::tools::AskAnswer>,
    pub target_actor_id: String,
    pub mode: String,
    pub content: Vec<crate::command::ContentBlock>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub command_id: String,
    pub accepted: bool,
    pub actor_state: String,
    #[serde(default)]
    pub notice: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopResponse {
    pub accepted: bool,
    pub actor_state: String,
}
/// `GET /v1/health` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthBody {
    pub status: String,
}
/// The bare acknowledgement of a permission answer or a child continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedBody {
    pub accepted: bool,
}
/// `POST /v1/sessions/{id}/permissions/{request_id}` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionAnswer {
    /// `allow_once`, `allow_always` or `reject`.
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
/// One command the server runs for a session (`GET /v1/commands`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServerCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub args_hint: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandsBody {
    pub commands: Vec<ServerCommand>,
}
/// `POST /v1/sessions/{id}/commands` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub name: String,
    pub arguments: String,
}
/// `POST /v1/sessions/{id}/commands` answer; the same outcome also arrives as `command.result`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResponse {
    pub operation_id: String,
    pub ok: bool,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BypassResponse {
    pub enabled: bool,
    pub changed: bool,
    pub bypass_seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn metadata(source: Option<&str>, variant_source: Option<&str>) -> SessionMetadata {
        SessionMetadata {
            yolo: false,
            permissions_unavailable: None,
            bypass: false,
            bypass_seq: 0,
            incarnation: None,
            editor: false,
            session_id: "01J0000000000000000000000S".into(),
            actor_state: "stopped".into(),
            workspace: "/ws".into(),
            model: "gw/m".into(),
            source: source.map(str::to_owned),
            variant_source: variant_source.map(str::to_owned),
            gateway: "gw".into(),
            upstream_id: "m".into(),
            read_only: false,
            resume_disabled_reason: None,
            agent: Some("build".into()),
            variant: None,
            current_seq: 3,
            parent_session_id: None,
            promptable: true,
            child_status: None,
        }
    }

    fn round_trip(value: &SessionMetadata) -> (Value, SessionMetadata) {
        let wire = serde_json::to_value(value).expect("serialise");
        let back = serde_json::from_value(wire.clone()).expect("a client decodes the server body");
        (wire, back)
    }

    // A session without stored selection provenance is sent with `"source": null`; a client that
    // read `source` as a plain string rejected the whole metadata body (found by reading, 2026-11).
    #[test]
    fn session_metadata_round_trips_with_and_without_provenance() {
        for (source, variant_source) in [
            (Some("config"), Some("model_default")),
            (None, None),
            (Some("override"), None),
            (None, Some("override")),
        ] {
            let sent = metadata(source, variant_source);
            let (wire, back) = round_trip(&sent);
            assert_eq!(back, sent);
            let object = wire.as_object().expect("object");
            assert!(object.contains_key("source") && object.contains_key("variant_source"));
        }
    }

    #[test]
    fn session_metadata_decodes_null_or_absent_provenance_as_none() {
        let mut wire = serde_json::to_value(metadata(Some("config"), Some("override"))).unwrap();
        let object = wire.as_object_mut().unwrap();
        object.insert("source".into(), Value::Null);
        object.remove("variant_source");
        let back: SessionMetadata = serde_json::from_value(wire).expect("decodes");
        assert_eq!(back.source, None);
        assert_eq!(back.variant_source, None);
    }

    // The session body keeps every key the server always sent, so browser and editor readers see
    // the same shape; only a child link and a non-default `promptable` are omitted when unset.
    #[test]
    fn session_metadata_serialises_the_server_shape() {
        let wire = serde_json::to_value(metadata(None, None)).unwrap();
        let object = wire.as_object().unwrap();
        for key in [
            "yolo",
            "permissions_unavailable",
            "bypass",
            "bypass_seq",
            "incarnation",
            "editor",
            "read_only",
            "resume_disabled_reason",
            "agent",
            "variant",
            "source",
            "variant_source",
            "gateway",
            "upstream_id",
            "current_seq",
        ] {
            assert!(object.contains_key(key), "missing {key}: {wire}");
        }
        for key in ["parent_session_id", "child_status", "promptable"] {
            assert!(!object.contains_key(key), "unexpected {key}: {wire}");
        }
        assert_eq!(wire["source"], Value::Null);
    }

    #[test]
    fn sparse_legacy_session_metadata_still_decodes() {
        let back: SessionMetadata = serde_json::from_value(json!({
            "session_id": "S", "actor_state": "idle", "workspace": "/ws", "model": "m", "current_seq": 0
        }))
        .expect("decodes");
        assert!(back.promptable);
        assert_eq!(back.source, None);
        assert_eq!(back.agent, None);
    }
}
