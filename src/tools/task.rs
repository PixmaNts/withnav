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
        json!({"description":"Start a background subagent","parameters":{"type":"object","properties":{"agent":{"type":"string"},"prompt":{"type":"string"},"delivery":{"type":"string","enum":["next_turn","steer","interrupt"]}},"required":["agent","prompt"]}})
    }
    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
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
            let id = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ToolError::Canceled),
                result = self.service.create_child_with_delivery(self.parent, agent, prompt,args.get("delivery").and_then(Value::as_str).unwrap_or("next_turn")) => result,
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_task_never_waits_for_admission() {
        let dir = tempfile::tempdir().unwrap();
        let service = Service::new(crate::session::SessionConfig {
            models_path: None,
            config_dir: dir.path().join("cfg"),
            history_db: dir.path().join("history.db"),
            data_dir: dir.path().join("data"),
            rules_roots: Default::default(),
        })
        .unwrap();
        let task = TaskTool::new(service, ulid::Ulid::new());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cx = ToolContext {
            binding: None,
            workspace: std::env::temp_dir(),
            writable_roots: vec![],
            readable_roots: vec![],
        };
        assert!(matches!(
            task.call(json!({"agent":"explore","prompt":"inspect"}), cancel, &cx)
                .await,
            Err(ToolError::Canceled)
        ));
    }
}
