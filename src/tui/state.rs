//! Data-only conversation projections (one per tab) and presentation state; no I/O or terminal
//! access.
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
    /// The outcome of a server command: its name, whether it ran, and what it answered.
    Command {
        name: String,
        ok: bool,
        text: String,
    },
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
/// The most conversations open at once; a tenth is refused.
pub const MAX_TABS: usize = 9;

/// Where the transcript of a tab was scrolled to, kept while another tab is shown.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TabView {
    pub scroll: u16,
    pub follow_bottom: bool,
    pub selected_block: Option<String>,
    pub expanded: std::collections::HashSet<String>,
}

/// One open conversation: its projection, its session, its model and what the driver left in it.
/// Every tab has its own `generation`, so the events of its stream and of its requests reach it and
/// no other tab.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tab {
    /// Stable for the life of the tab, unlike `generation`.
    pub id: u64,
    pub connection: ConnectionStatus,
    pub session: Option<super::protocol::SessionMetadata>,
    /// The conversation this tab asked the server to resume, until the answer arrives: the tab
    /// already belongs to it.
    pub resuming: Option<String>,
    pub selected_model_alias: Option<String>,
    pub selected_model_name: Option<String>,
    pub context_window: Option<u64>,
    pub conversation: Conversation,
    pub activity: Activity,
    pub last_applied_seq: Option<u64>,
    pub generation: Generation,
    pub pending_submit: Option<u64>,
    pub delivery_uncertain: bool,
    pub latest_envelope: Option<EventEnvelope>,
    pub pending_submit_text: Option<String>,
    pub pending_model_pick: bool,
    pub editor_revision: u64,
    /// The primary agent the conversation runs as, or starts as; `None` is build.
    pub agent: Option<String>,
    /// The model variant the conversation runs with, or starts with; `None` is none.
    pub variant: Option<String>,
    /// Something arrived while another tab was shown.
    pub unread: bool,
    /// The editor text while another tab is shown, and the editor revision it was saved at.
    pub draft: String,
    pub draft_revision: u64,
    /// The transcript position while another tab is shown.
    pub view: TabView,
    /// Agents started by this conversation, shown under it; empty until the server reports them.
    pub children: Vec<ChildAgent>,
}

/// The agent a conversation runs as when nothing else is known.
pub const DEFAULT_AGENT: &str = "build";
/// The other primary agent the build/plan switch goes to.
pub const PLAN_AGENT: &str = "plan";

impl Tab {
    /// The primary agent's name for display.
    pub fn agent_name(&self) -> &str {
        self.agent.as_deref().unwrap_or(DEFAULT_AGENT)
    }

    /// True when the tab holds nothing yet: no session, nothing being created or resumed, no block.
    pub fn is_blank(&self) -> bool {
        self.session.is_none()
            && self.resuming.is_none()
            && self.pending_submit_text.is_none()
            && self.conversation.blocks.is_empty()
    }
}

/// An agent working for a tab's conversation, drawn nested under it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChildAgent {
    pub name: String,
    pub activity: Activity,
}

/// The open tabs and what they share (catalog, session list, workspace). `State` reads and writes
/// the active tab through `Deref`, so code that handles one conversation stays as it was.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub tabs: Vec<Tab>,
    /// Index of the tab on screen; always valid.
    pub active: usize,
    /// Last generation or tab id handed out; never reused.
    pub issued: u64,
    pub catalog: Vec<ModelEntry>,
    pub sessions: Vec<SessionListEntry>,
    /// The commands the server runs; empty until it answered (or for a server without them).
    pub server_commands: Vec<super::protocol::ServerCommand>,
    pub last_protocol_error: Option<String>,
    pub notice: Option<String>,
    /// Workspace path sent in create requests; filled by the runtime at startup.
    pub workspace: String,
    /// Counter for ids of local blocks (`local:<n>`); never reset.
    pub local_block_seq: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            tabs: vec![Tab::default()],
            active: 0,
            issued: 0,
            catalog: Vec::new(),
            sessions: Vec::new(),
            server_commands: Vec::new(),
            last_protocol_error: None,
            notice: None,
            workspace: String::new(),
            local_block_seq: 0,
        }
    }
}

impl State {
    /// A number never handed out before, for a generation or a tab id.
    pub fn issue(&mut self) -> u64 {
        self.issued += 1;
        self.issued
    }

    /// The tab whose requests and stream are tagged with `generation`.
    pub fn tab_of(&self, generation: &Generation) -> Option<usize> {
        self.tabs.iter().position(|t| &t.generation == generation)
    }

    /// The tab showing the conversation `session_id`, or waiting for it to be resumed.
    pub fn tab_of_session(&self, session_id: &str) -> Option<usize> {
        self.tabs.iter().position(|t| {
            t.resuming.as_deref() == Some(session_id)
                || t.session
                    .as_ref()
                    .is_some_and(|m| m.session_id == session_id)
        })
    }
}

impl std::ops::Deref for State {
    type Target = Tab;
    fn deref(&self) -> &Tab {
        &self.tabs[self.active]
    }
}

impl std::ops::DerefMut for State {
    fn deref_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }
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
    /// The tab sidebar is hidden by the driver (it also hides itself on narrow terminals).
    pub sidebar_hidden: bool,
}
