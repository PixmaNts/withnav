use std::{future::Future, path::PathBuf, pin::Pin};

use serde_json::Value;
use tokio_util::sync::CancellationToken;
#[path = "captain_messages.rs"]
mod captain_evidence;
pub use captain_evidence::{captain_messages, captain_messages_with_cut};

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

impl FlowVerdict {
    /// Compatibility for the existing agent loop until permission cutover.
    #[allow(non_upper_case_globals)]
    pub const NoObjection: Self = Self::Allow;
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
    pub protected: bool,
    pub skill: Option<ValidatedSkill>,
    pub captain_messages: Vec<String>,
    pub captain_messages_cut: bool,
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
    fn pre_call<'a>(
        &'a self,
        call: &'a ToolCallContext,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = FlowVerdict> + Send + 'a>>;
}
