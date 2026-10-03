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
    /// The server's estimate of the next request, from `context.updated` or
    /// `GET /context`; the status line shows it. `context_tokens` stays the
    /// provider's last prompt count.
    pub context: Option<crate::bus::event::ContextSnapshot>,
    /// Bumped on every resume and every stream (re)connection: a context fetch answered
    /// under an older epoch is dropped.
    pub context_epoch: u64,
    /// The runtime incarnations the gauge has moved past (the last few): a snapshot of one
    /// of them is never accepted again.
    pub retired_incarnations: std::collections::VecDeque<ulid::Ulid>,
    pub completion_tokens: Option<u64>,
    pub session_tokens: Option<u64>,
    pub missing_usage: bool,
    pub current_turn: Option<String>,
    pub current_step: Option<(String, String, String)>,
    pub step_blocks: HashMap<(String, String, String), String>,
    pub step_turns: HashMap<(String, String, String), String>,
    pub counted_steps: std::collections::HashSet<(String, String, String)>,
    pub pending_tools: HashMap<String, String>,
    pub pending_tool_fragments: HashMap<String, String>,
    /// Key of the last `llm.response` seen (the turn-end usage fallback).
    pub last_response_step: Option<(String, String, String)>,
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
    pub pending_submit_text: Option<String>,
    pub pending_model_pick: bool,
    pub editor_revision: u64,
    pub notice: Option<String>,
    /// Workspace path sent in create requests; filled by the runtime at startup.
    pub workspace: String,
    /// Counter for ids of local blocks (`local:<n>`); never reset.
    pub local_block_seq: u64,
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
    /// The theme the screen is drawn with; while the theme picker is open, the one previewed.
    pub theme: crate::tui::theme::ThemeId,
    /// The theme in effect when the theme picker opened; `esc` goes back to it.
    pub theme_on_open: Option<crate::tui::theme::ThemeId>,
}
