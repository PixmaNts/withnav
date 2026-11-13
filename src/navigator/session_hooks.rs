//! Editor context for the session service: editor roots carry the
//! pair-programmer prompt and the `intent` and `reveal` tools, and drop a legacy
//! copy of that prompt from a restored transcript. When a conversation feed
//! is attached, a navigator session also registers with it and feeds it
//! the session's events.

use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::agent::actor::AgentActor;
use crate::navigator::conversation::{
    ConversationFeed, ConversationTarget, NAVIGATOR_ACTOR_ID, SessionPost, WorkspaceFeedRegistry,
};
use crate::navigator::intent::IntentTool;
use crate::navigator::reveal::RevealTool;
use crate::provider::openai::ChatMessage;
use crate::session::{SessionHooks, SessionSender, StartedSession, Subscription};

/// Pair-programmer context in an editor session, editable as Markdown.
pub const COGNITION_PROMPT: &str = include_str!("cognition.md");

/// [`SessionHooks`] for editor sessions; other sessions are left plain.
#[derive(Debug, Clone, Default)]
pub struct NavigatorHooks {
    feed: Option<ConversationFeed>,
    registry: Option<WorkspaceFeedRegistry>,
}

impl NavigatorHooks {
    /// Hooks without a conversation feed: prompt and tools only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hooks that attach navigator sessions to a single conversation `feed`.
    pub fn with_feed(feed: ConversationFeed) -> Self {
        Self {
            feed: Some(feed),
            registry: None,
        }
    }

    /// Hooks that select feeds through a workspace registry.
    pub fn with_registry(registry: WorkspaceFeedRegistry) -> Self {
        Self {
            feed: None,
            registry: Some(registry),
        }
    }

    /// Hooks with explicit feed and registry fallback.
    pub fn with_feed_and_registry(
        feed: Option<ConversationFeed>,
        registry: Option<WorkspaceFeedRegistry>,
    ) -> Self {
        Self { feed, registry }
    }

    /// Returns the active registry if one was configured.
    pub fn registry(&self) -> Option<&WorkspaceFeedRegistry> {
        self.registry.as_ref()
    }

    /// Resolves the conversation feed for a given workspace path.
    pub fn feed_for_workspace(&self, workspace: &Path) -> Option<ConversationFeed> {
        if let Some(registry) = &self.registry {
            Some(registry.get_or_create(workspace))
        } else {
            self.feed.clone()
        }
    }
}

/// The feed's wake poster: routes a navigator-sourced message through the
/// session's own command path. A session already closing swallows the error
/// (the wake has nowhere to land, not a fault worth logging).
fn wake_poster(sender: SessionSender, cancel: CancellationToken) -> SessionPost {
    Arc::new(move |text| {
        let sender = sender.clone();
        let cancel = cancel.clone();
        Box::pin(async move {
            match sender.send(text, NAVIGATOR_ACTOR_ID).await {
                Ok(()) => Ok(()),
                Err(_) if cancel.is_cancelled() => Ok(()),
                Err(err) => Err(err.to_string()),
            }
        })
    })
}

/// Feeds a session's events into the hub, until it is cancelled or its
/// stream ends. Replay first (a resume rebuilds the digest without announcing
/// old turns), then live. Disconnect, lag, or resync marks the incarnation unavailable.
async fn watch(
    feed: ConversationFeed,
    session_id: Ulid,
    incarnation: Ulid,
    subscription: Subscription,
    cancel: CancellationToken,
) {
    match subscription.replay {
        crate::bus::ResumeOutcome::Replay(events) => {
            for event in &events {
                feed.observe_replayed_incarnation(incarnation, event).await;
            }
        }
        crate::bus::ResumeOutcome::Resync | crate::bus::ResumeOutcome::InvalidCursor => {
            feed.invalidate_session_incarnation(session_id, incarnation);
            return;
        }
    }
    let Some(mut live) = subscription.live else {
        feed.invalidate_session_incarnation(session_id, incarnation);
        return;
    };
    if live.is_closed() {
        feed.invalidate_session_incarnation(session_id, incarnation);
        return;
    }
    feed.mark_incarnation_ready(session_id, incarnation);
    loop {
        if live.is_closed() {
            feed.invalidate_session_incarnation(session_id, incarnation);
            break;
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                feed.invalidate_session_incarnation(session_id, incarnation);
                break;
            }
            event = live.recv() => match event {
                Some(event) => feed.observe_incarnation(incarnation, &event).await,
                None => {
                    feed.invalidate_session_incarnation(session_id, incarnation);
                    break;
                }
            }
        }
    }
}

impl SessionHooks for NavigatorHooks {
    fn configure_actor(&self, editor: bool, actor: &mut AgentActor) {
        if !editor {
            return;
        }
        // The navigator role rides as a second system message after the
        // agent loop's own contract; it is not part of the transcript.
        actor.system_prompt = Some(COGNITION_PROMPT.to_string());
        // The intent record is the navigator's working memory: only a
        // editor-session agent may update it.
        actor.registry.register(Box::new(IntentTool));
        // Points the Captain's editor at a file and symbol.
        actor
            .registry
            .register(Box::new(RevealTool::new(self.feed.clone())));
        // A transcript saved before the role prompt left the transcript
        // starts with that prompt as a system message; drop it so the
        // actor does not send it twice.
        if matches!(
            actor.transcript.first(),
            Some(ChatMessage::System { content }) if content == COGNITION_PROMPT
        ) {
            actor.transcript.remove(0);
        }
    }

    fn configure_session_actor(&self, cx: &StartedSession, actor: &mut AgentActor) {
        if !cx.editor {
            return;
        }
        actor.system_prompt = Some(COGNITION_PROMPT.to_string());
        actor.registry.register(Box::new(IntentTool));
        let feed = self.feed_for_workspace(&cx.workspace);
        let target = ConversationTarget {
            workspace: cx.workspace.clone(),
            session_id: cx.id,
            incarnation: cx.incarnation,
        };
        actor
            .registry
            .register(Box::new(RevealTool::for_session(feed, target)));
        if matches!(
            actor.transcript.first(),
            Some(ChatMessage::System { content }) if content == COGNITION_PROMPT
        ) {
            actor.transcript.remove(0);
        }
    }

    fn started(
        &self,
        info: StartedSession,
        sender: SessionSender,
        subscription: Subscription,
        cancel: CancellationToken,
    ) {
        let Some(feed) = self.feed_for_workspace(&info.workspace) else {
            return;
        };
        let id = info.id;
        let incarnation = info.incarnation;
        feed.register_session_incarnation(
            id,
            incarnation,
            wake_poster(sender.clone(), cancel.clone()),
            false,
        );
        feed.register_flow_poster_incarnation(
            id,
            incarnation,
            Arc::new(move |text, flow, run_id, wake| {
                sender.queue_flow_note_report(text, flow, run_id, wake)
            }),
        );
        tokio::spawn(async move {
            watch(feed.clone(), id, incarnation, subscription, cancel).await;
            feed.unbind_editor_incarnation(id, incarnation);
            feed.forget_session_incarnation(id, incarnation);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn editor_flow_poster_keeps_provenance_without_publishing_message_received() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.json"),
            r#"{"agents":{"build":{"model":"mock/test"}}}"#,
        )
        .unwrap();
        crate::provider::auth::set_key(&config_dir, "mock", "test").unwrap();
        let models_path = dir.path().join("models.json");
        std::fs::write(&models_path, r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/test":{"provider":"mock","model":"test","context_window":128000}}}"#).unwrap();
        let feed = ConversationFeed::new();
        let service = crate::session::Service::with_hooks(
            crate::session::SessionConfig {
                models_path: Some(models_path),
                config_dir,
                history_db: dir.path().join("history.db"),
                data_dir: dir.path().join("data"),
                rules_roots: Default::default(),
            },
            Arc::new(NavigatorHooks::with_feed(feed.clone())),
            Arc::new(crate::provider::openai::ClientCache::new()),
        )
        .unwrap();
        let id = service
            .create_session(dir.path(), None, true)
            .await
            .unwrap();
        let handle = service.handle(id).unwrap();
        let mut live = handle.sequencer.subscribe_live().await.unwrap().receiver;
        feed.observe(&crate::bus::event::EventEnvelope::new(
            id,
            0,
            crate::bus::event::EventKind::MessageReceived,
            serde_json::json!({"source":"user:test", "content":"captain task"}),
        ))
        .await;
        feed.post_flow_note(
            "queued evidence".into(),
            "editor-review".into(),
            "note".into(),
            false,
        )
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), live.recv())
                .await
                .is_err()
        );
        feed.post_flow_note(
            "wake evidence".into(),
            "editor-review".into(),
            "wake".into(),
            true,
        )
        .unwrap();
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), live.recv())
                .await
                .unwrap()
                .unwrap();
            assert_ne!(event.kind, "message.received");
            if event.kind == "turn.end" {
                break;
            }
        }
        service.close_all().await;
        let transcript = service
            .history()
            .load(id)
            .await
            .unwrap()
            .unwrap()
            .transcript;
        let notes: Vec<_> = transcript
            .iter()
            .filter_map(|message| match message {
                ChatMessage::User {
                    content,
                    provenance:
                        crate::provider::openai::UserProvenance::Flow {
                            source: crate::provider::openai::FlowSource::Flow,
                            flow,
                            run_id,
                        },
                    ..
                } => Some((content, flow, run_id)),
                _ => None,
            })
            .collect();
        assert_eq!(notes.len(), 2);
        assert!(
            notes
                .iter()
                .all(|(text, flow, _)| text.starts_with("<untrusted-flow-note>")
                    && *flow == "editor-review")
        );
        assert_eq!(notes[0].2, "note");
        assert_eq!(notes[1].2, "wake");
    }

    #[tokio::test]
    async fn terminal_root_session_is_registered_in_workspace_feed_without_actor_tools() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.json"),
            r#"{"agents":{"build":{"model":"mock/test"}}}"#,
        )
        .unwrap();
        crate::provider::auth::set_key(&config_dir, "mock", "test").unwrap();
        let models_path = dir.path().join("models.json");
        std::fs::write(&models_path, r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/test":{"provider":"mock","model":"test","context_window":128000}}}"#).unwrap();
        let registry = WorkspaceFeedRegistry::new();
        let service = crate::session::Service::with_hooks(
            crate::session::SessionConfig {
                models_path: Some(models_path),
                config_dir,
                history_db: dir.path().join("history.db"),
                data_dir: dir.path().join("data"),
                rules_roots: Default::default(),
            },
            Arc::new(NavigatorHooks::with_registry(registry.clone())),
            Arc::new(crate::provider::openai::ClientCache::new()),
        )
        .unwrap();

        // Terminal session: editor = false
        let id = service
            .create_session(dir.path(), None, false)
            .await
            .unwrap();

        let feed = registry
            .get(dir.path())
            .expect("feed must be registered for workspace");
        tokio::task::yield_now().await;
        feed.observe(&crate::bus::event::EventEnvelope::new(
            id,
            0,
            crate::bus::event::EventKind::MessageReceived,
            serde_json::json!({"source":"user:cli", "content":"terminal work"}),
        ))
        .await;

        assert_eq!(feed.active_session(), Some(id));
        assert!(feed.digest().contains("captain: terminal work"));

        // Verify session was created and registered in workspace feed
        let handle = service.handle(id).unwrap();
        assert_eq!(handle.workspace, dir.path());

        service.close_all().await;
    }

    #[test]
    fn an_old_incarnation_does_not_forget_a_resumed_one() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        let old_inc = Ulid::generate();
        let new_inc = Ulid::generate();

        feed.register_session_incarnation(
            id,
            old_inc,
            Arc::new(|_| Box::pin(async { Ok(()) })),
            true,
        );
        feed.register_session_incarnation(
            id,
            new_inc,
            Arc::new(|_| Box::pin(async { Ok(()) })),
            true,
        );

        assert!(
            !feed.forget_session_incarnation(id, old_inc),
            "stale cleanup is a no-op"
        );
        assert!(
            feed.is_incarnation_available(id, new_inc),
            "new incarnation still present"
        );
        assert!(
            feed.forget_session_incarnation(id, new_inc),
            "current incarnation cleans up"
        );
        assert!(
            !feed.forget_session_incarnation(id, new_inc),
            "and only once"
        );
    }

    #[tokio::test]
    async fn live_receiver_closure_or_resync_invalidates_incarnation() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        let inc = Ulid::generate();
        feed.register_session_incarnation(id, inc, Arc::new(|_| Box::pin(async { Ok(()) })), false);

        // Case 1: Resync outcome invalidates immediately
        let sub_resync = Subscription {
            replay: crate::bus::ResumeOutcome::Resync,
            live: None,
        };
        watch(feed.clone(), id, inc, sub_resync, CancellationToken::new()).await;
        assert!(!feed.is_incarnation_available(id, inc));

        // Re-register a fresh incarnation
        let inc2 = Ulid::generate();
        feed.register_session_incarnation(
            id,
            inc2,
            Arc::new(|_| Box::pin(async { Ok(()) })),
            false,
        );

        // Case 2: Live receiver closed with buffered events present
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(crate::bus::event::EventEnvelope::new(
            id,
            1,
            crate::bus::event::EventKind::MessageReceived,
            serde_json::json!({"source":"user:test", "content":"buffered"}),
        ))
        .await
        .unwrap();
        drop(tx); // channel is closed, but has 1 buffered event
        let sub_closed = Subscription {
            replay: crate::bus::ResumeOutcome::Replay(Vec::new()),
            live: Some(rx),
        };
        watch(feed.clone(), id, inc2, sub_closed, CancellationToken::new()).await;
        assert!(
            !feed.is_incarnation_available(id, inc2),
            "closed receiver with buffered events must invalidate before ready"
        );
    }

    #[tokio::test]
    async fn with_feed_actor_reveal_preserves_compatibility_with_workspace_target() {
        use crate::tools::Tool;

        let dir = tempfile::tempdir().unwrap();
        let target_file = dir.path().join("main.rs");
        std::fs::write(&target_file, "fn main() {}\n").unwrap();

        let config_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.json"),
            r#"{"agents":{"build":{"model":"mock/test"}}}"#,
        )
        .unwrap();
        crate::provider::auth::set_key(&config_dir, "mock", "test").unwrap();
        let models_path = dir.path().join("models.json");
        std::fs::write(
            &models_path,
            r#"{"providers":{"mock":{"base_url":"http://127.0.0.1:1/v1"}},"models":{"mock/test":{"provider":"mock","model":"test","context_window":128000}}}"#,
        )
        .unwrap();

        // Feed constructed with empty workspace (standard NavigatorHooks::with_feed mode)
        let feed = ConversationFeed::new();
        feed.set_editor(Arc::new(|_params| {
            Box::pin(async move {
                Ok(crate::navigator::lsp::protocol::RevealResult {
                    group: Some(1),
                    symbol_found: Some(true),
                    error: None,
                })
            })
        }));

        let service = crate::session::Service::with_hooks(
            crate::session::SessionConfig {
                models_path: Some(models_path),
                config_dir,
                history_db: dir.path().join("history.db"),
                data_dir: dir.path().join("data"),
                rules_roots: Default::default(),
            },
            Arc::new(NavigatorHooks::with_feed(feed.clone())),
            Arc::new(crate::provider::openai::ClientCache::new()),
        )
        .unwrap();

        // Editor session started in dir.path()
        let id = service
            .create_session(dir.path(), None, true)
            .await
            .unwrap();

        let target = ConversationTarget {
            workspace: dir.path().to_path_buf(),
            session_id: id,
            incarnation: service.handle(id).unwrap().incarnation,
        };
        let tool = RevealTool::for_session(Some(feed), target);
        let cx = crate::tools::ToolContext {
            secrets: None,
            binding: None,
            workspace: dir.path().to_path_buf(),
            writable_roots: vec![dir.path().to_path_buf()],
            readable_roots: Vec::new(),
        };

        // Wait for watch loop to process initial start event and mark incarnation ready
        tokio::task::yield_now().await;

        let result = tool
            .call(
                serde_json::json!({"path": "main.rs"}),
                CancellationToken::new(),
                &cx,
            )
            .await
            .unwrap();

        assert_eq!(result.status, crate::tools::ToolStatus::Ok);
        assert!(result.output.contains("revealed in the editor: main.rs"));

        service.close_all().await;
    }

    #[test]
    fn the_cognition_prompt_keeps_guidance_narrow_and_wakes_as_hypotheses() {
        assert!(COGNITION_PROMPT.contains("answer the immediate next step"));
        assert!(COGNITION_PROMPT.contains("Do not run project-wide checks"));
        assert!(COGNITION_PROMPT.contains("or that the Captain looks stuck"));
        assert!(COGNITION_PROMPT.contains("a hypothesis to check, never an order"));
    }
}
