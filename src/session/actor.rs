//! Building a session's actor and the small pieces `Service` mints it from:
//! the tool registry and the `session.start` candidate.

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::agent::actor::AgentActor;
use crate::bus::event::{EventKind, SessionStart, ROOT_ACTOR_ID};
use crate::bus::sequencer::Candidate;
use crate::provider::config::ResolvedModel;
use crate::tools::{Registry, ToolContext};

/// The `session.start` candidate: always seq 0 for a fresh session.
pub(super) fn start_candidate(workspace: &Path) -> Candidate {
    let payload = serde_json::to_value(SessionStart {
        workspace: workspace.display().to_string(),
        root_actor_id: ROOT_ACTOR_ID.into(),
    })
    .unwrap_or(serde_json::Value::Null);
    Candidate::new(EventKind::SessionStart, payload)
}

/// The agent actor for a session, mirroring `app.rs run_command`'s build:
/// 5 native tools + ask, workspace (+ config dir) writable. Profile policy is
/// applied afterwards by the session hooks.
pub(super) fn build_actor(
    workspace: &Path,
    config_dir: &Path,
    resolved: ResolvedModel,
    api_key: String,
    cancel: CancellationToken,
) -> AgentActor {
    let tool_context = ToolContext {
        workspace: workspace.to_path_buf(),
        writable_roots: vec![workspace.to_path_buf(), config_dir.to_path_buf()],
        readable_roots: Vec::new(),
    };
    AgentActor::new(
        build_registry(),
        tool_context,
        resolved.provider,
        api_key,
        resolved.model.model,
        resolved.model.reasoning_history,
        resolved.model.max_output_tokens,
        cancel,
    )
}

fn build_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(Box::new(crate::tools::native::ReadTool));
    registry.register(Box::new(crate::tools::native::WriteTool));
    registry.register(Box::new(crate::tools::native::EditTool));
    registry.register(Box::new(crate::tools::bash::BashTool));
    registry.register(Box::new(crate::tools::native::GrepTool));
    registry.register(Box::new(crate::tools::glob::GlobTool));
    registry.register(Box::new(crate::tools::fetch::FetchTool::new()));
    registry.register(Box::new(crate::tools::control::AskTool));
    registry
}
