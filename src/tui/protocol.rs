//! Serialized HTTP/SSE contracts and messages exchanged by the TUI runtime; no transport execution.
use crate::bus::event::EventEnvelope;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Generation(pub u64);
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
    pub breakdown: crate::bus::event::Breakdown,
    pub output_reserve: u64,
    pub margin: u64,
    pub input_budget: u64,
    pub percent: u64,
    pub last_usage: Option<LastUsage>,
    #[serde(default)]
    pub skills: crate::bus::event::SkillCounts,
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
    pub answer: Option<crate::tools::control::AskAnswer>,
    pub target_actor_id: String,
    pub mode: String,
    pub content: Vec<crate::bus::command::ContentBlock>,
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
/// Why a permission answer was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// The request is no longer waiting (answered, cancelled, expired).
    Stale,
    Other(String),
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
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    BypassSet {
        generation: Generation,
        result: Result<BypassResponse, String>,
    },
    ConfigLoaded {
        generation: Generation,
        request: u64,
        result: Result<super::config::SettingsBody, String>,
    },
    ConfigWritten {
        generation: Generation,
        key: String,
        value: serde_json::Value,
        result: Result<super::config::SettingsBody, super::config::SettingsError>,
    },
    GatewayTested {
        generation: Generation,
        name: String,
        result: Result<super::config::GatewayTest, String>,
    },
    Agents {
        generation: Generation,
        workspace: String,
        request: u64,
        session_id: Option<String>,
        result: Result<AgentsBody, String>,
    },
    Layers {
        generation: Generation,
        workspace: String,
        request: u64,
        result: Result<LayersBody, String>,
    },
    Skills {
        generation: Generation,
        result: Result<crate::bus::event::SkillsBody, String>,
    },
    ChildEnvelope {
        generation: Generation,
        child_session_id: String,
        envelope: EventEnvelope,
    },
    Envelope {
        generation: Generation,
        envelope: EventEnvelope,
    },
    Resync {
        generation: Generation,
    },
    StreamConnected {
        generation: Generation,
    },
    StreamDisconnected {
        generation: Generation,
    },
    Reconnecting {
        generation: Generation,
    },
    Models {
        generation: Generation,
        workspace: String,
        request: u64,
        result: Result<ModelsBody, String>,
    },
    Sessions {
        generation: Generation,
        result: Result<SessionListBody, String>,
    },
    SessionDeleted {
        generation: Generation,
        session_id: String,
        result: Result<bool, String>,
    },
    Created {
        generation: Generation,
        result: Result<CreateSessionResponse, String>,
    },
    Resumed {
        generation: Generation,
        result: Result<CreateSessionResponse, String>,
    },
    Metadata {
        generation: Generation,
        result: Result<SessionMetadata, String>,
    },
    Context {
        generation: Generation,
        /// The conversation's context epoch when the fetch was issued.
        epoch: u64,
        result: Result<ContextSnapshot, String>,
    },
    Submitted {
        generation: Generation,
        revision: u64,
        result: Result<SubmitResponse, String>,
    },
    /// A permission answer came back.
    PermissionAnswered {
        generation: Generation,
        request_id: String,
        result: Result<(), AnswerError>,
    },
    QuestionAnswered {
        generation: Generation,
        call_id: String,
        result: Result<SubmitResponse, String>,
    },
    /// The server's command list.
    Commands {
        generation: Generation,
        result: Result<CommandsBody, String>,
    },
    /// A command request came back; `revision` is the editor revision it was typed at, if typed;
    /// `token` is the one its `Effect::RunCommand` carried.
    CommandRan {
        generation: Generation,
        name: String,
        revision: Option<u64>,
        token: u64,
        result: Result<CommandResponse, String>,
    },
    Stopped {
        generation: Generation,
        result: Result<StopResponse, String>,
    },
    ChildControlled {
        generation: Generation,
        child_session_id: String,
        control: ChildControl,
        result: Result<(), String>,
        pause_token: Option<crate::bus::event::PauseToken>,
    },
    DeliveryUncertain {
        generation: Generation,
    },
    ProtocolError {
        generation: Generation,
        message: String,
    },
    /// The workspace's git state was read; `None` when it is not a repository.
    Git {
        generation: Generation,
        status: Option<crate::workspace::git::GitStatus>,
    },
    /// Saving the user settings failed; the change still applies until the client quits.
    SettingsNotSaved {
        generation: Generation,
        message: String,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    NextTurn,
    Steer,
    Interrupt,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    ToggleBypass,
    OpenConfig,
    Config(super::config::ConfigAction),
    PickAgent(String),
    Submit(String),
    OpenOverlay(Overlay),
    CloseOverlay,
    Select(usize),
    NewConversation,
    ResumeSession(String),
    RequestDeleteSession(String),
    ConfirmDeleteSession,
    CancelDeleteSession,
    SessionDeleted {
        session_id: String,
        result: Result<bool, String>,
    },
    OpenChild(String),
    ContinueChild,
    ConfirmChildControl,
    CancelChildControl,
    PickModel(String),
    /// Moves the active conversation to `model_id` (`/model <model_id>`); on a tab with no
    /// conversation yet, the conversation will start on it.
    SwitchModel(String),
    Stop,
    Quit,
    Scroll(i32),
    ToggleBlock(String),
    SetMode(Mode),
    /// Draws with this theme while the picker is open, without saving it.
    PreviewTheme(crate::tui::theme::ThemeId),
    /// Keeps this theme and saves it in the settings.
    PickTheme(crate::tui::theme::ThemeId),
    /// Saves the favorite models (already updated in the UI state) in the settings.
    SaveFavoriteModels(Vec<String>),
    /// Submit with the editor revision the text was read at; `Submit(t)` is `SubmitAt { t, 0 }`.
    SubmitAt {
        text: String,
        revision: u64,
    },
    /// Runs a server command in the active conversation; `revision` is the editor revision when
    /// it was typed, so the editor is cleared once the server took it.
    RunCommand {
        name: String,
        arguments: String,
        revision: Option<u64>,
    },
    /// A row of the variant list: a variant name, or `off` for none. What it applies to is the
    /// list's `UiState::variant_pick`.
    PickVariant(String),
    /// Answers the open permission request of the active tab.
    AnswerPermission {
        request_id: String,
        answer: PermissionAnswer,
    },
    QuestionMove(i32),
    QuestionToggle,
    QuestionDismiss,
    QuestionCustom,
    AnswerQuestion(crate::tools::control::AskAnswer),
    /// Shows a notice in the active conversation; nothing else happens.
    ShowNotice(String),
    /// Switches the active conversation between the build and plan agents.
    ToggleAgent,
    /// Shows the tab at this index (0 is the first).
    SwitchTab(usize),
    /// Closes the active tab; its conversation stays on the server.
    CloseTab,
    /// Hides or shows the tab sidebar.
    ToggleSidebar,
    /// Hides or shows the right column.
    TogglePanel,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Agents,
    Skills,
    Models,
    Sessions,
    Commands,
    Help,
    Themes,
    /// The variants of a model: the second step of the model picker, or `/variant`.
    Variants,
    Children,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildControl {
    Stop,
    Continue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    AnswerQuestion {
        session_id: String,
        answer: crate::tools::control::AskAnswer,
    },
    SetBypass {
        session_id: String,
        enabled: bool,
    },
    FetchConfig(u64),
    WriteConfig(super::config::WriteRequest),
    TestGateway(String),
    ApplyConfigTheme(crate::tui::theme::ThemeId),
    LeaveConfig,
    FetchAgents(Option<String>),
    FetchLayers,
    FetchSkills(String),
    WatchChild {
        parent_session_id: String,
        tab_id: u64,
        generation: Generation,
        child_session_id: String,
    },
    UnwatchChild(String),
    FetchModels,
    ListSessions,
    DeleteSession(String),
    /// Reads the server's command list.
    FetchCommands,
    /// Sends a decision on a permission request; the outcome comes back as
    /// `ClientEvent::PermissionAnswered`.
    AnswerPermission {
        session_id: String,
        request_id: String,
        answer: PermissionAnswer,
    },
    /// Runs a command in a session; the outcome comes back as `ClientEvent::CommandRan`, with
    /// the same `token` (the tab's own number for the request, so an answer finds its request).
    RunCommand {
        session_id: String,
        request: CommandRequest,
        revision: Option<u64>,
        token: u64,
    },
    Create(CreateSessionRequest),
    Resume(String),
    FetchMetadata(String),
    /// Briefly retry the persisted outcome after a child's terminal stream event.
    FetchChildTerminalMetadata(String),
    /// Reads the session's context snapshot; `epoch` is echoed in `ClientEvent::Context`.
    FetchContext {
        session_id: String,
        epoch: u64,
    },
    Subscribe {
        session_id: String,
        cursor: Option<u64>,
    },
    Unsubscribe,
    Send(SubmitRequest),
    Stop(String),
    ControlChild {
        parent_session_id: String,
        child_session_id: String,
        control: ChildControl,
        pause_token: Option<crate::bus::event::PauseToken>,
    },
    Quit,
    /// Writes the theme id to the user settings.
    SaveTheme(crate::tui::theme::ThemeId),
    /// Writes the favorite model names to the user settings.
    SaveFavoriteModels(Vec<String>),
    /// Reads the workspace's git state in the background; it comes back as `ClientEvent::Git`.
    ReadGit,
    /// Clears the editor if it still holds the text sent at `revision`.
    ClearEditor {
        revision: u64,
    },
    /// Send `request`; the runtime echoes `revision` in `ClientEvent::Submitted` so the editor is
    /// only cleared if it was not edited since. Replaces `Send`, which the reducer no longer emits.
    SendAt {
        request: SubmitRequest,
        revision: u64,
    },
    /// Opens the variant list of the active conversation, from the offer the server just gave.
    ShowVariants {
        /// The variant in use according to that same answer; the list starts on it.
        current: Option<String>,
    },
    /// Puts the draft of the tab that came forward in the editor, at the revision it was kept at,
    /// so a send still in flight from that tab can clear it.
    SetEditor {
        text: String,
        revision: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("protocol error: {0}")]
pub struct ProtocolError(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BypassResponse {
    pub enabled: bool,
    pub changed: bool,
    pub bypass_seq: u64,
}
