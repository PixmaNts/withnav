//! The `skill` tool: loads one skill by its exact id.
//!
//! Success returns the body between a header naming the file and its
//! directory, then the relative paths of the files next to it (the model
//! opens them with `read`/`grep`, which accept skill roots for reading
//! only). Failures are structured errors with a stable `code`:
//! `skill_unknown`, `skill_ambiguous`, `skill_disabled`, `skill_unreadable`
//! and `skill_oversized`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{Resolution, Skills};
use crate::error::ToolError;
use crate::tools::{Tool, ToolContext, ToolResult};

/// The largest body the tool returns; a bigger one is an error, not a cut.
pub const MAX_BODY_BYTES: usize = 32 * 1024;
/// The most resource paths listed.
pub const MAX_RESOURCES: usize = 64;

pub struct SkillTool {
    skills: Arc<Skills>,
}

impl SkillTool {
    pub fn new(skills: Arc<Skills>) -> Self {
        Self { skills }
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn schema(&self) -> Value {
        json!({
            "description": "Load a skill by its exact id from the skills catalog. Returns its instructions and the files next to it. Loading does not run anything.",
            "parameters": {
                "type": "object",
                "properties": {"name": {"type": "string", "description": "the skill id, exactly as listed"}},
                "required": ["name"]
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
            let Some(name) = args.get("name").and_then(Value::as_str) else {
                return Err(ToolError::Denied {
                    code: "invalid_args".into(),
                    message: "missing string argument: name".into(),
                });
            };
            match self.skills.resolve(name) {
                Resolution::Unknown => {
                    return Ok(ToolResult::error("skill_unknown", "unknown skill"))
                }
                Resolution::Disabled => {
                    return Ok(ToolResult::error("skill_disabled", "disabled skill"))
                }
                Resolution::Ambiguous(paths) => {
                    return Ok(ToolResult::error(
                        "skill_ambiguous",
                        format!("ambiguous skill: {paths:?}"),
                    ))
                }
                Resolution::Found(_) => {}
            }
            let admitted = self
                .skills
                .validation
                .approved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(name)
                .cloned();
            if let Some(id) = admitted {
                if let Some(snapshot) = self
                    .skills
                    .validation
                    .snapshots
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&id)
                {
                    return Ok(snapshot.output.clone());
                }
            }
            Ok(ToolResult::error(
                "skill_not_validated",
                format!("skill {name} was not loaded: not validated"),
            ))
        })
    }
}
