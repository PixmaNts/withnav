use std::{future::Future, pin::Pin};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::memory::{Actor, ChangeStatus, ChangedBy};
use crate::session::{Service, SessionId};
use crate::tools::{Tool, ToolContext, ToolResult};

#[derive(Clone, Copy)]
pub(crate) enum Action {
    Add,
    Remove,
    Attach,
    Detach,
}

pub(crate) struct MemoryScope {
    pub service: Service,
    pub session: SessionId,
    pub action: Action,
}

impl Tool for MemoryScope {
    fn is_memory_scope_control(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        match self.action {
            Action::Add => "project_group_add",
            Action::Remove => "project_group_remove",
            Action::Attach => "session_attach",
            Action::Detach => "session_detach",
        }
    }

    fn schema(&self) -> Value {
        let (description, required) = match self.action {
            Action::Add => (
                "Propose adding a project to a memory group; the Captain must confirm.",
                vec!["project_id", "group_id"],
            ),
            Action::Remove => (
                "Remove this session's active project from a memory group, or propose removing another project.",
                vec!["project_id", "group_id"],
            ),
            Action::Attach => (
                "Propose attaching this session to a project; the Captain must confirm.",
                vec!["project_id"],
            ),
            Action::Detach => (
                "Detach this session from its active memory project immediately.",
                vec![],
            ),
        };
        let properties = required
            .iter()
            .map(|key| (key.to_string(), json!({"type":"string"})))
            .collect::<serde_json::Map<_, _>>();
        json!({"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
    }

    fn call<'a>(
        &'a self,
        args: Value,
        cancel: CancellationToken,
        _: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(ToolError::Canceled);
            }
            let denied = |message| super::subagent_denied("memory_scope", message);
            let session = self.session.to_string();
            let source = ChangedBy::new(Actor::Agent, Some(session.clone()));
            let store = self
                .service
                .memory_store()
                .await
                .map_err(|_| denied("memory register unavailable".into()))?;
            let action = self.action;
            let row = store
                .run(move |store| {
                    let id = |key: &str| {
                        args[key].as_str().ok_or_else(|| {
                            crate::memory::Error::Invalid(format!("{key} must be a string"))
                        })
                    };
                    match action {
                        Action::Add => {
                            store.propose_project_group(id("project_id")?, id("group_id")?, &source)
                        }
                        Action::Remove => store.request_project_group_removal(
                            &session,
                            id("project_id")?,
                            id("group_id")?,
                        ),
                        Action::Attach => {
                            store.propose_session_attach(&session, id("project_id")?, &source)
                        }
                        Action::Detach => store.detach_session(&session, &source),
                    }
                })
                .await
                .map_err(|error| denied(error.to_string()))?;
            let message = if row.status == ChangeStatus::Proposed {
                "Waiting for the Captain's confirmation."
            } else {
                "Memory scope narrowed."
            };
            Ok(ToolResult::ok(
                json!({"change_id":row.id,"status":row.status.to_string(),"message":message})
                    .to_string(),
            ))
        })
    }
}
