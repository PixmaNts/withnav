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
    pub context_window: Option<u64>,
    /// Added by the catalog: absent in a payload from an older server, and
    /// then not written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_age_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discrepancy: Option<Discrepancy>,
    /// The variant names the model_id offers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// The variant a new conversation starts on when none is chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_variant: Option<String>,
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
    #[serde(default, skip_serializing_if = "is_false")]
    pub bypass: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bypass_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_disabled_reason: Option<String>,
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u64,
    /// The primary agent; absent from an older server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
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
fn is_false(value: &bool) -> bool {
    !value
}
fn is_zero(value: &u64) -> bool {
    *value == 0
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    #[serde(default, skip_serializing_if = "is_false")]
    pub yolo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions_unavailable: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bypass: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bypass_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<String>,
    pub session_id: String,
    pub actor_state: String,
    pub workspace: String,
    pub model: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub variant_source: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub resume_disabled_reason: Option<String>,
    /// The primary agent; absent from an older server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The model variant; `None` for none or an older server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub current_seq: u64,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default = "promptable_default")]
    pub promptable: bool,
    #[serde(default)]
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
