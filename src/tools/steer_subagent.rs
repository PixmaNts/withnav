use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::session::{Service, SessionId};
use crate::tools::{Tool, ToolContext, ToolResult};

pub struct SteerSubagent {
    service: Service,
    parent: SessionId,
}
impl SteerSubagent {
    pub fn new(service: Service, parent: SessionId) -> Self {
        Self { service, parent }
    }
}
impl Tool for SteerSubagent {
    fn name(&self) -> &str {
        "steer_subagent"
    }
    fn schema(&self) -> Value {
        json!({"description":"Guide or interrupt an owned background subagent","parameters":{"type":"object","properties":{"child_id":{"type":"string"},"message":{"type":"string"},"mode":{"type":"string","enum":["steer","interrupt"]}},"required":["child_id","message"]}})
    }
    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
        _cx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(ToolError::Canceled);
            }
            let denied = |message: String| ToolError::Denied {
                code: "subagent_error".into(),
                message,
            };
            let child = args["child_id"]
                .as_str()
                .ok_or_else(|| denied("child_id must be a string".into()))?
                .parse()
                .map_err(|_| denied("invalid child_id".into()))?;
            let message = args["message"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| denied("message must be nonempty".into()))?;
            let interrupt = match args.get("mode").and_then(Value::as_str).unwrap_or("steer") {
                "steer" => false,
                "interrupt" => true,
                _ => return Err(denied("mode must be steer or interrupt".into())),
            };
            self.service
                .steer_child(self.parent, child, message, interrupt)
                .await
                .map_err(|e| denied(e.to_string()))?;
            Ok(ToolResult::ok(
                json!({"child_id":child,"accepted":true}).to_string(),
            ))
        })
    }
}
