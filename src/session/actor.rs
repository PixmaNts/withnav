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
    build_actor_with_policy(
        workspace, config_dir, resolved, api_key, cancel, None, false,
    )
}

pub(super) fn build_child_actor(
    workspace: &Path,
    config_dir: &Path,
    resolved: ResolvedModel,
    api_key: String,
    cancel: CancellationToken,
    allowed_tools: std::collections::BTreeMap<String, bool>,
    read_only: bool,
) -> AgentActor {
    build_actor_with_policy(
        workspace,
        config_dir,
        resolved,
        api_key,
        cancel,
        Some(allowed_tools),
        read_only,
    )
}

fn build_actor_with_policy(
    workspace: &Path,
    config_dir: &Path,
    resolved: ResolvedModel,
    api_key: String,
    cancel: CancellationToken,
    allowed_tools: Option<std::collections::BTreeMap<String, bool>>,
    read_only: bool,
) -> AgentActor {
    let tool_context = ToolContext {
        binding: None,
        workspace: workspace.to_path_buf(),
        writable_roots: vec![workspace.to_path_buf(), config_dir.to_path_buf()],
        readable_roots: Vec::new(),
    };
    let max_message_tokens = resolved.model.max_message_tokens;
    let mut actor = AgentActor::new(
        build_registry(allowed_tools, read_only),
        tool_context,
        resolved.provider,
        api_key,
        resolved.model.model,
        resolved.model.reasoning_history,
        resolved.model.max_output_tokens,
        cancel,
    );
    actor.compaction_max_message_tokens = max_message_tokens;
    actor
}

fn build_registry(
    allowed_tools: Option<std::collections::BTreeMap<String, bool>>,
    read_only: bool,
) -> Registry {
    let mut registry = Registry::new();
    registry.register(Box::new(crate::tools::native::ReadTool));
    registry.register(Box::new(crate::tools::native::WriteTool));
    registry.register(Box::new(crate::tools::native::EditTool));
    registry.register(Box::new(crate::tools::bash::BashTool));
    registry.register(Box::new(crate::tools::native::GrepTool));
    registry.register(Box::new(crate::tools::glob::GlobTool));
    registry.register(Box::new(crate::tools::fetch::FetchTool::new()));
    if !read_only {
        registry.register(Box::new(crate::tools::control::AskTool));
    }
    if read_only {
        registry.retain(|name| !matches!(name, "write" | "edit" | "bash" | "ask" | "task"));
    }
    if let Some(allowed) = allowed_tools {
        registry.retain(|name| allowed.get(name).copied().unwrap_or(true));
    }
    registry
}
