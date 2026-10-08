//! Editor context for the session service: editor roots carry the
//! pair-programmer prompt and the `intent` tool, and drop a legacy
//! copy of that prompt from a restored transcript. When a conversation feed
//! is attached, a navigator session also registers with it and feeds it
//! the session's events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::agent::actor::AgentActor;
use crate::navigator::conversation::{ConversationFeed, SessionPost, NAVIGATOR_ACTOR_ID};
use crate::navigator::intent::IntentTool;
use crate::provider::openai::ChatMessage;
use crate::session::{SessionHooks, SessionSender, StartedSession, Subscription};

/// Pair-programmer context in an editor session, editable as Markdown.
pub const COGNITION_PROMPT: &str = include_str!("cognition.md");

/// [`SessionHooks`] for editor sessions; other sessions are left plain.
#[derive(Debug, Clone, Default)]
pub struct NavigatorHooks {
    feed: Option<ConversationFeed>,
    incarnations: Arc<Incarnations>,
}

impl NavigatorHooks {
    /// Hooks without a conversation feed: prompt and tools only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hooks that also attach navigator sessions to `feed`, so the language
    /// server can read the digest and wake the cognition.
    pub fn with_feed(feed: ConversationFeed) -> Self {
        Self {
            feed: Some(feed),
            ..Self::default()
        }
    }
}

/// Which incarnation of a session id the feed currently watches. A resumed
/// session reuses its id, so the old incarnation's cleanup must not drop the
/// new one's registration.
#[derive(Debug, Default)]
struct Incarnations {
    next: Mutex<(u64, HashMap<Ulid, u64>)>,
}

impl Incarnations {
    fn lock(&self) -> std::sync::MutexGuard<'_, (u64, HashMap<Ulid, u64>)> {
        self.next.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new incarnation of `id`; it supersedes any earlier one.
    fn begin(&self, id: Ulid) -> u64 {
        let mut guard = self.lock();
        guard.0 += 1;
        let incarnation = guard.0;
        guard.1.insert(id, incarnation);
        incarnation
    }

    /// Ends `incarnation`; true when it was still the current one.
    fn end(&self, id: Ulid, incarnation: u64) -> bool {
        let mut guard = self.lock();
        if guard.1.get(&id) == Some(&incarnation) {
            guard.1.remove(&id);
            true
        } else {
            false
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
/// stream ends. Replay first (a resume rebuilds the digest and the active
/// flag without announcing old turns), then live. A slow feed never slows
/// the sequencer: the bus disconnects it and the digest simply stops
/// growing at the tail.
async fn watch(feed: ConversationFeed, subscription: Subscription, cancel: CancellationToken) {
    if let crate::bus::ResumeOutcome::Replay(events) = subscription.replay {
        for event in &events {
            feed.observe_replayed(event).await;
        }
    }
    let Some(mut live) = subscription.live else {
        return;
    };
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            event = live.recv() => match event {
                Some(event) => feed.observe(&event).await,
                None => break,
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

    fn started(
        &self,
        info: StartedSession,
        sender: SessionSender,
        subscription: Subscription,
        cancel: CancellationToken,
    ) {
        let Some(feed) = self.feed.clone() else {
            return;
        };
        if !info.editor {
            return;
        }
        let id = info.id;
        let incarnation = self.incarnations.begin(id);
        feed.register_session(id, wake_poster(sender.clone(), cancel.clone()));
        feed.register_flow_poster(
            id,
            Arc::new(move |text, flow, run_id, wake| {
                sender.queue_flow_note_report(text, flow, run_id, wake)
            }),
        );
        let incarnations = self.incarnations.clone();
        tokio::spawn(async move {
            watch(feed.clone(), subscription, cancel).await;
            if incarnations.end(id, incarnation) {
                feed.forget_session(id);
            }
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
                } => Some((content, flow, run_id)),
                _ => None,
            })
            .collect();
        assert_eq!(notes.len(), 2);
        assert!(notes
            .iter()
            .all(|(text, flow, _)| text.starts_with("<untrusted-flow-note>")
                && *flow == "editor-review"));
        assert_eq!(notes[0].2, "note");
        assert_eq!(notes[1].2, "wake");
    }

    // Given a session id watched twice (a resume replaced the first
    // incarnation)
    // When the OLD incarnation ends after the new one began
    // Then it must not forget the registration; the new one's end does
    #[test]
    fn an_old_incarnation_does_not_forget_a_resumed_one() {
        let incarnations = Incarnations::default();
        let id = Ulid::new();
        let old = incarnations.begin(id);
        let new = incarnations.begin(id);
        assert!(!incarnations.end(id, old), "stale cleanup is a no-op");
        assert!(incarnations.end(id, new), "the current one cleans up");
        assert!(!incarnations.end(id, new), "and only once");
    }

    #[test]
    fn the_cognition_prompt_keeps_guidance_narrow_and_wakes_as_hypotheses() {
        assert!(COGNITION_PROMPT.contains("answer the immediate next step"));
        assert!(COGNITION_PROMPT.contains("Do not run project-wide checks"));
        assert!(COGNITION_PROMPT.contains("or that the Captain looks stuck"));
        assert!(COGNITION_PROMPT.contains("a hypothesis to check, never an order"));
    }
}
