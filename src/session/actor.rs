//! Building a session's actor and the small pieces `Service` mints it from:
//! the tool registry, the `session.start` candidate and the navigator feed's wake poster.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::message::accept;
use super::service::{Service, SessionHandle, SessionId};
use crate::agent::actor::AgentActor;
use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::{EventKind, SessionStart, ROOT_ACTOR_ID};
use crate::bus::sequencer::Candidate;
use crate::provider::config::ResolvedModel;
use crate::tools::{Registry, ToolContext};

/// The feed's wake poster: routes a navigator-sourced message through the
/// same core `POST /messages` uses. A session already closing swallows the
/// error (the wake has nowhere to land, not a fault worth logging).
pub(super) fn wake_poster(
    handle: SessionHandle,
    session_id: SessionId,
) -> crate::navigator::SessionPost {
    Arc::new(move |text| {
        let handle = handle.clone();
        Box::pin(async move {
            let command = Command {
                command_id: Ulid::new(),
                session_id,
                source_actor_id: crate::navigator::NAVIGATOR_ACTOR_ID.to_string(),
                target_actor_id: ROOT_ACTOR_ID.to_string(),
                mode: DeliveryMode::NextTurn,
                content: vec![ContentBlock::Text { text }],
                created_at: SystemTime::now(),
            };
            match accept(&handle, command).await {
                Ok(_) => Ok(()),
                Err(_) if handle.session_cancel.is_cancelled() => Ok(()),
                Err(err) => Err(err.to_string()),
            }
        })
    })
}

/// The subscriber task feeding a navigator session's events into the
/// conversation hub, until the session is cancelled or its stream ends.
pub(super) fn spawn_feed_watch(
    service: Service,
    feed: crate::navigator::ConversationFeed,
    session_cancel: CancellationToken,
    session_id: SessionId,
) {
    tokio::spawn(async move {
        // Replay first (a resume rebuilds the digest and the
        // active flag from the stored conversation), then live
        // until the session ends. A slow feed never slows the
        // sequencer: the bus disconnects it and the digest simply
        // stops growing at the tail.
        let Ok(subscription) = service.subscribe(session_id, None).await else {
            return;
        };
        if let crate::bus::ResumeOutcome::Replay(events) = subscription.replay {
            for event in &events {
                feed.observe_replayed(event).await;
            }
        }
        let mut live = match subscription.live {
            Some(live) => live,
            None => return,
        };
        loop {
            tokio::select! {
                _ = session_cancel.cancelled() => break,
                event = live.recv() => match event {
                    Some(event) => feed.observe(&event).await,
                    None => break,
                }
            }
        }
        feed.forget_session(session_id);
    });
}

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
    registry.register(Box::new(crate::tools::control::AskTool));
    registry
}
