use std::{future::Future, path::PathBuf, pin::Pin};

use serde_json::Value;
use tokio_util::sync::CancellationToken;
#[path = "captain_messages.rs"]
mod captain_evidence;
pub use captain_evidence::{
    PERMISSION_PROJECTION, RecentMessage, TOOL_TRAIL_CALLS, ToolTrailEntry, captain_messages,
    captain_messages_with_cut, current_request_with_cut, delegation_with_cut, recent_with_cut,
    tool_trail_with_cut,
};

#[derive(Debug, PartialEq, Eq)]
pub enum FlowVerdict {
    Allow,
    Deny {
        note: String,
    },
    AskCaptain {
        note: String,
        flow: String,
        run_id: String,
    },
}

/// The verdict actions a flow may propose on `tool.pre_call`, carried as a
/// type so that aggregation matches exhaustively instead of falling through a
/// string wildcard. The snake_case names are the flow-definition spelling and
/// the serialized form (`flow_trace` in `permission_decisions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowAction {
    Allow,
    AskCaptain,
    Deny,
    ApplyRule,
}

impl FlowAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AskCaptain => "ask_captain",
            Self::Deny => "deny",
            Self::ApplyRule => "apply_rule",
        }
    }

    /// Parse a proposed action name. `None` for anything the catalogue does
    /// not declare: callers must fail safe on it, never allow.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "allow" => Some(Self::Allow),
            "ask_captain" => Some(Self::AskCaptain),
            "deny" => Some(Self::Deny),
            "apply_rule" => Some(Self::ApplyRule),
            _ => None,
        }
    }

    /// Strictest-wins order: `deny`, then `ask_captain`, then `allow`.
    /// `apply_rule` is resolved to the rule's action before comparison.
    fn rank(self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::AskCaptain => 1,
            Self::Deny => 2,
            Self::ApplyRule => 0,
        }
    }

    /// The strictest of two resolved actions (neither `ApplyRule`).
    pub fn strictest(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// A rule reaching the flow cannot be a deny: written denies stop beforehand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleResult {
    Allow,
    Ask,
    None,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PermissionCommand {
    pub raw: String,
    pub normalized: String,
    pub prefix: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ValidatedSkill {
    pub name: String,
    pub instructions: String,
}

/// New call context leaves today's callers and permission gate unchanged.
#[derive(Debug)]
pub struct PermissionCallContext {
    pub call: ToolCallContext,
    pub rule: Option<String>,
    pub commands: Option<Vec<PermissionCommand>>,
    pub inside_project: Option<bool>,
    /// Where the call acts, from its literal paths: `project`, `scratch`,
    /// `temp`, `outside` or `unknown`; see [`crate::permission::floor::location`].
    pub location: Option<&'static str>,
    pub protected: bool,
    pub skill: Option<ValidatedSkill>,
    pub captain_messages: Vec<String>,
    pub captain_messages_cut: bool,
    pub delegation: Vec<String>,
    pub delegation_cut: bool,
    /// The last messages between the Captain and the agent, oldest first; the
    /// agent's are context for a short Captain answer, never a request.
    pub recent: Vec<RecentMessage>,
    pub recent_cut: bool,
    /// What the Captain asked for in the current request: the agent's message that ended the
    /// previous turn, then the Captain's messages and answers since. Empty when the review is
    /// off, because nothing reads it then.
    pub current_request: Vec<RecentMessage>,
    pub current_request_cut: bool,
    /// The tools already called in the current request and how each ended; no arguments, no
    /// output. Empty when the review is off.
    pub tool_trail: Vec<ToolTrailEntry>,
    /// Whether older calls were dropped to keep the newest.
    pub tool_trail_cut: bool,
    /// Whether the consequence review runs beside the decision.
    pub review: crate::settings::ReviewMode,
    /// The host already ran the compiled protected check for this call: its
    /// answer is the only authority, and the flows do not ask it again.
    pub protected_check: Option<ProtectedCheck>,
}

/// One flow run behind a permission decision: its id in the flow run log
/// (`flows/runs/<workspace>/runs.jsonl`) and the score of each node that asked
/// Instinct a question, with the model and the flow text that produced them.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct RunScores {
    pub run_id: String,
    pub scores: std::collections::BTreeMap<String, f64>,
    /// Digest of the flow definition that ran (see [`PermissionFlowAnswer`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub definition_digest: String,
    /// The Instinct model the run was configured with.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
}

impl ProtectedCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The answer of the compiled-in protected check alone, the only authority
/// for a write to WithNav's own configuration or database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectedCheck {
    /// The Captain's own messages explicitly ask for the change.
    Passed,
    /// The check ran and the messages do not ask for it.
    Failed,
    /// The built-in flow is disabled, or the check failed or could not rely on
    /// its evidence: nothing is established.
    Unavailable,
}

#[derive(Debug)]
pub struct PermissionVerdict {
    pub verdict: FlowVerdict,
    pub flows: Vec<PermissionFlowAnswer>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PermissionFlowAnswer {
    pub flow: String,
    pub run_id: String,
    pub action: FlowAction,
    pub used_rule: bool,
    /// The score of each node of this run that asked Instinct.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub scores: std::collections::BTreeMap<String, f64>,
    /// Digest of the flow definition that ran. The decision row keeps the
    /// Captain's answer, the label a score is compared with; the 673 scored
    /// rows read on 2026-11-25 named neither the flow text nor the model, so
    /// scores from different versions could not be told apart. Empty when no
    /// flow ran.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub definition_digest: String,
    /// The Instinct model the run was configured with; empty when no flow ran.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
}

#[derive(Debug)]
pub struct ToolCallContext {
    pub call_id: String,
    pub tool: String,
    pub args: Value,
    pub agent: Option<String>,
    pub workspace: PathBuf,
}

pub trait ToolCallFlows: Send + Sync + std::fmt::Debug {
    fn permission_pre_call<'a>(
        &'a self,
        ctx: PermissionCallContext,
        rule_result: RuleResult,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = PermissionVerdict> + Send + 'a>> {
        Box::pin(async move {
            let verdict = if !ctx.protected && rule_result == RuleResult::Allow {
                FlowVerdict::Allow
            } else {
                FlowVerdict::AskCaptain {
                    note: "Permission rule requires the Captain.".into(),
                    flow: String::new(),
                    run_id: String::new(),
                }
            };
            let _ = cancel;
            PermissionVerdict {
                verdict,
                flows: vec![],
            }
        })
    }
    /// The compiled-in protected check alone; without a flow runtime nothing
    /// can establish the Captain's request.
    fn protected_check<'a>(
        &'a self,
        ctx: PermissionCallContext,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ProtectedCheck> + Send + 'a>> {
        let _ = (ctx, cancel);
        Box::pin(async { ProtectedCheck::Unavailable })
    }
    /// [`Self::protected_check`] with the run that answered it, when one ran.
    fn protected_check_traced<'a>(
        &'a self,
        ctx: PermissionCallContext,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = (ProtectedCheck, Option<RunScores>)> + Send + 'a>> {
        Box::pin(async move { (self.protected_check(ctx, cancel).await, None) })
    }
}
