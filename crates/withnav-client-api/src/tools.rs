//! Tool-related value types that travel inside events and request bodies: a tool call's
//! settled status, the choices an `ask` offers and the Captain's structured answer.

use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Ok,
    Denied,
    Error,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskChoices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<AskOption>,
    pub multiple: bool,
    pub allow_custom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskAnswer {
    pub question_call_id: String,
    pub selected: Vec<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

impl AskAnswer {
    /// Retain provenance even for events from servers that stored bare labels.
    pub fn historical_text(&self, content: &str) -> String {
        let mut text = format!(
            "Historical answer: selected option(s) offered by the assistant (recorded content): {}",
            json!(content)
        );
        if let Some(custom) = &self.custom {
            text.push_str("\nCaptain's own words: ");
            text.push_str(custom);
        }
        text
    }
}

impl AskChoices {
    pub fn answer_text(&self, question: &str, answer: &AskAnswer) -> Result<String, String> {
        if !self.multiple && answer.selected.len() > 1 {
            return Err("this question allows only one selection".into());
        }
        let mut seen = std::collections::HashSet::new();
        let mut text = Vec::new();
        for &index in &answer.selected {
            let option = self.options.get(index).ok_or("choice index out of range")?;
            if !seen.insert(index) {
                return Err("duplicate choice index".into());
            }
            text.push(option.label.clone());
        }
        if let Some(custom) = &answer.custom
            && (!self.allow_custom || custom.trim().is_empty())
        {
            return Err("custom answer is not allowed or is empty".into());
        }
        if text.is_empty() && answer.custom.is_none() {
            return Err("select a choice or provide a custom answer".into());
        }
        let mut rendered = format!(
            "Answer to {}: selected option(s) offered by the assistant: {}",
            json!(question),
            json!(text)
        );
        if let Some(custom) = &answer.custom {
            rendered.push_str("\nCaptain's own words: ");
            rendered.push_str(custom);
        }
        Ok(rendered)
    }
}
