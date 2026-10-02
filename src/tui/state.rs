//! Data-only conversation projection and presentation state; no I/O or terminal access.
use super::protocol::{Generation, ModelEntry, SessionListEntry};
use crate::{bus::event::EventEnvelope, provider::Usage, tools::ToolStatus};
use serde_json::Value;
use std::collections::HashMap;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Activity {
    #[default]
    Idle,
    Running,
    AwaitingUser,
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}
#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Message {
        source: String,
        text: String,
    },
    AssistantStep {
        text: String,
        thinking: String,
        revision: u64,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
        status: Option<ToolStatus>,
        output: String,
        metadata: Value,
        duration_ms: Option<u64>,
        exit_code: Option<i32>,
    },
    YieldQuestion(String),
    Error(String),
    Notice(String),
}
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub id: String,
    pub kind: BlockKind,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Conversation {
    pub blocks: Vec<Block>,
    pub steps: HashMap<(String, String, String), String>,
    pub tools_by_turn: HashMap<(String, String), Vec<String>>,
    pub usage_by_step: HashMap<(String, String, String), Usage>,
    pub context_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub session_tokens: Option<u64>,
    pub missing_usage: bool,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct State {
    pub connection: ConnectionStatus,
    pub session: Option<super::protocol::SessionMetadata>,
    pub selected_model_alias: Option<String>,
    pub selected_model_name: Option<String>,
    pub context_window: Option<u64>,
    pub catalog: Vec<ModelEntry>,
    pub sessions: Vec<SessionListEntry>,
    pub conversation: Conversation,
    pub activity: Activity,
    pub last_applied_seq: Option<u64>,
    pub generation: Generation,
    pub pending_submit: Option<u64>,
    pub delivery_uncertain: bool,
    pub last_protocol_error: Option<String>,
    pub latest_envelope: Option<EventEnvelope>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Editor,
    Transcript,
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UiState {
    pub focus: Focus,
    pub overlay: Option<super::protocol::Overlay>,
    pub selection: HashMap<String, usize>,
    pub scroll: u16,
    pub follow_bottom: bool,
    pub selected_block: Option<String>,
    pub expanded: std::collections::HashSet<String>,
    pub mode: super::protocol::Mode,
    pub quit_confirm: bool,
    pub tick: u64,
    pub filter: String,
}
