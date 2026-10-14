use std::{future::Future, path::PathBuf, pin::Pin};

use serde_json::Value;
use tokio_util::sync::CancellationToken;
#[path = "captain_messages.rs"]
mod captain_evidence;
pub use captain_evidence::{
    captain_messages, captain_messages_with_cut, delegation_with_cut, recent_with_cut,
    RecentMessage,
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
    /// The host already ran the compiled protected check for this call: its
    /// answer is the only authority, and the flows do not ask it again.
    pub protected_check: Option<ProtectedCheck>,
}

/// One flow run behind a permission decision: its id in the flow run log
/// (`flows/runs/<workspace>/runs.jsonl`) and the score of each node that asked
/// Instinct a question.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct RunScores {
    pub run_id: String,
    pub scores: std::collections::BTreeMap<String, f64>,
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct PermissionFlowAnswer {
    pub flow: String,
    pub run_id: String,
    pub action: String,
    pub used_rule: bool,
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
