use serde_json::Value;

use super::Settings;
use crate::provider::assignments::Layer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAssignment {
    pub model: String,
    pub variant: Option<String>,
}

impl Settings {
    pub fn layer_assignment(&self, layer: Layer) -> Result<Option<ModelAssignment>, String> {
        self.assignment("layers", layer.config_key(), true)
    }

    pub fn agent_assignment(&self, name: &str) -> Result<Option<ModelAssignment>, String> {
        self.assignment("agents", name, false)
    }

    pub fn default_agent(&self) -> Result<&str, String> {
        self.root
            .get("default_agent")
            .map_or(Ok("build"), |value| nonempty_string(value, "default_agent"))
    }

    fn assignment(
        &self,
        section: &str,
        name: &str,
        shorthand: bool,
    ) -> Result<Option<ModelAssignment>, String> {
        let Some(values) = self.root.get(section) else {
            return Ok(None);
        };
        let values = values
            .as_object()
            .ok_or_else(|| format!("{section} must be an object"))?;
        let Some(value) = values.get(name) else {
            return Ok(None);
        };
        let key = format!("{section}.{name}");
        if shorthand && value.is_string() {
            return Ok(Some(ModelAssignment {
                model: nonempty_string(value, &key)?.into(),
                variant: None,
            }));
        }
        let object = value.as_object().ok_or_else(|| {
            format!(
                "{key} must be {}an object with a model string",
                if shorthand { "a model string or " } else { "" }
            )
        })?;
        let model_key = format!("{key}.model");
        let model = object
            .get("model")
            .ok_or_else(|| format!("{model_key} is required"))?;
        Ok(Some(ModelAssignment {
            model: nonempty_string(model, &model_key)?.into(),
            variant: object
                .get("variant")
                .map(|v| nonempty_string(v, &format!("{key}.variant")).map(str::to_owned))
                .transpose()?,
        }))
    }
}

fn nonempty_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c.is_control()))
        .ok_or_else(|| format!("{key} must be a nonempty string without whitespace"))
}
