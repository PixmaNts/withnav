use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::tools::{Tool, ToolContext, ToolResult};

pub struct AskTool;

impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }
    fn schema(&self) -> Value {
        json!({
            "description": "Pause and ask the human operator for input or a physical action. Use when progress requires a human, missing credentials, or a decision only the operator can make.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": {"type": "string"},
                    "kind": {"type": "string"},
                    "details": {"type": "object"}
                },
                "required": ["question"]
            }
        })
    }
    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        _cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let question = args
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(ToolResult::ok(question))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_schema_advertises_question() {
        let schema = AskTool.schema();
        assert_eq!(schema["parameters"]["required"][0], "question");
    }
}
