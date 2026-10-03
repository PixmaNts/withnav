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
    /// A gated tool call waiting for the driver's decision, then how it ended.
    Permission(PermissionPrompt),
    /// The outcome of a server command: its name, whether it ran, and what it answered.
    Command {
        name: String,
        ok: bool,
        text: String,
    },
}
/// What a permission request asks and, once it ended, how.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionPrompt {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool: String,
    /// The call's arguments as JSON text, cut by the server to 4 KiB.
    pub arguments: String,
    /// `folder` or `origin`.
    pub scope_kind: String,
    /// The folder path or the `scheme://host[:port]` origin.
    pub scope: String,
    pub agent: String,
    pub reason: String,
    /// `None` while open; then `allow_once`, `allow_folder`, `deny`, `cancelled` or `expired`.
    pub resolution: Option<String>,
    /// What a denial told the model.
    pub message: Option<String>,
    /// The server said the request was no longer waiting (409) before its `permission.resolved`
    /// arrived: it takes no answer, and how it ended is not known yet.
    pub stale: bool,
}

impl PermissionPrompt {
    /// Whether the request still waits for an answer: every "open" check goes through here.
    pub fn is_open(&self) -> bool {
        self.resolution.is_none() && !self.stale
    }
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
    /// Block ids of the permission requests still open, oldest first.
    pub open_permissions: Vec<String>,
    /// Key of the last `llm.response` seen (the turn-end usage fallback).
    pub last_response_step: Option<(String, String, String)>,
    /// The last `context.compacted` of the conversation, if any.
    pub last_compaction: Option<crate::bus::event::ContextCompacted>,
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
    /// Metadata must establish whether this session accepts prompts before a resume is sent.
    pub inspecting: bool,
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
    /// The variants the conversation's server offers (`/variant` answered `offered`); `None` until
    /// asked. The `/variant` list shows these, not the catalog's.
    pub variant_offer: Option<Vec<String>>,
    /// `/variant` was asked to open its list: the server's answer opens it.
    pub variant_list_wanted: bool,
    /// The seq of the last event that set `agent` / `variant`: a session summary read at an
    /// older seq does not overwrite them.
    pub agent_seq: Option<u64>,
    pub variant_seq: Option<u64>,
    /// The variant chosen in the model picker for a conversation not created yet: `None` leaves
    /// the alias's default, `Some(None)` asks for none.
    pub start_variant: Option<Option<String>>,
    /// The last `/model` this client sent, from the send until the server settles it: the header
    /// shows `→ alias (after this turn)` once it is accepted for after the running turn.
    pub model_switch: Option<ModelSwitch>,
    /// The last token given to a command request of this tab.
    pub command_token: u64,
    /// The `/model` operations the stream reported as settled (done, refused, cancelled, or
    /// carried by a `model.changed`), the last few: an HTTP answer arriving after its own result
    /// does not mark a settled switch as waiting.
    pub settled_model_ops: std::collections::VecDeque<String>,
    /// The seq of the last `model.changed`: a session summary read at an older seq does not
    /// overwrite the model.
    pub model_seq: Option<u64>,
    /// The request an answer was sent for, until `permission.resolved` ends it (or the answer
    /// did not reach the server): its keys are ignored meanwhile, another request's are not.
    pub answered_permission: Option<String>,
    /// Something arrived while another tab was shown.
    pub unread: bool,
    /// The editor text while another tab is shown, and the editor revision it was saved at.
    pub draft: String,
    pub draft_revision: u64,
    /// The transcript position while another tab is shown.
    pub view: TabView,
    /// Agents started by this conversation, shown under it; empty until the server reports them.
    pub children: Vec<ChildAgent>,
    /// Read-only transcript opened from a parent's reported child link.
    pub parent_session_id: Option<String>,
    pub child_status: ChildStatus,
    pub child_status_seq: Option<u64>,
    /// The workspace's branch and changes at the last read; `None` until read, or when the
    /// workspace is not a git repository.
    pub git: Option<super::git::GitStatus>,
    /// A read of the git state is running; `git_again` asks for one more when it returns.
    pub git_reading: bool,
    pub git_again: bool,
}

/// The agent a conversation runs as when nothing else is known.
pub const DEFAULT_AGENT: &str = "build";
/// The other primary agent the build/plan switch goes to.
pub const PLAN_AGENT: &str = "plan";

impl Tab {
    pub fn read_only(&self) -> bool {
        self.parent_session_id.is_some()
    }

    /// The oldest permission request still open, if any.
    pub fn open_permission(&self) -> Option<&PermissionPrompt> {
        self.conversation.open_permissions.iter().find_map(|id| {
            self.conversation.blocks.iter().find_map(|b| match &b.kind {
                BlockKind::Permission(p) if &b.id == id && p.is_open() => Some(p),
                _ => None,
            })
        })
    }

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

/// A `/model` this client sent and the server has not settled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelSwitch {
    pub alias: String,
    /// The token of the request that asked for it: only its HTTP answer binds the operation.
    pub token: u64,
    /// The operation the server gave the command; `None` until its HTTP answer.
    pub operation_id: Option<String>,
    /// The server accepted it for after the running turn.
    pub deferred: bool,
}

impl Tab {
    /// The alias a switch accepted for after this turn goes to, while it waits.
    pub fn pending_model(&self) -> Option<&str> {
        self.model_switch
            .as_ref()
            .filter(|m| m.deferred)
            .map(|m| m.alias.as_str())
    }
}

/// An agent working for a tab's conversation, drawn nested under it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChildAgent {
    pub session_id: String,
    pub name: String,
    pub goal: String,
    pub status: ChildStatus,
    pub delivery: Option<String>,
    pub reason: Option<String>,
    /// A sequenced parent event takes precedence over a session-list snapshot.
    pub event_seq: Option<u64>,
    pub child_seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChildStatus {
    #[default]
    Unknown,
    Running,
    Permission,
    OutOfSteps,
    Doubt,
    Done,
    Failed,
    Stopped,
}

impl ChildStatus {
    pub fn project(status: &mut Self, seq: &mut Option<u64>, event: &EventEnvelope) {
        if status.terminal() || seq.is_some_and(|held| event.seq <= held) {
            return;
        }
        *seq = Some(event.seq);
        *status = match event.kind.as_str() {
            "turn.start" => Self::Running,
            "agent.yield" if event.payload["kind"] == "budget" => Self::OutOfSteps,
            "permission.requested" => Self::Permission,
            "permission.resolved" => Self::Running,
            _ => *status,
        };
    }
    pub fn from_wire(status: &str) -> Self {
        match status {
            "running" | "active" => Self::Running,
            "waiting_for_permission" | "permission" => Self::Permission,
            "out_of_steps" | "step_budget_exhausted" | "parked" => Self::OutOfSteps,
            "doubt" => Self::Doubt,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "—",
            Self::Running => "running",
            Self::Permission => "waiting for permission",
            Self::OutOfSteps => "out of steps",
            Self::Doubt => "in doubt",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Stopped)
    }
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
    pub fn active_child_status(&self) -> ChildStatus {
        self.parent_session_id
            .as_deref()
            .and_then(|id| self.tab_of_session(id))
            .and_then(|i| {
                self.tabs[i].children.iter().find(|c| {
                    self.session
                        .as_ref()
                        .map(|m| m.session_id.as_str())
                        .or(self.resuming.as_deref())
                        == Some(c.session_id.as_str())
                })
            })
            .map(|c| c.status)
            .unwrap_or(self.child_status)
    }
    pub fn child_rows(&self, filter: &str) -> Vec<&ChildAgent> {
        let parent = self.parent_session_id.as_deref();
        let tab = parent
            .and_then(|id| self.tab_of_session(id))
            .map(|i| &self.tabs[i])
            .unwrap_or(self);
        let filter = filter.to_lowercase();
        tab.children
            .iter()
            .filter(|c| {
                format!("{} {} {}", c.name, c.goal, c.session_id)
                    .to_lowercase()
                    .contains(&filter)
            })
            .collect()
    }

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
/// What a choice in the variant list applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantPick {
    /// The second step of the model picker: the variant a new conversation on `alias` starts on.
    NewConversation(String),
    /// `/variant`: the active conversation's variant, changed through the server.
    Current,
}

/// The word for "no variant" in the list and in `/variant off`.
pub const NO_VARIANT: &str = "off";

/// A deny message being typed, for one permission request only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DenyDraft {
    pub request_id: String,
    pub text: String,
}

impl UiState {
    /// The deny message typed for `request_id`, if that is the request it was started on.
    pub fn deny_draft(&self, request_id: &str) -> Option<&str> {
        self.deny_message
            .as_ref()
            .filter(|d| d.request_id == request_id)
            .map(|d| d.text.as_str())
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
    pub child_confirm: Option<(String, String, super::protocol::ChildControl)>,
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
    /// Enter was pressed once on a message while this permission request (id, editor revision)
    /// was open: a second enter on the same text sends it.
    pub send_over_permission: Option<(String, u64)>,
    /// The message being typed after `n` on a permission prompt, with the request it denies;
    /// `None` when not denying.
    pub deny_message: Option<DenyDraft>,
    /// The command palette was opened by typing `/` in the editor: the editor holds the line and
    /// the filter follows it.
    pub palette_typed: bool,
    /// What the variant list is choosing for, while it is open.
    pub variant_pick: Option<VariantPick>,
    /// The tab sidebar is hidden by the driver (it also hides itself on narrow terminals).
    pub sidebar_hidden: bool,
    /// The right column: `None` follows the terminal width, `Some` is the driver's choice.
    pub panel_shown: Option<bool>,
    /// Whether the right column was on screen at the last draw: the toggle flips what was seen.
    pub panel_drawn: bool,
    /// The open permission request whose keys were on screen at the last draw. For any other
    /// (scrolled away, or not drawn yet), the first `y`, `a` or `n` brings it into view instead
    /// of answering it.
    pub prompt_shown: Option<String>,
}
