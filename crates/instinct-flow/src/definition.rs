//! Canonical TOML and JSON definition model with per-kind schema admission.
use crate::{Capabilities, Catalogue, Diagnostic};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub flow: Flow,
    #[serde(default)]
    pub constants: BTreeMap<String, Value>,
    pub node: Vec<Node>,
    pub fallback: Option<Fallback>,
    #[serde(default)]
    pub layout: BTreeMap<String, Position>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub version: u32,
    pub name: String,
    pub on: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub blocking: bool,
    pub budget_ms: Option<u64>,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub when: Option<String>,
    pub start: Vec<String>,
}
fn enabled() -> bool {
    true
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Threshold {
    Number(f64),
    Reference(String),
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum NodeKind {
    Noul {
        instructions: String,
        threshold: Threshold,
        state_template: Option<String>,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
        state_template: Option<String>,
    },
    Score {
        instructions: String,
        levels: Vec<String>,
        at_least: String,
        state_template: Option<String>,
    },
    Expression {
        condition: String,
        #[serde(default)]
        assign: BTreeMap<String, String>,
    },
    Action {
        #[serde(deserialize_with = "captain_action")]
        action: String,
        params: BTreeMap<String, Value>,
    },
}
// A custom deserializer avoids serde's flatten/deny_unknown_fields incompatibility.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    pub id: String,
    pub label: Option<String>,
    #[serde(flatten)]
    pub kind: NodeKind,
    pub inputs: BTreeMap<String, String>,
    pub next: BTreeMap<String, Vec<String>>,
}
impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut raw = BTreeMap::<String, Value>::deserialize(d)?;
        fn take<T: serde::de::DeserializeOwned, E: serde::de::Error>(
            raw: &mut BTreeMap<String, Value>,
            key: &str,
            default: Option<T>,
        ) -> Result<T, E> {
            match raw.remove(key) {
                Some(v) => serde_json::from_value(v).map_err(E::custom),
                None => default.ok_or_else(|| E::custom(format!("missing field {key}"))),
            }
        }
        let id = take(&mut raw, "id", None)?;
        let label = take(&mut raw, "label", Some(None))?;
        let inputs = take(&mut raw, "inputs", Some(BTreeMap::new()))?;
        let next = take(&mut raw, "next", Some(BTreeMap::new()))?;
        let kind =
            serde_json::from_value(serde_json::to_value(raw).map_err(serde::de::Error::custom)?)
                .map_err(serde::de::Error::custom)?;
        Ok(Self {
            id,
            label,
            kind,
            inputs,
            next,
        })
    }
}
impl NodeKind {
    pub fn ports(&self) -> Vec<String> {
        match self {
            Self::Noul { .. } => vec!["yes".into(), "no".into()],
            Self::Choice { criteria, .. } => criteria.keys().cloned().collect(),
            Self::Score { .. } => vec!["at_least".into(), "below".into()],
            Self::Expression { .. } => vec!["true".into(), "false".into()],
            Self::Action { .. } => vec!["then".into()],
        }
    }
    pub fn template(&self) -> Option<&str> {
        match self {
            Self::Noul { state_template, .. }
            | Self::Choice { state_template, .. }
            | Self::Score { state_template, .. } => state_template.as_deref(),
            _ => None,
        }
    }
    pub fn question(&self) -> Option<crate::protocol::Question> {
        use crate::protocol::Question;
        match self {
            Self::Noul { instructions, .. } => Some(Question::Noul {
                instructions: instructions.clone(),
            }),
            Self::Choice {
                instructions,
                criteria,
                ..
            } => Some(Question::Choice {
                instructions: instructions.clone(),
                criteria: criteria.clone(),
            }),
            Self::Score {
                instructions,
                levels,
                ..
            } => Some(Question::Score {
                instructions: instructions.clone(),
                criteria: levels.clone(),
            }),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackAction {
    #[serde(deserialize_with = "captain_action")]
    pub action: String,
    pub params: BTreeMap<String, Value>,
    pub then: Option<Box<FallbackAction>>,
}
/// Keep accepting the previous human-action spelling in saved flows and runs.
pub(crate) fn captain_action<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let action = String::deserialize(d)?;
    if action == "ask_driver" {
        eprintln!("ask_driver is deprecated; use ask_captain");
        Ok("ask_captain".into())
    } else {
        Ok(action)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Fallback {
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub on_failure: Option<FallbackAction>,
    pub on_timeout: Option<FallbackAction>,
    pub on_provider_error: Option<FallbackAction>,
    pub on_invalid_answer: Option<FallbackAction>,
}
#[derive(Debug, Clone)]
pub struct CompiledDefinition {
    pub(crate) definition: Definition,
    pub(crate) catalogue: Catalogue,
    pub(crate) templates: BTreeMap<String, String>,
    pub(crate) order: Vec<usize>,
    pub(crate) predecessors: Vec<Vec<(usize, String)>>,
    pub(crate) expressions: BTreeMap<String, crate::expression::Expression>,
    pub(crate) digest: String,
}
impl CompiledDefinition {
    pub fn definition(&self) -> &Definition {
        &self.definition
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
}
impl Definition {
    pub fn parse(text: &str) -> Result<Self, Vec<Diagnostic>> {
        if text.len() > crate::MAX_FLOW_BYTES {
            return Err(vec![Diagnostic::new(
                "limit",
                "flow exceeds byte limit",
                "flow",
            )]);
        }
        toml::from_str(text).map_err(|e| vec![Diagnostic::new("parse", e.to_string(), "flow")])
    }
    pub fn to_toml(&self) -> Result<String, Vec<Diagnostic>> {
        if self
            .constants
            .values()
            .any(|v| !v.is_string() && !v.is_number() && !v.is_boolean())
        {
            return Err(vec![Diagnostic::new(
                "schema",
                "constants must be scalar values",
                "constants",
            )]);
        }
        // JSON null denotes optional fields, whereas TOML has no null value.
        fn omit_null(v: &mut Value) {
            match v {
                Value::Object(m) => {
                    m.retain(|_, v| !v.is_null());
                    for v in m.values_mut() {
                        omit_null(v);
                    }
                }
                Value::Array(a) => {
                    for v in a {
                        omit_null(v);
                    }
                }
                _ => {}
            }
        }
        let mut value = serde_json::to_value(self)
            .map_err(|e| vec![Diagnostic::new("schema", e.to_string(), "flow")])?;
        omit_null(&mut value);
        toml::to_string_pretty(&value)
            .map_err(|e| vec![Diagnostic::new("schema", e.to_string(), "flow")])
    }
    pub fn validate(
        &self,
        catalogue: &Catalogue,
        capabilities: &Capabilities,
        templates: &BTreeMap<String, String>,
    ) -> Result<CompiledDefinition, Vec<Diagnostic>> {
        crate::validation::validate(self, catalogue, capabilities, templates)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_defaults_normalize_and_null_constants_are_not_serialized() {
        let mut definition=Definition::parse("[flow]\nversion=1\nname='sample'\non='save'\nstart=['a']\n[[node]]\nid='a'\ntype='action'\naction='log'\n[node.params]\ntext='sample'\n").unwrap();
        assert!(definition.flow.enabled);
        assert!(!definition.flow.blocking);
        assert_eq!(
            Definition::parse(&definition.to_toml().unwrap()).unwrap(),
            definition
        );
        definition.constants.insert("NULL".into(), Value::Null);
        assert!(definition.to_toml().is_err());
    }
}
