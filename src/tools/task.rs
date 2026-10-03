use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::session::{Service, SessionId};
use crate::tools::{Tool, ToolContext, ToolResult};

pub struct TaskTool {
    service: Service,
    parent: SessionId,
}

impl TaskTool {
    pub fn new(service: Service, parent: SessionId) -> Self {
        Self { service, parent }
    }
}

impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn schema(&self) -> Value {
        json!({"description":"Start a background subagent","parameters":{"type":"object","properties":{"agent":{"type":"string"},"prompt":{"type":"string"}},"required":["agent","prompt"]}})
    }
    fn call<'a>(
        &'a self,
        args: Value,
        _cancel: CancellationToken,
        _cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let agent =
                args.get("agent")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::Denied {
                        code: "invalid_args".into(),
                        message: "agent must be a string".into(),
                    })?;
            let prompt =
                args.get("prompt")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::Denied {
                        code: "invalid_args".into(),
                        message: "prompt must be a string".into(),
                    })?;
            let id = self
                .service
                .create_child_session(self.parent, agent, prompt)
                .await
                .map_err(|e| ToolError::Denied {
                    code: "subagent_error".into(),
                    message: e.to_string(),
                })?;
            Ok(ToolResult::ok(
                json!({"child_id":id.to_string(),"agent":agent,"status":"running"}).to_string(),
            ))
        })
    }
}
