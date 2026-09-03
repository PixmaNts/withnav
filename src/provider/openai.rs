use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ProviderError;
use crate::provider::config::ProviderConfig;
use crate::provider::Usage;

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<RequestToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: RequestFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDef {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub error: Option<ApiErrorBody>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: ChunkDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiErrorBody {
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

pub fn build_client(provider: &ProviderConfig) -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(provider.connect_timeout_ms))
        .no_proxy()
        .build()
        .map_err(ProviderError::Http)
}

pub fn build_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    api_key: &str,
    body: &ChatRequest,
) -> reqwest::RequestBuilder {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let mut req = client.post(url).bearer_auth(api_key).json(body);
    for (k, v) in &provider.headers {
        req = req.header(k, v);
    }
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_serializes_stream_true_and_tool() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::User {
                content: "hi".into(),
            }],
            tools: vec![ToolDef::function(
                "read",
                "read a file",
                json!({"type":"object","properties":{"path":{"type":"string"}}}),
            )],
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            max_tokens: Some(100),
            temperature: None,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"stream\":true"));
        assert!(s.contains("\"type\":\"function\""));
        assert!(s.contains("\"include_usage\":true"));
    }

    #[test]
    fn assistant_message_serializes_tool_calls_and_reasoning() {
        let msg = ChatMessage::Assistant {
            content: Some("done".into()),
            reasoning_content: Some("thinking".into()),
            tool_calls: vec![RequestToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: RequestFunction {
                    name: "read".into(),
                    arguments: "{\"path\":\"a\"}".into(),
                },
            }],
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"role\":\"assistant\""));
        assert!(s.contains("\"reasoning_content\":\"thinking\""));
        assert!(s.contains("\"tool_calls\""));
        assert!(s.contains("call_1"));
    }

    #[test]
    fn tool_message_serializes_tool_call_id() {
        let msg = ChatMessage::Tool {
            tool_call_id: "call_1".into(),
            content: "result".into(),
        };
        let s = serde_json::to_string(&msg).unwrap();
        assert!(s.contains("\"role\":\"tool\""));
        assert!(s.contains("\"tool_call_id\":\"call_1\""));
    }

    #[test]
    fn deserialize_reasoning_chunk() {
        let raw = include_str!("../../tests/fixtures/chunks/reasoning.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            chunk.choices[0].delta.reasoning_content.as_deref(),
            Some("We")
        );
        assert!(chunk.choices[0].delta.content.is_none());
    }

    #[test]
    fn deserialize_toolcall_first_fragment() {
        let raw = include_str!("../../tests/fixtures/chunks/toolcall_first.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_00_199TsKpFMjN3EXHTQZFE3344"));
        assert_eq!(
            tc.function.as_ref().unwrap().name.as_deref(),
            Some("get_weather")
        );
    }

    #[test]
    fn deserialize_toolcall_args_fragment_has_no_id_or_name() {
        let raw = include_str!("../../tests/fixtures/chunks/toolcall_args.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert!(tc.id.is_none());
        let f = tc.function.as_ref().unwrap();
        assert!(f.name.is_none());
        assert_eq!(f.arguments.as_deref(), Some("{\"city\":\"Paris\"}"));
    }

    #[test]
    fn deserialize_final_usage_chunk() {
        let raw = include_str!("../../tests/fixtures/chunks/final_usage.json");
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            chunk.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 365);
        assert_eq!(usage.completion_tokens, 63);
        assert_eq!(usage.total_tokens, 428);
    }

    #[test]
    fn deserialize_error_object_routes_to_error_branch() {
        let raw = r#"{"error":{"message":"model overloaded","type":"server_error","code":"503"}}"#;
        let chunk: ChatChunk = serde_json::from_str(raw).unwrap();
        assert!(chunk.error.is_some());
        assert_eq!(chunk.error.unwrap().message, "model overloaded");
    }
}
