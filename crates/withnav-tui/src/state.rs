//! Data-only conversation projections (one per tab) and presentation state; no I/O or terminal
//! access.
use super::protocol::{Generation, ModelEntry, SessionListEntry};
use std::collections::HashMap;
pub use withnav_client_state::{
    Activity, Block, BlockKind, ChildAgent, ChildStatus, Conversation, DEFAULT_AGENT, ModelSwitch,
    PermissionPrompt, QuestionPrompt, SessionState, SkillScript,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
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

/// One open conversation: its projection, its session, its model and what the Captain left in it.
/// Every tab has its own `generation`, so the events of its stream and of its requests reach it and
/// no other tab.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tab {
    /// What the event stream says about the conversation. The tab reads and writes it through
    /// `Deref`; everything else in the tab is terminal-side.
    pub projection: SessionState,
    /// `/variant` was asked to open its list: the server's answer opens it.
    pub variant_list_wanted: bool,
    pub bypass_pending: bool,
    pub default_agent: Option<String>,
    pub agents: Vec<super::protocol::AgentEntry>,
    pub agents_request: u64,
    pub start_model: Option<String>,
    pub skills: withnav_client_api::event::SkillsBody,
    /// Stable for the life of the tab, unlike `generation`.
    pub id: u64,
    pub connection: ConnectionStatus,
    pub session: Option<super::protocol::SessionMetadata>,
    /// The conversation this tab asked the server to resume, until the answer arrives: the tab
    /// already belongs to it.
    pub resuming: Option<String>,
    /// Metadata must establish whether this session accepts prompts before a resume is sent.
    pub inspecting: bool,
    pub generation: Generation,
    pub pending_submit: Option<u64>,
    pub delivery_uncertain: bool,
    pub pending_submit_text: Option<String>,
    /// Images attached with `/attach`, sent with the next message and shown as chips under the
    /// message box. They leave only when the server acknowledged the message that carried them.
    pub pending_attachments: Vec<PendingAttachment>,
    /// How many of the first `pending_attachments` ride in a message the server has not
    /// acknowledged yet. Another send is refused meanwhile (it would carry the same images
    /// twice); an `/attach` made meanwhile lands after them and stays for the next message.
    pub attachments_sending: usize,
    pub pending_model_pick: bool,
    pub editor_revision: u64,
    /// The variant chosen in the model picker for a conversation not created yet: `None` leaves
    /// the model_id's default, `Some(None)` asks for none.
    pub start_variant: Option<Option<String>>,
    /// The last token given to a command request of this tab.
    pub command_token: u64,
    /// Something arrived while another tab was shown.
    pub unread: bool,
    /// The editor text while another tab is shown, and the editor revision it was saved at.
    pub draft: String,
    pub draft_revision: u64,
    /// The transcript position while another tab is shown.
    pub view: TabView,
    /// Read-only transcript opened from a parent's reported child link.
    pub parent_session_id: Option<String>,
    pub child_status: ChildStatus,
    pub child_status_seq: Option<u64>,
    /// The workspace's branch and changes at the last read; `None` until read, or when the
    /// workspace is not a git repository.
    pub git: Option<crate::git::GitStatus>,
    /// A read of the git state is running; `git_again` asks for one more when it returns.
    pub git_reading: bool,
    pub git_again: bool,
}

/// The most an image sent in one message may weigh: 3.75 MiB, so that once base64-encoded it stays
/// under 5 MB, the lowest per-image limit among the providers a gateway can route to. The server
/// refuses more; checking here spares the upload and tells the Captain at `/attach` time.
pub const MAX_ATTACHMENT_BYTES: u64 = 3_932_160;
/// The most images one message may carry; the server refuses more.
pub const MAX_ATTACHMENTS: usize = 8;

/// An image the Captain attached with `/attach`, still on disk: it is uploaded to the session
/// only when the message is sent, because a conversation is created on the first message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttachment {
    /// Absolute path read at send time.
    pub path: std::path::PathBuf,
    /// The file's base name, as shown in the chip and sent to the server.
    pub name: String,
    /// Size on disk when it was attached.
    pub size: u64,
}

impl PendingAttachment {
    /// `[image shot.png, 120 KB]`
    pub fn chip(&self) -> String {
        format!(
            "[image {}, {}]",
            self.name,
            withnav_client_api::attachment::human_size(self.size)
        )
    }
}

/// The other primary agent the build/plan switch goes to.
pub const PLAN_AGENT: &str = "plan";

impl Tab {
    pub fn archive(&self) -> bool {
        self.parent_session_id.is_none()
            && self
                .session
                .as_ref()
                .is_some_and(|m| m.read_only || !m.promptable)
    }
    pub fn read_only(&self) -> bool {
        self.parent_session_id.is_some()
            || self
                .session
                .as_ref()
                .is_some_and(|m| !m.promptable || m.read_only)
    }

    /// The primary agent's name for display.
    pub fn agent_name(&self) -> &str {
        self.projection.agent_name_or(self.default_agent.as_deref())
    }

    /// True when the tab holds nothing yet: no session, nothing being created or resumed, no block.
    pub fn is_blank(&self) -> bool {
        self.session.is_none()
            && self.resuming.is_none()
            && self.pending_submit_text.is_none()
            && self.conversation.blocks.is_empty()
    }
}

impl std::ops::Deref for Tab {
    type Target = SessionState;
    fn deref(&self) -> &SessionState {
        &self.projection
    }
}

impl std::ops::DerefMut for Tab {
    fn deref_mut(&mut self) -> &mut SessionState {
        &mut self.projection
    }
}

/// The open tabs and what they share (catalog, session list, workspace). `State` reads and writes
/// the active tab through `Deref`, so code that handles one conversation stays as it was.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub config: super::config::ConfigState,
    pub models_request: u64,
    pub layers_request: u64,
    pub layers: Vec<super::protocol::LayerEntry>,
    /// The MCP servers of the service, the newest snapshot read or heard; `None` until one
    /// arrives. Shared by every tab: the servers are the service's, not a conversation's.
    pub mcp: Option<withnav_client_api::event::McpStatus>,
    pub default_agent: String,
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
    /// The home folder, shown as `~` in paths; `None` when unknown or not valid Unicode.
    pub home: Option<String>,
    /// Counter for ids of local blocks (`local:<n>`); never reset.
    pub local_block_seq: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            config: Default::default(),
            models_request: 0,
            layers_request: 0,
            layers: vec![],
            mcp: None,
            default_agent: DEFAULT_AGENT.into(),
            tabs: vec![Tab::default()],
            active: 0,
            issued: 0,
            catalog: Vec::new(),
            sessions: Vec::new(),
            server_commands: Vec::new(),
            last_protocol_error: None,
            notice: None,
            workspace: String::new(),
            home: None,
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
    /// The second step of the model picker: the variant a new conversation on `model_id` starts on.
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
    /// Screen routing: `Config` replaces the session transcript; overlays remain shared.
    pub screen: super::config::Screen,
    pub settings_target: Option<super::config::SettingsTarget>,
    pub collapsed_models: std::collections::HashSet<String>,
    /// Models the Captain marked as favorites, saved in the settings. Kept even when a model is
    /// no longer in the catalogue; the picker only lists those it knows.
    pub favorite_models: std::collections::BTreeSet<String>,
    pub child_confirm: Option<(String, String, super::protocol::ChildControl)>,
    pub delete_confirm: Option<String>,
    pub delete_pending: Option<String>,
    pub child_confirm_token: Option<withnav_client_api::event::PauseToken>,
    pub focus: Focus,
    pub overlay: Option<super::protocol::Overlay>,
    pub selection: HashMap<String, usize>,
    pub scroll: u16,
    pub follow_bottom: bool,
    pub selected_block: Option<String>,
    pub expanded: std::collections::HashSet<String>,
    pub mode: super::protocol::Mode,
    /// The key that asked to quit and must be pressed again to confirm.
    pub quit_confirm: Option<&'static str>,
    pub tick: u64,
    pub filter: String,
    /// The theme the screen is drawn with; while the theme picker is open, the one previewed.
    pub theme: crate::theme::ThemeId,
    /// The theme in effect when the theme picker opened; `esc` goes back to it.
    pub theme_on_open: Option<crate::theme::ThemeId>,
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
    /// The tab sidebar is hidden by the Captain (it also hides itself on narrow terminals).
    pub sidebar_hidden: bool,
    /// The right column: `None` follows the terminal width, `Some` is the Captain's choice.
    pub panel_shown: Option<bool>,
    /// Whether the right column was on screen at the last draw: the toggle flips what was seen.
    pub panel_drawn: bool,
    /// The open permission request whose keys were on screen at the last draw. For any other
    /// (scrolled away, or not drawn yet), the first `y`, `a` or `n` brings it into view instead
    /// of answering it.
    pub prompt_shown: Option<String>,
}
