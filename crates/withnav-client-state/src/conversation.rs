//! The blocks of a conversation transcript and the projection that builds them.
use std::collections::HashMap;

use serde_json::Value;
use withnav_client_api::{provider::Usage, tools::ToolStatus};

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct SkillScript {
    pub skill: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Activity {
    #[default]
    Idle,
    Running,
    AwaitingUser,
    OutOfSteps,
    Stopped,
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
    Question(QuestionPrompt),
    Error(String),
    Notice(String),
    /// A gated tool call waiting for the Captain's decision, then how it ended.
    Permission(PermissionPrompt),
    /// The outcome of a server command: its name, whether it ran, and what it answered.
    Command {
        name: String,
        ok: bool,
        text: String,
    },
}
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionPrompt {
    pub text: String,
    pub call_id: String,
    pub choices: withnav_client_api::tools::AskChoices,
    pub answer: Option<withnav_client_api::tools::AskAnswer>,
    pub cursor: usize,
    pub selected: Vec<usize>,
    pub sending: bool,
    pub dismissed: bool,
    pub custom: bool,
    pub error: Option<String>,
}
/// What a permission request asks and, once it ended, how.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionPrompt {
    pub always_label: Option<String>,
    pub skill_script: Option<Box<SkillScript>>,
    pub child_session_id: Option<String>,
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
    /// What a denial told the model, or why an "Always" answer was not saved.
    pub message: Option<String>,
    /// False when an "Always" answer could not save its rules: the call ran once.
    pub always_saved: bool,
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
    pub context: Option<withnav_client_api::protocol::ContextSnapshot>,
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
    pub last_compaction: Option<withnav_client_api::event::ContextCompacted>,
}

pub fn block(id: impl Into<String>, kind: BlockKind) -> Block {
    Block {
        id: id.into(),
        kind,
    }
}
