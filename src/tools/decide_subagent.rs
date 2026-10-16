use crate::error::ToolError;
use crate::session::{Service, SessionId};
use crate::tools::{Tool, ToolContext, ToolResult};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

pub struct DecideSubagent {
    service: Service,
    parent: SessionId,
}
impl DecideSubagent {
    pub fn new(service: Service, parent: SessionId) -> Self {
        Self { service, parent }
    }
}
impl Tool for DecideSubagent {
    fn name(&self) -> &str {
        "decide_subagent"
    }
    fn schema(&self) -> Value {
        json!({"description":"Continue or stop an owned subagent parked by a watchdog doubt (not a permission or budget answer)",
            "parameters":{"type":"object","properties":{"child_id":{"type":"string"},"decision":{"type":"string","enum":["continue","stop"]},"note":{"type":"string","maxLength":16384}},"required":["child_id","decision"],"additionalProperties":false}})
    }
    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let denied = |message| super::subagent_denied("subagent_decision", message);
            let child = super::parse_child_id(&args, &cancel, "subagent_decision")?;
            let stop = match args["decision"].as_str() {
                Some("continue") => false,
                Some("stop") => true,
                _ => return Err(denied("decision must be continue or stop".into())),
            };
            let note = match args.get("note") {
                None => None,
                Some(Value::String(note)) if note.len() <= 16 * 1024 => Some(note.as_str()),
                _ => return Err(denied("note must be a string of at most 16 KiB".into())),
            };
            self.service
                .decide_subagent(self.parent, child, stop, note)
                .await
                .map_err(|e| denied(e.to_string()))?;
            Ok(ToolResult::ok(
                json!({"child_id":child,"decision":args["decision"],"accepted":true}).to_string(),
            ))
        })
    }
}
