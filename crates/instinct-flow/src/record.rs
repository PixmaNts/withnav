//! Versioned private capture DTOs. Provider usage belongs to batches, never nodes.
use crate::{protocol::*, ClientIdentity, Event, FailureKind, InputValue, ProposedAction};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub version: u32,
    pub run_id: String,
    pub flow_name: String,
    pub definition_digest: String,
    pub mode: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub budget_ms: u64,
    pub event: Event,
    pub identity: ClientIdentity,
    pub inputs: BTreeMap<String, InputValue>,
    pub nodes: Vec<NodeRecord>,
    pub batches: Vec<BatchRecord>,
    pub status: String,
    pub failure: Option<FailureRecord>,
    pub actions: Vec<ProposedAction>,
    pub fallback_actions: Vec<ProposedAction>,
    pub usage: Option<DecisionUsage>,
    pub cost_usd: Option<f64>,
    pub application: ApplyReport,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: String,
    pub status: String,
    pub port: Option<String>,
    pub duration_ms: u64,
    pub answer: Option<Answer>,
    pub result: Option<bool>,
    pub variables: BTreeMap<String, serde_json::Value>,
    pub batch_id: Option<String>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_inputs: Option<BTreeMap<String, SelectedInput>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectedInput {
    pub reference: String,
    pub captured: InputValue,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRecord {
    pub id: String,
    pub node_ids: Vec<String>,
    pub state: String,
    pub questions: Questions,
    pub duration_ms: u64,
    pub usage: Option<DecisionUsage>,
    pub cost_usd: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub kind: FailureKind,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub status: String,
    pub actions: Vec<AppliedAction>,
}
impl Default for ApplyReport {
    fn default() -> Self {
        Self {
            status: "not_applied".into(),
            actions: Vec::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedAction {
    pub node_id: Option<String>,
    pub action: String,
    pub status: String,
    pub message: Option<String>,
}
#[derive(Debug, Clone, thiserror::Error)]
#[error("action application failed: {0}")]
pub struct ApplyError(pub String);
#[derive(Debug, Clone, Serialize)]
pub struct ReplayNode {
    pub id: String,
    pub status: String,
}
#[derive(Debug, Clone)]
pub struct ReplayResult {
    pub comparable: bool,
    pub nodes: Vec<ReplayNode>,
    pub outcome: Option<crate::RunOutcome>,
}
impl Serialize for ReplayResult {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            comparable: bool,
            nodes: &'a [ReplayNode],
            outcome: Option<&'a RunRecord>,
        }
        Wire {
            comparable: self.comparable,
            nodes: &self.nodes,
            outcome: self.outcome.as_ref().map(|o| &o.trace),
        }
        .serialize(s)
    }
}
pub(crate) fn timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Gregorian civil-date conversion makes capture timestamps independent of locale/timezone.
    let days = (seconds / 86400) as i64 + 719468;
    let era = days / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}
