//! The conversation feed: the in-process seam between the session server
//! (where the cognition's sessions live) and the language server (where the
//! fast layers watch the driver). One hub shared by both — the whole
//! navigator runs in one process over two channels.
//!
//! It tracks the ACTIVE conversation — the latest editor session
//! that received a driver message — keeps a bounded digest of it, remembers
//! whether the cognition is waiting on an `ask`, and posts wakes: short
//! navigator-sourced messages routed in-process, never over HTTP, so the
//! durable `message.received.source` keeps telling the panel and this feed
//! who wrote what.
//!
//! The ask guard lives here, where the wake is posted: the first message
//! after a yield answers the pending `ask`, and a navigator message must
//! never become that answer. So while the active conversation waits on the
//! driver, wakes are queued — coalesced, latest wins — and delivered right
//! after the next driver message has been accepted. (An open permission
//! request is guarded by the session runtime, not here: it holds a
//! navigator-sourced message until nothing is pending.) A navigator-sourced
//! message is also never a conversation trigger: the digest marks it as the
//! navigator's own words, and the turn it provokes is reported as not
//! driver-caused, so a wake never becomes a conversation trigger (no
//! self-reinforcing loops).
//!
//! The language server subscribes ([`ConversationFeed::subscribe`]) to hear
//! when a turn ends and whether the driver caused it. Replayed events
//! rebuild state but stay silent: a resumed session must not re-trigger
//! the turns it already lived.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;
use ulid::Ulid;

use crate::bus::event::EventEnvelope;
use crate::navigator::log::excerpt;

/// The actor id of the navigator itself: the sender of in-process wakes,
/// never choosable by an HTTP client (the server assigns `user:http`
/// there, so the two provenances cannot be confused on the wire).
pub const NAVIGATOR_ACTOR_ID: &str = "navigator";

/// How many exchanges of the active conversation the digest keeps.
const MAX_EXCHANGES: usize = 12;

/// Each exchange's text is cut around this many characters: enough to
/// recognize what was agreed, not enough to mirror the whole chat.
const MAX_EXCHANGE_CHARS: usize = 600;

/// Posts one navigator-sourced message into a live session. Built by the
/// session server around a session handle; the feed stays independent of
/// the server types.
pub type SessionPost =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Typed flow effects bypass the message inbox; notes need not start a turn.
pub(crate) type FlowPost =
    Arc<dyn Fn(String, String, String, bool) -> Result<(), String> + Send + Sync>;

/// What came of posting a wake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    /// Posted as a navigator-sourced message to this session.
    Posted(Ulid),
    /// The cognition is waiting on the driver: queued (latest wins), to be
    /// delivered right after the next driver message.
    Queued(Ulid),
    /// No active conversation in this process.
    NoSession,
    /// The session exists but refused the post (stopping, full inbox).
    Failed(Ulid, String),
}

/// What the feed tells its subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationEvent {
    /// A navigator session finished a turn. `caused_by_driver` is true only
    /// when the turn answered a driver message (`message.received` whose
    /// source starts with `user:`); a turn provoked by a navigator wake is
    /// false, which keeps wakes from triggering themselves.
    TurnEnded {
        session_id: Ulid,
        caused_by_driver: bool,
    },
}

/// Capacity of the subscription channel; a lagging subscriber skips ahead.
const EVENT_CAPACITY: usize = 32;

/// Who said it, in the digest's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Author {
    Driver,
    Cognition,
    Navigator,
    Subagent,
}

impl Author {
    fn as_str(self) -> &'static str {
        match self {
            Author::Driver => "captain",
            Author::Cognition => "cognition",
            Author::Navigator => "navigator",
            Author::Subagent => "subagent (untrusted)",
        }
    }
}

#[derive(Debug, Clone)]
struct Exchange {
    author: Author,
    text: String,
}

/// One watched navigator session: its poster, its ask state, its queued
/// wake and its bounded digest.
struct SessionConversation {
    flow_post: Option<FlowPost>,
    post: SessionPost,
    waiting_on_driver: bool,
    queued_wake: Option<String>,
    exchanges: VecDeque<Exchange>,
    /// A driver message arrived since the last `turn.end`: the next turn
    /// end answers the driver.
    driver_pending: bool,
}

impl SessionConversation {
    fn push_exchange(&mut self, author: Author, text: &str) {
        // Only driver messages carry the editor-context block the editor
        // appends at send time; it describes the editor, not the driver.
        let text = match author {
            Author::Driver => strip_editor_context(text),
            _ => text.to_string(),
        };
        let text = excerpt(text.trim(), MAX_EXCHANGE_CHARS);
        self.exchanges.push_back(Exchange { author, text });
        while self.exchanges.len() > MAX_EXCHANGES {
            self.exchanges.pop_front();
        }
    }
}

#[derive(Default)]
struct FeedState {
    sessions: HashMap<Ulid, SessionConversation>,
    /// The latest navigator session that received a driver message.
    active: Option<Ulid>,
}

/// The shared hub. Cheap to clone (one `Arc`); every method is safe under
/// concurrency. Locks are standard mutexes held only across synchronous
/// steps — awaits happen after the guard is dropped.
#[derive(Clone)]
pub struct ConversationFeed {
    inner: Arc<Mutex<FeedState>>,
    events: broadcast::Sender<ConversationEvent>,
}

impl Default for ConversationFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ConversationFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("ConversationFeed")
            .field(
                "active",
                &state.active.map(|id| id.to_string()).unwrap_or_default(),
            )
            .field("sessions", &state.sessions.len())
            .finish_non_exhaustive()
    }
}

impl ConversationFeed {
    pub fn new() -> Self {
        Self {
            inner: Arc::default(),
            events: broadcast::channel(EVENT_CAPACITY).0,
        }
    }

    /// A new subscription to the feed's [`ConversationEvent`]s, starting
    /// now. Cheap; any number of subscribers.
    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent> {
        self.events.subscribe()
    }

    /// Registers a navigator session the feed can watch and wake. Called by
    /// the session server when the session is spawned, before any of its
    /// events exist.
    pub fn register_session(&self, session_id: Ulid, post: SessionPost) {
        self.lock().sessions.insert(
            session_id,
            SessionConversation {
                flow_post: None,
                post,
                waiting_on_driver: false,
                queued_wake: None,
                exchanges: VecDeque::new(),
                driver_pending: false,
            },
        );
    }

    pub(crate) fn register_flow_poster(&self, session_id: Ulid, post: FlowPost) {
        if let Some(entry) = self.lock().sessions.get_mut(&session_id) {
            entry.flow_post = Some(post);
        }
    }

    pub(crate) fn post_flow_note(
        &self,
        text: String,
        flow: String,
        run_id: String,
        wake: bool,
    ) -> Result<(), String> {
        let post = {
            let state = self.lock();
            state
                .active
                .and_then(|id| state.sessions.get(&id))
                .and_then(|entry| entry.flow_post.clone())
        }
        .ok_or_else(|| "no active flow session".to_string())?;
        post(text, flow, run_id, wake)
    }

    /// Drops a session (it ended); its queued wake, if any, goes with it.
    pub fn forget_session(&self, session_id: Ulid) {
        let mut state = self.lock();
        state.sessions.remove(&session_id);
        if state.active == Some(session_id) {
            state.active = None;
        }
    }

    /// Feeds one durable session event into the hub. The session server
    /// runs a subscriber per navigator session; replays flow through here
    /// too, so a resumed conversation rebuilds its digest and active flag.
    ///
    /// Of the whole envelope stream only a few kinds carry meaning:
    /// `message.received` (who wrote what, and the end of a pending ask),
    /// `llm.response` (the cognition's own words), `agent.yield` (the
    /// cognition now waits on the driver) and `turn.end` (announced to
    /// subscribers). A driver message also makes its session the active
    /// conversation and delivers a queued wake.
    pub async fn observe(&self, envelope: &EventEnvelope) {
        self.apply(envelope, true).await;
    }

    /// Like [`observe`](Self::observe) for events read back from storage:
    /// state is rebuilt, nothing is announced to subscribers.
    pub async fn observe_replayed(&self, envelope: &EventEnvelope) {
        self.apply(envelope, false).await;
    }

    async fn apply(&self, envelope: &EventEnvelope, announce: bool) {
        match envelope.kind.as_str() {
            "message.received" => self.observe_message(envelope).await,
            "turn.end" => {
                let caused_by_driver = {
                    let mut state = self.lock();
                    match state.sessions.get_mut(&envelope.session_id) {
                        Some(entry) => std::mem::take(&mut entry.driver_pending),
                        None => return,
                    }
                };
                if announce {
                    // No subscriber is not an error.
                    let _ = self.events.send(ConversationEvent::TurnEnded {
                        session_id: envelope.session_id,
                        caused_by_driver,
                    });
                }
            }
            "llm.response" => {
                let text = envelope
                    .payload
                    .get("message")
                    .and_then(|m| m.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if text.trim().is_empty() {
                    return;
                }
                let mut state = self.lock();
                if let Some(entry) = state.sessions.get_mut(&envelope.session_id) {
                    entry.push_exchange(Author::Cognition, text);
                }
            }
            "agent.yield" => {
                let mut state = self.lock();
                if let Some(entry) = state.sessions.get_mut(&envelope.session_id) {
                    entry.waiting_on_driver = true;
                }
            }
            "command.result"
                if envelope.payload["name"] == "watchdog"
                    && envelope.payload["data"]["resolution"].is_string() =>
            {
                let mut state = self.lock();
                if let Some(entry) = state.sessions.get_mut(&envelope.session_id) {
                    entry.waiting_on_driver = false;
                }
            }
            _ => {}
        }
    }

    /// One `message.received`: records the exchange, switches the active
    /// conversation on a driver message, ends a pending ask and delivers a
    /// queued wake right after it.
    async fn observe_message(&self, envelope: &EventEnvelope) {
        let source = envelope
            .payload
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = envelope
            .payload
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if content.trim().is_empty() {
            return;
        }
        let driver = crate::bus::command::is_driver_source(source);
        let flush = {
            let mut state = self.lock();
            let Some(entry) = state.sessions.get_mut(&envelope.session_id) else {
                // An unregistered session must not become the active
                // conversation, even on a driver message.
                return;
            };
            if driver {
                // A driver message makes this session the active
                // conversation and answers any pending ask: the wake
                // queued behind that ask can finally be delivered.
                entry.waiting_on_driver = false;
                entry.driver_pending = true;
            }
            entry.push_exchange(
                if driver {
                    Author::Driver
                } else if matches!(source, "subagent" | "user:subagent") {
                    Author::Subagent
                } else {
                    Author::Navigator
                },
                content,
            );
            let queued = if driver {
                entry.queued_wake.take()
            } else {
                None
            };
            let post = queued.map(|text| (entry.post.clone(), text));
            if driver {
                state.active = Some(envelope.session_id);
            }
            post
        };
        if let Some((post, text)) = flush {
            if let Err(error) = post(text).await {
                tracing::warn!(
                    session = %envelope.session_id,
                    error = %error,
                    "queued wake delivery failed"
                );
            }
        }
    }

    /// The active conversation's session id, if any driver message has
    /// reached a navigator session in this process.
    pub fn active_session(&self) -> Option<Ulid> {
        self.lock().active
    }

    /// Whether the active conversation's cognition is waiting on an `ask`
    /// the driver has not answered yet. False when there is no active
    /// conversation.
    pub fn is_waiting_on_driver(&self) -> bool {
        let state = self.lock();
        state
            .active
            .and_then(|id| state.sessions.get(&id))
            .is_some_and(|entry| entry.waiting_on_driver)
    }

    /// The active conversation's digest: plain text lines, newest last,
    /// `author: text`. Empty when there is no active conversation.
    pub fn digest(&self) -> String {
        let state = self.lock();
        match state.active.and_then(|id| state.sessions.get(&id)) {
            Some(entry) => entry
                .exchanges
                .iter()
                .map(|e| format!("{}: {}", e.author.as_str(), e.text))
                .collect::<Vec<_>>()
                .join("\n"),
            None => String::new(),
        }
    }

    /// Posts a wake to the active conversation as a navigator-sourced
    /// message. The ask guard: while the cognition waits on the driver, the
    /// wake is queued instead (coalesced, latest wins) and delivered by the
    /// next driver message. A permission request needs no guard here: the
    /// session runtime holds a navigator message while a request or an ask
    /// is pending, and delivers it once the turn has nothing left to wait on.
    pub async fn post_wake(&self, text: &str) -> WakeOutcome {
        let (session_id, post) = {
            let mut state = self.lock();
            let Some(session_id) = state.active else {
                return WakeOutcome::NoSession;
            };
            let Some(entry) = state.sessions.get_mut(&session_id) else {
                state.active = None;
                return WakeOutcome::NoSession;
            };
            if entry.waiting_on_driver {
                entry.queued_wake = Some(text.to_string());
                return WakeOutcome::Queued(session_id);
            }
            (session_id, entry.post.clone())
        };
        match post(text.to_string()).await {
            Ok(()) => WakeOutcome::Posted(session_id),
            Err(error) => WakeOutcome::Failed(session_id, error),
        }
    }

    /// Poison recovery via into_inner: a panicking holder must not wedge
    /// the feed (same policy as the session registry).
    fn lock(&self) -> MutexGuard<'_, FeedState> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Removes the `<editor-context>` block the editor appends to driver
/// messages: it describes the editor state at send time, not what the
/// driver said.
fn strip_editor_context(text: &str) -> String {
    const OPEN: &str = "<editor-context>";
    const CLOSE: &str = "</editor-context>";
    let Some(open) = text.find(OPEN) else {
        return text.to_string();
    };
    let after = &text[open + OPEN.len()..];
    let cut = match after.find(CLOSE) {
        Some(end) => open + OPEN.len() + end + CLOSE.len(),
        // Unterminated block: everything after the opening tag is block.
        None => text.len(),
    };
    let mut out = format!("{}{}", &text[..open], &text[cut..]);
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::event::EventKind;
    use serde_json::json;

    fn message_envelope(session: Ulid, source: &str, content: &str) -> EventEnvelope {
        EventEnvelope::new(
            session,
            0,
            EventKind::MessageReceived,
            json!({
                "command_id": Ulid::new().to_string(),
                "source": source,
                "target": "agent:main",
                "mode": "next-turn",
                "content": content,
            }),
        )
    }

    fn response_envelope(session: Ulid, text: &str) -> EventEnvelope {
        EventEnvelope::new(
            session,
            0,
            EventKind::LlmResponse,
            json!({ "turn_id": Ulid::new().to_string(), "step_id": Ulid::new().to_string(), "message": { "text": text } }),
        )
    }

    fn yield_envelope(session: Ulid) -> EventEnvelope {
        EventEnvelope::new(
            session,
            0,
            EventKind::AgentYield,
            json!({ "actor_id": "agent:main", "turn_id": Ulid::new().to_string(), "question": "which one?" }),
        )
    }

    /// A poster that records every text it is handed and always succeeds.
    fn recording_poster(sink: Arc<Mutex<Vec<String>>>) -> SessionPost {
        Arc::new(move |text| {
            sink.lock().unwrap().push(text);
            Box::pin(async { Ok::<(), String>(()) })
        })
    }

    #[test]
    fn editor_context_block_is_stripped_from_driver_text() {
        let raw = "add the command\n\n<editor-context>\nfocused: src/main.rs\nopen files (1): src/main.rs\n</editor-context>";
        assert_eq!(
            strip_editor_context(raw),
            "add the command",
            "the block and its padding disappear"
        );
        assert_eq!(strip_editor_context("no block here"), "no block here");
        assert_eq!(
            strip_editor_context("cut <editor-context> unterminated"),
            "cut"
        );
    }

    #[tokio::test]
    async fn digest_records_authors_newest_last_and_strips_the_block() {
        let feed = ConversationFeed::new();
        let id = Ulid::new();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted.clone()));

        feed.observe(&message_envelope(
            id,
            "user:http",
            "add the command\n\n<editor-context>\nfocused: x\n</editor-context>",
        ))
        .await;
        feed.observe(&response_envelope(id, "start in src/cli.rs"))
            .await;
        feed.observe(&message_envelope(id, NAVIGATOR_ACTOR_ID, "wake: drift"))
            .await;

        assert_eq!(feed.active_session(), Some(id));
        let digest = feed.digest();
        assert_eq!(
            digest,
            "driver: add the command\ncognition: start in src/cli.rs\nnavigator: wake: drift",
            "plain lines, newest last, block stripped"
        );
    }

    #[tokio::test]
    async fn digest_is_bounded_to_the_last_twelve_exchanges() {
        let feed = ConversationFeed::new();
        let id = Ulid::new();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted));
        for i in 0..20 {
            feed.observe(&message_envelope(id, "user:http", &format!("m{i}")))
                .await;
        }
        let digest = feed.digest();
        let lines: Vec<&str> = digest.lines().collect();
        assert_eq!(lines.len(), MAX_EXCHANGES, "only the last twelve remain");
        assert_eq!(lines.first().unwrap(), &"driver: m8");
        assert_eq!(lines.last().unwrap(), &"driver: m19");
    }

    #[tokio::test]
    async fn long_texts_are_cut_near_six_hundred_characters() {
        let feed = ConversationFeed::new();
        let id = Ulid::new();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted));
        let long = "x".repeat(5_000);
        feed.observe(&message_envelope(id, "user:http", &long))
            .await;
        let line = feed.digest();
        assert!(line.chars().count() < MAX_EXCHANGE_CHARS + 16);
        assert!(line.ends_with('…'), "the cut is marked");
    }

    #[tokio::test]
    async fn active_conversation_switches_on_a_driver_message_to_another_session() {
        let feed = ConversationFeed::new();
        let a = Ulid::new();
        let b = Ulid::new();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(a, recording_poster(posted.clone()));
        feed.register_session(b, recording_poster(posted));

        feed.observe(&message_envelope(a, "user:http", "first conversation"))
            .await;
        assert_eq!(feed.active_session(), Some(a));
        // A navigator-sourced message does NOT steal the active spot.
        feed.observe(&message_envelope(b, NAVIGATOR_ACTOR_ID, "wake"))
            .await;
        assert_eq!(feed.active_session(), Some(a));

        feed.observe(&message_envelope(b, "user:http", "second conversation"))
            .await;
        assert_eq!(feed.active_session(), Some(b));
        // The navigator message to B earlier stays in B's digest; the
        // ACTIVE digest is B's now.
        assert_eq!(
            feed.digest(),
            "navigator: wake\ndriver: second conversation"
        );
    }

    #[tokio::test]
    async fn queued_wakes_coalesce_and_deliver_after_the_driver_answers() {
        let feed = ConversationFeed::new();
        let id = Ulid::new();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted.clone()));

        feed.observe(&message_envelope(id, "user:http", "add a command"))
            .await;
        assert!(!feed.is_waiting_on_driver());

        feed.observe(&yield_envelope(id)).await;
        assert!(feed.is_waiting_on_driver(), "agent.yield starts the wait");

        assert_eq!(
            feed.post_wake("first wake").await,
            WakeOutcome::Queued(id),
            "a wake while waiting is queued, not posted"
        );
        assert_eq!(feed.post_wake("latest wake").await, WakeOutcome::Queued(id));
        assert!(posted.lock().unwrap().is_empty(), "nothing posted yet");

        // The driver's answer ends the wait and delivers the LATEST wake;
        // what the poster emitted is what the session's subscriber would
        // observe as the navigator's own `message.received`.
        feed.observe(&message_envelope(id, "user:http", "the auth remove one"))
            .await;
        assert!(!feed.is_waiting_on_driver());
        assert_eq!(
            posted.lock().unwrap().clone(),
            vec!["latest wake".to_string()],
            "coalesced: latest wins, delivered once"
        );
        feed.observe(&message_envelope(id, NAVIGATOR_ACTOR_ID, "latest wake"))
            .await;
        let digest = feed.digest();
        assert!(digest.contains("navigator: latest wake"));
    }

    #[tokio::test]
    async fn watchdog_resolution_clears_waiting_on_driver() {
        let feed = ConversationFeed::new();
        let id = Ulid::new();
        feed.register_session(id, recording_poster(Arc::new(Mutex::new(Vec::new()))));
        feed.observe(&message_envelope(id, "user:http", "work"))
            .await;
        feed.observe(&yield_envelope(id)).await;
        assert!(feed.is_waiting_on_driver());
        let mut resolved = yield_envelope(id);
        resolved.kind = "command.result".into();
        resolved.payload = serde_json::json!({"name":"watchdog","data":{"resolution":"continued"}});
        feed.observe(&resolved).await;
        assert!(!feed.is_waiting_on_driver());
    }

    #[tokio::test]
    async fn paused_turn_supersession_keeps_replacement_driver_causality() {
        let feed = ConversationFeed::new();
        let id = Ulid::new();
        feed.register_session(id, recording_poster(Arc::new(Mutex::new(Vec::new()))));
        let mut events = feed.subscribe();
        feed.observe(&message_envelope(id, "user:http", "original"))
            .await;
        feed.observe(&yield_envelope(id)).await;
        feed.observe(&turn_end_envelope(id)).await;
        assert!(matches!(
            events.try_recv().unwrap(),
            ConversationEvent::TurnEnded {
                caused_by_driver: true,
                ..
            }
        ));
        feed.observe(&message_envelope(id, "user:subagent", "not driver"))
            .await;
        feed.observe(&message_envelope(id, "user:cli", "replacement"))
            .await;
        let mut resolution = yield_envelope(id);
        resolution.kind = "command.result".into();
        resolution.payload =
            json!({"name":"watchdog","data":{"resolution":"superseded","turn_id":Ulid::new()}});
        feed.observe(&resolution).await;
        assert!(
            events.try_recv().is_err(),
            "resolution never ends the old turn again"
        );
        feed.observe(&turn_end_envelope(id)).await;
        assert!(matches!(
            events.try_recv().unwrap(),
            ConversationEvent::TurnEnded {
                caused_by_driver: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_wake_without_wait_or_session_posts_or_reports_none() {
        let feed = ConversationFeed::new();
        assert_eq!(
            feed.post_wake("anyone?").await,
            WakeOutcome::NoSession,
            "no conversation at all"
        );

        let id = Ulid::new();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted.clone()));
        feed.observe(&message_envelope(id, "user:http", "hello"))
            .await;
        assert_eq!(feed.post_wake("not waiting").await, WakeOutcome::Posted(id));
        assert_eq!(posted.lock().unwrap().len(), 1);

        feed.forget_session(id);
        assert_eq!(feed.active_session(), None);
        assert_eq!(feed.digest(), "");
        assert_eq!(feed.post_wake("gone").await, WakeOutcome::NoSession);
    }

    fn turn_end_envelope(session: Ulid) -> EventEnvelope {
        EventEnvelope::new(
            session,
            0,
            EventKind::TurnEnd,
            json!({ "turn_id": Ulid::new().to_string(), "actor_id": "agent:main", "outcome": "done", "duration_ms": 1 }),
        )
    }

    #[tokio::test]
    async fn turn_ended_tells_whether_the_driver_caused_it() {
        let feed = ConversationFeed::new();
        let id = Ulid::new();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted));
        let mut events = feed.subscribe();

        feed.observe(&message_envelope(id, "user:http", "add a command"))
            .await;
        feed.observe(&turn_end_envelope(id)).await;
        assert_eq!(
            events.try_recv().unwrap(),
            ConversationEvent::TurnEnded {
                session_id: id,
                caused_by_driver: true
            },
            "a driver message's turn"
        );

        feed.observe(&message_envelope(id, NAVIGATOR_ACTOR_ID, "wake: drift"))
            .await;
        feed.observe(&turn_end_envelope(id)).await;
        assert_eq!(
            events.try_recv().unwrap(),
            ConversationEvent::TurnEnded {
                session_id: id,
                caused_by_driver: false
            },
            "a navigator wake's turn"
        );
        assert!(events.try_recv().is_err(), "one event per turn end");
    }

    #[tokio::test]
    async fn replayed_turn_ends_and_unknown_sessions_announce_nothing() {
        let feed = ConversationFeed::new();
        let id = Ulid::new();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted));
        let mut events = feed.subscribe();

        feed.observe_replayed(&message_envelope(id, "user:http", "old chat"))
            .await;
        feed.observe_replayed(&turn_end_envelope(id)).await;
        feed.observe(&turn_end_envelope(Ulid::new())).await;
        assert!(events.try_recv().is_err());
        assert!(feed.digest().contains("driver: old chat"), "state rebuilt");
    }

    #[tokio::test]
    async fn unknown_sessions_and_empty_kinds_are_ignored() {
        let feed = ConversationFeed::new();
        // Events for a session that was never registered change nothing.
        feed.observe(&message_envelope(Ulid::new(), "user:http", "hello"))
            .await;
        feed.observe(&yield_envelope(Ulid::new())).await;
        assert_eq!(feed.active_session(), None);
        assert!(!feed.is_waiting_on_driver());
        assert_eq!(feed.digest(), "");
    }

    #[tokio::test]
    async fn child_events_do_not_replace_root_conversation_or_answer_its_ask() {
        let feed = ConversationFeed::new();
        let root = Ulid::new();
        let child = Ulid::new();
        feed.register_session(root, recording_poster(Arc::new(Mutex::new(Vec::new()))));
        feed.observe(&message_envelope(root, "user:http", "root request"))
            .await;
        feed.observe(&yield_envelope(root)).await;
        feed.observe(&message_envelope(child, "user:http", "child request"))
            .await;
        feed.observe(&response_envelope(child, "raw child transcript"))
            .await;
        feed.observe(&turn_end_envelope(child)).await;
        feed.observe(&message_envelope(root, "subagent", "bounded child result"))
            .await;
        assert_eq!(feed.active_session(), Some(root));
        assert!(feed.is_waiting_on_driver());
        assert_eq!(
            feed.digest(),
            "driver: root request\nsubagent (untrusted): bounded child result"
        );
    }
}
