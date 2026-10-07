//! Host-owned schemas and ceilings. No action names are built into the engine.
use crate::{Diagnostic, FallbackAction};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalogue {
    pub version: u32,
    pub triggers: Vec<TriggerSpec>,
    pub inputs: Vec<InputSpec>,
    pub actions: Vec<ActionSpec>,
    pub limits: Limits,
}
pub trait Trigger: Send + Sync {
    fn spec(&self) -> TriggerSpec;
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSpec {
    pub name: String,
    pub payload_fields: Vec<FieldSpec>,
    pub can_block: bool,
    pub allowed_actions: Vec<String>,
    pub sample_payload: serde_json::Value,
    pub default_fallback: Option<FallbackAction>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    pub name: String,
    pub value_type: ValueType,
    pub required: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    pub name: String,
    pub input_type: InputType,
    pub value_type: ValueType,
    pub source: String,
    pub triggers: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    Text,
    Image,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueType {
    String,
    Number,
    Integer,
    Boolean,
    Object,
    Array,
}
impl ValueType {
    pub fn matches(self, value: &serde_json::Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            Self::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            Self::Boolean => value.is_boolean(),
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    pub name: String,
    pub params: Vec<ParamSpec>,
    pub kind: ActionKind,
    pub priority: Option<u32>,
    pub allowed_triggers: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    pub name: String,
    pub value_type: ValueType,
    pub required: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Verdict,
    Effect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub nodes: usize,
    pub decisions_per_run: usize,
    pub blocking_budget_ms: u64,
    pub async_deadline_ms: u64,
    pub call_deadline_ms: u64,
    pub flow_bytes: usize,
    pub input_bytes: usize,
    pub run_input_bytes: usize,
    pub state_bytes: usize,
    pub template_bytes: usize,
    pub expression_bytes: usize,
    pub expression_tokens: usize,
    pub expression_operators: usize,
    pub expression_nesting: usize,
    pub expression_ast_depth: usize,
    pub expression_ast_nodes: usize,
    pub response_bytes: usize,
    pub concurrent_calls: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            nodes: 64,
            decisions_per_run: 16,
            blocking_budget_ms: 2000,
            async_deadline_ms: 5000,
            call_deadline_ms: 2000,
            flow_bytes: 262144,
            input_bytes: 16384,
            run_input_bytes: 65536,
            state_bytes: 65536,
            template_bytes: 65536,
            expression_bytes: 4096,
            expression_tokens: 128,
            expression_operators: 32,
            expression_nesting: 16,
            expression_ast_depth: 32,
            expression_ast_nodes: 128,
            response_bytes: 262144,
            concurrent_calls: 4,
        }
    }
}
impl Catalogue {
    pub fn trigger(&self, name: &str) -> Option<&TriggerSpec> {
        self.triggers.iter().find(|s| s.name == name)
    }
    pub fn action(&self, name: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|s| s.name == name)
    }
    pub fn validate(&self) -> Result<(), Vec<Diagnostic>> {
        let mut errors = Vec::new();
        if self.version != 1 {
            errors.push(Diagnostic::new(
                "schema",
                "unsupported catalogue version",
                "catalogue.version",
            ));
        }
        for (path, names) in [
            (
                "triggers",
                self.triggers.iter().map(|s| &s.name).collect::<Vec<_>>(),
            ),
            ("inputs", self.inputs.iter().map(|s| &s.name).collect()),
            ("actions", self.actions.iter().map(|s| &s.name).collect()),
        ] {
            let mut seen = BTreeSet::new();
            for name in names {
                if !seen.insert(name) {
                    errors.push(Diagnostic::new("schema", "duplicate catalogue name", path));
                }
            }
        }
        for trigger in &self.triggers {
            let mut fields = BTreeSet::new();
            for field in &trigger.payload_fields {
                if !fields.insert(&field.name) {
                    errors.push(Diagnostic::new(
                        "schema",
                        "duplicate payload field",
                        &trigger.name,
                    ));
                }
            }
            for action in &trigger.allowed_actions {
                if !self
                    .action(action)
                    .is_some_and(|a| a.allowed_triggers.contains(&trigger.name))
                {
                    errors.push(Diagnostic::new(
                        "action_not_allowed",
                        "catalogue permissions disagree",
                        &trigger.name,
                    ));
                }
            }
        }
        for action in &self.actions {
            if (action.kind == ActionKind::Verdict) != action.priority.is_some() {
                errors.push(Diagnostic::new(
                    "schema",
                    "only verdicts require a priority",
                    &action.name,
                ));
            }
            let mut params = BTreeSet::new();
            for param in &action.params {
                if !params.insert(&param.name) {
                    errors.push(Diagnostic::new(
                        "schema",
                        "duplicate parameter",
                        &action.name,
                    ));
                }
            }
            for trigger in &action.allowed_triggers {
                if !self
                    .trigger(trigger)
                    .is_some_and(|t| t.allowed_actions.contains(&action.name))
                {
                    errors.push(Diagnostic::new(
                        "action_not_allowed",
                        "catalogue permissions disagree",
                        &action.name,
                    ));
                }
            }
        }
        for input in &self.inputs {
            for trigger in &input.triggers {
                if self.trigger(trigger).is_none() {
                    errors.push(Diagnostic::new(
                        "unknown_trigger",
                        "input names unknown trigger",
                        &input.name,
                    ));
                }
            }
        }
        if self.limits.concurrent_calls == 0 || self.limits.nodes == 0 {
            errors.push(Diagnostic::new(
                "limit",
                "catalogue limits must be positive",
                "limits",
            ));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asymmetric_permissions_and_duplicate_names_are_rejected() {
        let mut catalogue: Catalogue =
            serde_json::from_str(include_str!("../tests/fixtures/catalogue.json")).unwrap();
        catalogue.validate().unwrap();
        catalogue.triggers[0].allowed_actions.clear();
        assert!(catalogue.validate().is_err());
        let action = catalogue.actions[0].clone();
        catalogue.actions.push(action);
        assert!(catalogue
            .validate()
            .unwrap_err()
            .iter()
            .any(|d| d.code == "schema"));
    }
}
