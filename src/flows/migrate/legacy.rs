//! Private read-only schema for converting historical pipeline.toml files.
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(super) struct Prefilters {
    pub min_changed_lines: usize,
    pub cooldown_ms: u64,
    pub wake_cooldown_ms: u64,
    pub bypass_on_help_call: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BranchDef {
    pub when: String,
    pub action: ActionDef,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ActionDef {
    Silent,
    Remark,
    #[serde(alias = "wake_cognition")]
    WakeCognition,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TriggerDef {
    pub on: Vec<String>,
    #[serde(default)]
    pub prefilter: Prefilters,
    pub state: String,
    pub questions: BTreeMap<String, instinct_flow::protocol::Question>,
    #[serde(default)]
    pub branch: Vec<BranchDef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PipelineFile {
    #[serde(rename = "instinct")]
    pub _instinct: InstinctDef,
    #[serde(default, rename = "trigger")]
    pub triggers: Vec<TriggerDef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstinctDef {
    #[serde(rename = "model")]
    pub _model: String,
}
