//! Serialized HTTP/SSE contracts and messages exchanged by the TUI runtime; no transport execution.
use crate::bus::event::EventEnvelope;
pub use withnav_client_api::protocol::{
    AgentEntry, AgentsBody, ApiKind, BypassResponse, CommandRequest, CommandResponse, CommandsBody,
    ContextSnapshot, CreateSessionRequest, CreateSessionResponse, Discrepancy, LastUsage,
    LayerEntry, LayersBody, ModelChanged, ModelEntry, ModelIdentity, ModelsBody, PermissionAnswer,
    Selection, ServerCommand, SessionListBody, SessionListEntry, SessionMetadata, StopResponse,
    SubmitRequest, SubmitResponse,
};
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Generation(pub u64);
pub use withnav_client::client::AnswerError;
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
    Mcp {
        generation: Generation,
        result: Result<crate::bus::event::McpStatus, String>,
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
    /// Reads `GET /v1/mcp`: at start, and on every stream (re)connection, since the
    /// `mcp.status` event is live-only.
    FetchMcp,
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
pub use withnav_client_state::ProtocolError;
