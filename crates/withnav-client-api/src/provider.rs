//! Model-facing value types that travel inside events: tool calls, token usage, the assembled
//! assistant message and the incremental pieces of a streamed response.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
    #[serde(default = "default_true")]
    pub args_valid: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub text: String,
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// One incremental piece of a model response. On the wire (`llm.delta`) it
/// carries a `delta_type` tag next to its fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "delta_type", rename_all = "snake_case")]
pub enum StreamDelta {
    Text { text: String },
    Thinking { text: String },
    ToolCallStarted { index: usize, name: String },
    ToolCallArgs { index: usize, fragment: String },
}
