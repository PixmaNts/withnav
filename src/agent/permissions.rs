//! The wire types of a permission question: the request a held tool call
//! raises, the Captain's answer, and what the request is about. The decision
//! itself is made in `agent/authorize.rs` and `src/permission/`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::error::ToolError;
use crate::provider::ToolCall;
#[cfg(unix)]
use crate::tools::validate_path;
use crate::tools::{ToolContext, bound_output};

/// The longest rendering of a call's arguments kept in a request, in bytes.
pub const MAX_REVIEW_ARGUMENT_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOrigin {
    Captain,
    Bypass,
    /// The parallel metacognition review (`permissions.review` = `on`): it may only allow one
    /// call, once, and the call reads the rules again before it runs.
    Metacognition,
}

impl PermissionOrigin {
    /// The `approved_by` label a call, an event and a decision row carry for this origin.
    pub fn label(self) -> &'static str {
        match self {
            Self::Captain => "captain",
            Self::Bypass => "bypass",
            Self::Metacognition => "metacognition",
        }
    }
}

/// What the parallel review of one request reads and is bound to, prepared when the question is
/// raised. Present only on a request the review may answer (see `authorize::reviewable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTicket {
    /// The reviewer's input: the projection of the current request, the call and its context,
    /// already bounded and with the session's secrets redacted.
    pub input: String,
    /// The rule that matched when the question was raised; a different one at dispatch sends
    /// the call back through the normal decision.
    pub rule_id: Option<i64>,
}

/// Hex SHA-256 of the tool, its arguments and the call id: what an answer to a request is bound
/// to, so an answer meant for another call (a retried or rewritten one) is stale.
pub fn call_digest(call: &ToolCall) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(call.name.as_bytes());
    hash.update([0]);
    hash.update(serde_json::to_vec(&call.args).unwrap_or_default());
    hash.update([0]);
    hash.update(call.id.as_bytes());
    hex::encode(hash.finalize())
}

/// A Captain's answer to a [`PermissionRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Run this one call.
    AllowOnce,
    /// Save the proposed allow rules, then run this call.
    AllowAlways,
    /// Do not run the call; the optional message is the Captain's reason.
    Reject {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Legacy wire compatibility: the folder answer of the removed per-call
    /// review. Still parsed from older clients and stored events; applied as
    /// `AllowOnce` (see [`PermissionDecision::current`]).
    AllowFolder,
    /// Legacy wire compatibility: the session-wide configuration answer of
    /// the removed per-call review. Applied as `AllowOnce`.
    AllowConfigSession,
    /// Legacy wire compatibility: the machine-wide skill script answer. Always
    /// refused as stale.
    AllowSkillMachine,
    /// Legacy wire compatibility: the refusal of the removed per-call review.
    /// Applied as `Reject` with its message, which the model reads back.
    Deny { message: String },
}

pub use withnav_client_api::permissions::{
    REJECT_MESSAGE_LIMIT_ERROR, REJECT_MESSAGE_MAX_BYTES, Scope,
};

impl PermissionDecision {
    pub fn validate_message(&self) -> Result<(), &'static str> {
        let message = match self {
            Self::Reject { message } => message.as_deref(),
            Self::Deny { message } => Some(message.as_str()),
            _ => None,
        };
        if message.is_some_and(|message| message.len() > REJECT_MESSAGE_MAX_BYTES) {
            Err(REJECT_MESSAGE_LIMIT_ERROR)
        } else {
            Ok(())
        }
    }
}

/// A gated call waiting for a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    #[serde(default)]
    pub requires_captain: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_gate: Option<Box<Value>>,
    pub id: Ulid,
    pub tool_call_id: String,
    pub tool: String,
    /// The call's arguments as JSON text, cut to
    /// [`MAX_REVIEW_ARGUMENT_BYTES`] for review.
    pub arguments: String,
    pub scope: Scope,
    /// The id of the agent that made the call.
    pub agent: String,
    /// Why approval is needed, in a sentence a captain can show.
    pub reason: String,
    /// The text of the "Always" button; absent when "Always" is not offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_label: Option<String>,
    /// Whether this request can persist a reusable rule or skill validation.
    /// Explicitly false when the session's permission store is unavailable.
    #[serde(default)]
    pub reusable_approval: bool,
    /// The rules an "Always" answer saves; empty when it is not offered.
    #[serde(skip)]
    pub always: Vec<crate::permission::decide::RuleProposal>,
    /// The pending row of the decision table this request settles.
    #[serde(skip)]
    pub decision_id: Option<i64>,
    /// Identical consecutive calls of the turn when the call was decided.
    #[serde(skip)]
    pub repeats: usize,
    /// A permission rule asks about a skill whose content is already
    /// validated: an ordinary question, not a validation.
    #[serde(skip)]
    pub skill_rule: bool,
    /// The outcome of saving the "Always" rules when it happened before the
    /// answer was published: the saved rule ids, or why they were not saved.
    #[serde(skip)]
    pub always_saved: Option<Result<Vec<i64>, String>>,
    /// [`call_digest`] of the held call.
    #[serde(skip)]
    pub call_digest: String,
    /// The parallel review's input, when the review may answer this request.
    #[serde(skip)]
    pub review: Option<Box<ReviewTicket>>,
}

impl PermissionRequest {
    pub fn new(call: &ToolCall, scope: Scope, agent: &str) -> Self {
        let arguments = serde_json::to_string(&call.args).unwrap_or_default();
        let (arguments, _) = bound_output(&arguments, MAX_REVIEW_ARGUMENT_BYTES);
        let reason = match &scope {
            Scope::Folder(folder) => format!("`{}` acts in {}", call.name, folder.display()),
            Scope::Origin(origin) => format!("`{}` reaches {origin}", call.name),
        };
        Self {
            requires_captain: false,

            permission_gate: None,
            id: Ulid::generate(),
            tool_call_id: call.id.clone(),
            tool: call.name.clone(),
            arguments,
            scope,
            agent: agent.to_string(),
            reason,
            always_label: None,
            reusable_approval: false,
            always: Vec::new(),
            decision_id: None,
            repeats: 1,
            skill_rule: false,
            always_saved: None,
            call_digest: call_digest(call),
            review: None,
        }
    }
}

/// Why a decision was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PermissionError {
    /// No permission request with that id is waiting: it was already
    /// answered, it was settled by a stop or a new message, or it never
    /// existed.
    #[error("no such permission request is waiting")]
    Stale,
}

impl PermissionDecision {
    /// The answer as the rule-based model applies it. "Allow folder" and
    /// "allow configuration for the session" belong to the per-call review and
    /// count as one approval; a legacy `deny` is a reject with its message.
    pub fn current(self) -> Self {
        match self {
            Self::AllowFolder | Self::AllowConfigSession => Self::AllowOnce,
            Self::Deny { message } => Self::Reject {
                message: Some(message),
            },
            other => other,
        }
    }
}

pub fn resolve_read_target(path: &Path, cx: &ToolContext) -> Result<PathBuf, ToolError> {
    #[cfg(windows)]
    {
        let home = directories::BaseDirs::new();
        crate::tools::win_path::resolve_target(
            path,
            &cx.workspace,
            home.as_ref().map(|dirs| dirs.home_dir()),
        )
    }
    #[cfg(unix)]
    {
        let path = requested_path(path, cx)?;
        validate_path(&[PathBuf::from("/")], &path)
    }
}

pub fn requested_path(path: &Path, cx: &ToolContext) -> Result<PathBuf, ToolError> {
    #[cfg(windows)]
    {
        let home = directories::BaseDirs::new();
        crate::tools::win_path::requested_path(
            path,
            &cx.workspace,
            home.as_ref().map(|dirs| dirs.home_dir()),
        )
    }
    #[cfg(unix)]
    {
        // Native tools do not perform shell expansion: only a bare `~` component
        // names HOME; `~name` remains a literal workspace-relative filename.
        let path = if path.starts_with("~") {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| ToolError::PathTraversal(path.to_path_buf()))?;
            PathBuf::from(home).join(
                path.strip_prefix("~")
                    .map_err(|_| ToolError::PathTraversal(path.to_path_buf()))?,
            )
        } else if path.is_absolute() {
            path.to_path_buf()
        } else {
            cx.workspace.join(path)
        };
        if path.as_os_str().is_empty() || path.to_string_lossy().contains('\0') {
            return Err(ToolError::PathTraversal(path));
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        _tmp: tempfile::TempDir,
        ws: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap().join("ws");
        Fixture { _tmp: tmp, ws }
    }

    #[test]
    fn the_request_bounds_the_arguments_it_shows() {
        let f = fixture();
        let call = ToolCall {
            id: "c1".into(),
            name: "write".into(),
            args: json!({"path": f.ws.join("a"), "content": "x".repeat(50_000)}),
            args_valid: true,
        };
        let request = PermissionRequest::new(&call, Scope::Folder(f.ws.clone()), "build");
        assert!(request.arguments.len() <= MAX_REVIEW_ARGUMENT_BYTES);
        assert_eq!(request.tool_call_id, "c1");
        assert_eq!(request.agent, "build");
        assert!(request.reason.contains("write"));
    }

    #[test]
    fn legacy_answers_still_parse_and_map_to_current_ones() {
        for (wire, current) in [
            (
                json!({"decision":"allow_folder"}),
                PermissionDecision::AllowOnce,
            ),
            (
                json!({"decision":"allow_config_session"}),
                PermissionDecision::AllowOnce,
            ),
            (
                json!({"decision":"deny","message":"no"}),
                PermissionDecision::Reject {
                    message: Some("no".into()),
                },
            ),
        ] {
            let decision: PermissionDecision = serde_json::from_value(wire).unwrap();
            assert_eq!(decision.current(), current);
        }
    }
}
