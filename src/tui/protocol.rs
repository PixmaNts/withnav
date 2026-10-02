//! Serialized HTTP/SSE contracts and messages exchanged by the TUI runtime; no transport execution.
use crate::bus::event::EventEnvelope;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Generation(pub u64);
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub alias: String,
    pub model: String,
    pub context_window: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsBody {
    pub models: Vec<ModelEntry>,
    pub default: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u64,
    pub live: bool,
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
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub root_actor_id: String,
    pub actor_state: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub session_id: String,
    pub actor_state: String,
    pub workspace: String,
    pub model: String,
    pub current_seq: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitRequest {
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
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
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
        result: Result<ModelsBody, String>,
    },
    Sessions {
        generation: Generation,
        result: Result<SessionListBody, String>,
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
    Submitted {
        generation: Generation,
        revision: u64,
        result: Result<SubmitResponse, String>,
    },
    Stopped {
        generation: Generation,
        result: Result<StopResponse, String>,
    },
    DeliveryUncertain {
        generation: Generation,
    },
    ProtocolError {
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
    Submit(String),
    OpenOverlay(Overlay),
    CloseOverlay,
    Select(usize),
    NewConversation,
    ResumeSession(String),
    PickModel(String),
    Stop,
    Quit,
    Scroll(i32),
    ToggleBlock(String),
    SetMode(Mode),
    /// Draws with this theme while the picker is open, without saving it.
    PreviewTheme(crate::tui::theme::ThemeId),
    /// Keeps this theme and saves it in the settings.
    PickTheme(crate::tui::theme::ThemeId),
    /// Submit with the editor revision the text was read at; `Submit(t)` is `SubmitAt { t, 0 }`.
    SubmitAt {
        text: String,
        revision: u64,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Models,
    Sessions,
    Commands,
    Help,
    Themes,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    FetchModels,
    ListSessions,
    Create(CreateSessionRequest),
    Resume(String),
    FetchMetadata(String),
    Subscribe {
        session_id: String,
        cursor: Option<u64>,
    },
    Unsubscribe,
    Send(SubmitRequest),
    Stop(String),
    Quit,
    /// Writes the theme id to the user settings.
    SaveTheme(crate::tui::theme::ThemeId),
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
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("protocol error: {0}")]
pub struct ProtocolError(pub String);
