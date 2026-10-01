//! Building a session's actor and the small pieces `Service` mints it from:
//! the tool registry, the role prompt, the transcript restored on resume, the
//! `session.start` candidate and the navigator feed's wake poster.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::message::accept;
use super::service::{SessionHandle, SessionId};
use crate::agent::actor::AgentActor;
use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::{EventKind, SessionStart, ROOT_ACTOR_ID};
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::provider::config::ResolvedModel;
use crate::provider::openai::ChatMessage;
use crate::tools::{Registry, ToolContext};

/// Session profile selecting the navigator's cognition: the `cognition`
/// model alias by default and the pair-programmer system prompt.
pub const NAVIGATOR_PROFILE: &str = "navigator";

/// The cognition's role in a navigator session, editable as Markdown.
pub const COGNITION_PROMPT: &str = include_str!("../navigator/cognition.md");

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
    feed: crate::navigator::ConversationFeed,
    sequencer: Sequencer,
    session_cancel: CancellationToken,
    session_id: SessionId,
) {
    tokio::spawn(async move {
        // Replay first (a resume rebuilds the digest and the
        // active flag from the stored conversation), then live
        // until the session ends. A slow feed never slows the
        // sequencer: the bus disconnects it and the digest simply
        // stops growing at the tail.
        let Ok(reply) = sequencer.subscribe_since(None).await else {
            return;
        };
        if let crate::bus::ResumeOutcome::Replay(events) = reply.replay {
            for event in &events {
                feed.observe_replayed(event).await;
            }
        }
        let mut live = match reply.live {
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
/// 5 native tools + ask, workspace (+ config dir) writable.
pub(super) fn build_actor(
    workspace: &Path,
    config_dir: &Path,
    resolved: ResolvedModel,
    api_key: String,
    cancel: CancellationToken,
    navigator: bool,
) -> AgentActor {
    let tool_context = ToolContext {
        workspace: workspace.to_path_buf(),
        writable_roots: vec![workspace.to_path_buf(), config_dir.to_path_buf()],
    };
    let mut actor = AgentActor::new(
        build_registry(navigator),
        tool_context,
        resolved.provider,
        api_key,
        resolved.model.model,
        resolved.model.reasoning_history,
        resolved.model.max_output_tokens,
        cancel,
    );
    if navigator {
        // The navigator role rides as a second system message after the
        // agent loop's own contract; it is not part of the transcript.
        actor = actor.with_system_prompt(COGNITION_PROMPT);
    }
    actor
}

/// The stored transcript for a resumed session. A navigator transcript saved
/// before the role prompt left the transcript starts with that prompt as a
/// system message; it is dropped so the actor does not send it twice.
pub(super) fn restore_transcript(stored: &[ChatMessage], navigator: bool) -> Vec<ChatMessage> {
    let legacy_prompt = navigator
        && matches!(
            stored.first(),
            Some(ChatMessage::System { content }) if content == COGNITION_PROMPT
        );
    let skip = usize::from(legacy_prompt);
    stored[skip..].to_vec()
}

fn build_registry(navigator: bool) -> Registry {
    let mut registry = Registry::new();
    registry.register(Box::new(crate::tools::native::ReadTool));
    registry.register(Box::new(crate::tools::native::WriteTool));
    registry.register(Box::new(crate::tools::native::EditTool));
    registry.register(Box::new(crate::tools::bash::BashTool));
    registry.register(Box::new(crate::tools::native::GrepTool));
    registry.register(Box::new(crate::tools::control::AskTool));
    if navigator {
        // The intent record is the navigator's working memory: only a
        // navigator-profile cognition may update it.
        registry.register(Box::new(crate::navigator::intent::IntentTool));
    }
    registry
}
