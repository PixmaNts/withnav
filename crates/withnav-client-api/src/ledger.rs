//! The session ledger: a small, bounded record of what a session did, folded from its stored
//! events by the server and served by `GET /v1/sessions/{id}/ledger`. Facts only (turns, files
//! changed, shell commands and their outcome, failures that repeat, what waits for the Captain),
//! so a reader (the panel, a later bounded model call) gets them without reading the transcript.
//!
//! Every list has a cap and every text a length limit, named below: a long session keeps the same
//! size. Every text comes from the model or a tool and is untrusted: the server strips control
//! characters from it before it lands here.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::event::TurnEndOutcome;
pub use crate::goal::{GoalStatus, SessionGoal};

// The bounds below were raised about fourfold on 2026-11-25 so the models reading the ledger get
// more context; tighten them again if the goal check's latency or cost suffer.

/// Files kept in [`SessionLedger::files_touched`], most recently changed first.
pub const MAX_FILES_TOUCHED: usize = 200;
/// Shell commands kept in [`SessionLedger::commands`]: the last ones run.
pub const MAX_COMMANDS: usize = 100;
/// Commands kept in [`SessionLedger::repeated_failures`].
pub const MAX_REPEATED_FAILURES: usize = 50;
/// Requests kept in [`SessionLedger::open_questions`], oldest first.
pub const MAX_OPEN_QUESTIONS: usize = 50;
/// How many times one command must fail before it is a repeated failure.
pub const REPEATED_FAILURE_THRESHOLD: u64 = 2;
/// Longest command text, error excerpt, question title or path, in characters.
pub const MAX_TEXT_CHARS: usize = 1000;
/// Longest [`SessionLedger::last_turn_text`], in characters.
pub const MAX_LAST_TURN_TEXT_CHARS: usize = 2000;
/// Hard cap of the text digest, in bytes.
pub const MAX_DIGEST_BYTES: usize = 16384;

/// What a session did, rebuilt from its stored events on every read.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionLedger {
    /// The `seq` of the last event folded; `None` when the session has no event yet.
    #[serde(default)]
    pub through_seq: Option<u64>,
    #[serde(default)]
    pub turns: LedgerTurns,
    /// Files the `write` and `edit` tools changed, each once, most recently changed first.
    #[serde(default)]
    pub files_touched: Vec<FileTouched>,
    /// The last shell commands run, oldest first.
    #[serde(default)]
    pub commands: Vec<CommandRun>,
    /// Commands that failed at least [`REPEATED_FAILURE_THRESHOLD`] times, most failures first.
    #[serde(default)]
    pub repeated_failures: Vec<RepeatedFailure>,
    /// The last assistant text of the last settled turn; `None` when it ended without text.
    #[serde(default)]
    pub last_turn_text: Option<String>,
    /// Questions and permission requests still waiting for the Captain, oldest first.
    #[serde(default)]
    pub open_questions: Vec<OpenQuestion>,
    /// How many times the conversation was compacted.
    #[serde(default)]
    pub compactions: u64,
    /// The goal the Captain set for the session (`/goal`), paused or active; `None` when none is
    /// set or it was cleared. The same fold of the goal events a resume uses.
    #[serde(default)]
    pub goal: Option<SessionGoal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LedgerTurns {
    /// Turns started.
    pub count: u64,
    /// The last turn that ended.
    #[serde(default)]
    pub last: Option<SettledTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettledTurn {
    pub turn_id: Ulid,
    pub outcome: TurnEndOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTouched {
    /// The path as the tool call named it, with empty and `.` segments dropped (`..` is kept).
    pub path: String,
    /// Successful `write` and `edit` calls on it.
    pub changes: u64,
    /// The turn (1 for the first) of its last change.
    pub last_turn: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRun {
    /// The command on one line (whitespace runs collapsed), cut to [`MAX_TEXT_CHARS`].
    pub command: String,
    pub outcome: CommandOutcome,
    /// The turn (1 for the first) it ran in.
    pub turn: u64,
}

/// How a shell command ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandOutcome {
    /// The process exited with this code (-1 when a signal ended it).
    Exited { code: i64 },
    /// The tool failed without an exit code (a timeout, a spawn failure).
    Failed { error: String },
    /// The permission decision refused it.
    Denied,
    /// A stop or a new message interrupted it.
    Interrupted,
}

impl CommandOutcome {
    /// An error exit or a tool failure; a refusal or an interruption is not one.
    pub fn is_failure(&self) -> bool {
        match self {
            Self::Exited { code } => *code != 0,
            Self::Failed { .. } => true,
            Self::Denied | Self::Interrupted => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepeatedFailure {
    /// The command, trimmed with whitespace runs collapsed, cut to [`MAX_TEXT_CHARS`].
    pub command: String,
    pub count: u64,
    /// The end of the last failure's output, at most [`MAX_TEXT_CHARS`].
    pub last_error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// An `ask` call.
    Ask,
    /// A `request_secret` call.
    Secret,
    /// A permission request, the session's own or one mirrored from a child.
    Permission,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenQuestion {
    pub kind: QuestionKind,
    /// The tool call id of the question, or the permission request id.
    pub id: String,
    /// The question, or `<tool>: <reason>` for a permission, cut to [`MAX_TEXT_CHARS`].
    pub title: String,
    /// Set when the request is a child's, mirrored on its parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<Ulid>,
}
