//! The session `Service`: a registry + router over per-session sequencer
//! and runtime handles. It processes NO events: it mints sessions (sequencer
//! spawned FIRST, `session.start` published, THEN the runtime task), answers
//! create/get, and hands teardown to [`super::lifecycle`]. No HTTP type
//! appears here, so the CLI and the HTTP adapter drive the same service.
//!
//! Two notions of "gone", which are NOT the same thing:
//!
//! - Runtime stopped: the in-memory actor and sequencer no longer exist, so
//!   nothing can be posted to the session. A [`Tombstone`] may still answer
//!   `GET /v1/sessions/:id` ("stopped" at `final_seq`) and a late SSE
//!   reconnect (it replays the retained durable history through
//!   `session.end`, then ends). A tombstone is bounded and lives only in
//!   memory: after eviction or a process restart the id is unknown here.
//! - Conversation resumable: the SQLite store ([`super::store`]) holds
//!   the conversation. `POST /v1/sessions` with `resume` rebuilds a fresh
//!   actor and sequencer under the SAME id, continuing at `last_seq + 1`.
//!   This works from a tombstone or from disk alone.
//!
//! The registry conflates them in one place: `GET /v1/sessions/:id` and the
//! SSE route consult only the in-memory registry, so a conversation that is
//! resumable but has no tombstone (evicted, or from before a restart) answers
//! 404 even though `resume` would succeed. "Unknown here" does not mean
//! "not resumable".
//!
//! Lock discipline: the registry is a `std::sync::Mutex`
//! whose critical sections are short and purely synchronous. Every `.await`
//! happens AFTER the guard is dropped — clone the [`SessionHandle`] (its
//! `mpsc::Sender` and [`Sequencer`] are `Clone`), release the lock, THEN
//! await. clippy's `await_holding_lock` lint enforces this.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::actor::{
    build_actor, restore_transcript, spawn_feed_watch, start_candidate, wake_poster,
    NAVIGATOR_PROFILE,
};
use super::lifecycle::Tombstone;
use crate::agent::actor::AgentActor;
use crate::bus::command::Command;
use crate::bus::event::ROOT_ACTOR_ID;
use crate::bus::inbox::ActorInbox;
use crate::bus::runtime;
use crate::bus::sequencer::Sequencer;
use crate::bus::SessionBus;
use crate::error::ServerError;
use crate::provider::auth;
use crate::provider::config::{load_models, ResolvedModel};
use crate::provider::openai::ChatMessage;

/// A session id on the wire and in the registry.
pub type SessionId = Ulid;

/// Synchronously-readable lifecycle state of a LIVE registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Accepts commands and subscriptions.
    Active,
    /// Teardown started (stop/shutdown): new commands + subscriptions are
    /// rejected from here on.
    Stopping,
}

/// Everything needed to route to a live session. Cloned out of the registry
/// so no lock is ever held across an await.
#[derive(Clone)]
pub struct SessionHandle {
    /// Bounded per-actor command transport (message routes post here).
    pub inbox_tx: mpsc::Sender<Command>,
    /// The session's ONE authoritative event sequencer.
    pub sequencer: Sequencer,
    /// Fires on stop/shutdown; the runtime task tears the session down.
    pub session_cancel: CancellationToken,
    /// The runtime task's join handle, take-once: teardown awaits it so the
    /// interrupted turn's events are sequenced BEFORE the `session.end`
    /// fence. Shared so the registry's handle clones stay
    /// `Clone`; the fence races to take the same `Option`.
    pub runtime_task: Arc<Mutex<Option<tokio::task::JoinHandle<AgentActor>>>>,
    /// Always `agent:main` this slice (the single root actor).
    pub actor_id: String,
    pub lifecycle: Lifecycle,
    /// Last state the manager knows about ("idle" at creation).
    pub actor_state: String,
    pub workspace: PathBuf,
    /// The alias the session was created with.
    pub model: String,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("actor_id", &self.actor_id)
            .field("lifecycle", &self.lifecycle)
            .field("actor_state", &self.actor_state)
            .field("workspace", &self.workspace)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// One registry slot: a live, routable session OR a closed tombstone.
#[derive(Clone, Debug)]
pub enum SessionSlot {
    Live(SessionHandle),
    Tombstone(Tombstone),
}

/// What a GET summary needs, extracted under the lock — the sequencer head
/// query (a control-plane round-trip, not event reading) happens AFTER the
/// guard is released.
enum SummarySource {
    Live {
        sequencer: Sequencer,
        actor_state: String,
        workspace: String,
        model: String,
    },
    Tombstone {
        final_seq: u64,
        workspace: String,
        model: String,
    },
}

/// Frozen `GET /v1/sessions/:id` body.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub actor_state: String,
    pub workspace: String,
    pub model: String,
    pub current_seq: u64,
}

/// Immutable server configuration, validated at bind time.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Loopback-only by default; a non-loopback bind is refused unless
    /// `allow_non_loopback` is set (and even then: NO auth/TLS this slice).
    pub bind: SocketAddr,
    /// Explicit models.json path; `None` resolves per-workspace then the
    /// config dir (same order as the CLI `run` path).
    pub models_path: Option<PathBuf>,
    /// Auth store directory for API-key resolution.
    pub config_dir: PathBuf,
    pub allow_non_loopback: bool,
    /// When set, every route except health requires
    /// `Authorization: Bearer <token>` (the navigator's in-process server).
    pub bearer_token: Option<String>,
    /// Conversation-history database (`sessions.db`); `None` keeps today's
    /// behaviour with no persistence (tests, CLI `run`, `serve`).
    pub history_db: Option<PathBuf>,
}

impl ServerConfig {
    /// Bind-time validation: refuse a non-loopback address unless the
    /// explicit escape hatch was passed.
    pub fn validate(&self) -> Result<(), ServerError> {
        if !self.bind.ip().is_loopback() && !self.allow_non_loopback {
            return Err(ServerError::NonLoopbackBind(self.bind));
        }
        Ok(())
    }
}

/// The session service: the registry plus the immutable config. Cheap to
/// clone (all fields are `Arc`).
#[derive(Clone, Debug)]
pub struct Service {
    pub sessions: Arc<Mutex<HashMap<SessionId, SessionSlot>>>,
    /// Insertion order of tombstone ids — the FIFO eviction queue backing
    /// [`MAX_TOMBSTONES`].
    tombstone_order: Arc<Mutex<VecDeque<SessionId>>>,
    pub config: Arc<ServerConfig>,
    /// The conversation-history store; `None` when `history_db` is unset.
    history: Option<Arc<super::store::HistoryStore>>,
    /// The navigator's conversation hub, when this server shares its
    /// process with the language server. Set once by the host binary
    /// before any session exists; the feed is optional so plain HTTP
    /// servers and tests keep working without one.
    conversation: Option<crate::navigator::ConversationFeed>,
}

impl Service {
    pub fn new(config: ServerConfig) -> Self {
        Self::with_conversation(config, None)
    }

    /// [`Service::new`] sharing the navigator's conversation hub: a
    /// navigator-profile session registers itself there and a subscriber
    /// task feeds its events, so the language server can read the digest
    /// and wake the cognition.
    pub fn with_conversation(
        config: ServerConfig,
        conversation: Option<crate::navigator::ConversationFeed>,
    ) -> Self {
        let history = config.history_db.as_deref().and_then(|path| {
            match super::store::HistoryStore::open(path) {
                Ok(store) => Some(Arc::new(store)),
                Err(err) => {
                    // Persistence is an enhancement, never a dependency: a
                    // broken database degrades to no history.
                    tracing::error!("history store disabled: {err}");
                    None
                }
            }
        });
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            tombstone_order: Arc::new(Mutex::new(VecDeque::new())),
            config: Arc::new(config),
            history,
            conversation,
        }
    }

    /// The conversation hub, when one was attached.
    pub fn conversation(&self) -> Option<&crate::navigator::ConversationFeed> {
        self.conversation.as_ref()
    }

    /// The history store, when persistence is enabled.
    pub fn history(&self) -> Option<&super::store::HistoryStore> {
        self.history.as_deref()
    }

    /// Live sessions + retained tombstones (the whole registry). Client
    /// connects/disconnects never touch the registry: only `create_session`
    /// inserts and each session occupies at most one slot, so a
    /// connect-then-drop loop cannot grow it and the tombstone pool is
    /// capped at [`MAX_TOMBSTONES`].
    pub fn session_count(&self) -> usize {
        self.lock_sessions().len()
    }

    /// Retained tombstones (the bounded pool).
    pub fn tombstone_count(&self) -> usize {
        self.lock_tombstones().len()
    }

    /// A clone of the LIVE handle, for routing without holding the lock.
    /// `None` for tombstones (not routable) and unknown ids.
    pub fn handle(&self, id: SessionId) -> Option<SessionHandle> {
        match self.lock_sessions().get(&id) {
            Some(SessionSlot::Live(handle)) => Some(handle.clone()),
            Some(SessionSlot::Tombstone(_)) | None => None,
        }
    }

    /// A cloned slot (live handle vs tombstone) for the SSE endpoint, so it
    /// can distinguish a stopped session from an unknown id; `None` unknown.
    pub fn slot(&self, id: SessionId) -> Option<SessionSlot> {
        self.lock_sessions().get(&id).cloned()
    }

    /// A clone of the LIVE handle, gated on the lifecycle: `Err(404)` for
    /// unknown ids and tombstones, `Err(409 SessionStopping)` for a
    /// Stopping session. No lock is
    /// held past the return — the caller awaits only on the clone.
    pub fn routable_handle(&self, id: SessionId) -> Result<SessionHandle, ServerError> {
        match self.lock_sessions().get(&id) {
            Some(SessionSlot::Live(handle)) if handle.lifecycle == Lifecycle::Active => {
                Ok(handle.clone())
            }
            Some(SessionSlot::Live(_)) => Err(ServerError::SessionStopping),
            Some(SessionSlot::Tombstone(_)) | None => {
                Err(ServerError::SessionNotFound(id.to_string()))
            }
        }
    }

    /// Creates a session: resolves models.json + the key (NO live provider
    /// call — the actor is built but not run until a message arrives),
    /// mints tokens, spawns the sequencer FIRST, publishes `session.start`
    /// (seq 0), THEN spawns the runtime task, and registers the handle.
    pub async fn create_session(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
    ) -> Result<SessionId, ServerError> {
        self.create_or_resume_session(workspace, model, profile, None)
            .await
            .map(|(id, _)| id)
    }

    /// [`create_session`] with an optional `resume` id (the POST
    /// `/v1/sessions` `{"resume": "<id>"}` path):
    /// - live + active → the SAME id, no rebuild;
    /// - tombstone, or disk-only → rebuilt from the store under the SAME id
    ///   with its stored model and profile: the sequencer continues at
    ///   `last_seq + 1`, the bus is pre-filled with the stored envelopes (a
    ///   fresh subscriber replays the past conversation), NO new
    ///   `session.start`, the actor's transcript restored from the snapshot;
    /// - unknown id → [`ServerError::SessionNotFound`].
    ///
    /// Returns the id and whether the session was resumed.
    pub async fn create_or_resume_session(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        resume: Option<SessionId>,
    ) -> Result<(SessionId, bool), ServerError> {
        if let Some(id) = resume {
            // Live + active: the conversation never left.
            if let Some(SessionSlot::Live(handle)) = self.lock_sessions().get(&id) {
                if handle.lifecycle == Lifecycle::Active {
                    return Ok((id, true));
                }
            }
            // Tombstone in memory, or only on disk: rebuild under the same
            // id. Both need the stored row.
            let Some(store) = self.history.as_ref() else {
                return Err(ServerError::SessionNotFound(id.to_string()));
            };
            // The closing session's last events (`session.end` included) may
            // still be queued: the load must see them, or the resumed
            // sequence could restart below a seq a client already saw.
            store.flush().await.map_err(ServerError::History)?;
            let Some(loaded) = store.load(id).await.map_err(ServerError::History)? else {
                return Err(ServerError::SessionNotFound(id.to_string()));
            };
            self.spawn_session(workspace, None, None, Some(loaded), workspace)
                .await?;
            return Ok((id, true));
        }
        let id = self
            .spawn_session(workspace, model, profile, None, workspace)
            .await?;
        Ok((id, false))
    }

    /// The frozen `GET /v1/sessions/:id` summary. Unknown id is a structured
    /// 404; a tombstone answers "stopped" at its recorded `final_seq`.
    /// `current_seq` = last EMITTED seq: for a live
    /// session it is the sequencer head minus one; for a tombstone it is the
    /// retained `final_seq`.
    pub async fn session_summary(&self, id: SessionId) -> Result<SessionSummary, ServerError> {
        let source = {
            let guard = self.lock_sessions();
            match guard.get(&id) {
                Some(SessionSlot::Live(handle)) => SummarySource::Live {
                    sequencer: handle.sequencer.clone(),
                    actor_state: handle.actor_state.clone(),
                    workspace: handle.workspace.display().to_string(),
                    model: handle.model.clone(),
                },
                Some(SessionSlot::Tombstone(tombstone)) => SummarySource::Tombstone {
                    final_seq: tombstone.final_seq,
                    workspace: tombstone.workspace.display().to_string(),
                    model: tombstone.model.clone(),
                },
                None => return Err(ServerError::SessionNotFound(id.to_string())),
            }
        };
        // Guard released above; the head query is a plain sequencer
        // round-trip (the manager reads no events).
        let (current_seq, actor_state, workspace, model) = match source {
            SummarySource::Live {
                sequencer,
                actor_state,
                workspace,
                model,
            } => {
                let current_seq = sequencer
                    .current_seq()
                    .await
                    .map(|next| next.saturating_sub(1))
                    .unwrap_or(0);
                (current_seq, actor_state, workspace, model)
            }
            SummarySource::Tombstone {
                final_seq,
                workspace,
                model,
            } => (final_seq, "stopped".to_string(), workspace, model),
        };
        Ok(SessionSummary {
            session_id: id.to_string(),
            actor_state,
            workspace,
            model,
            current_seq,
        })
    }

    pub(super) fn lock_sessions(&self) -> MutexGuard<'_, HashMap<SessionId, SessionSlot>> {
        // Poison recovery via into_inner: a panicking writer must not wedge
        // the registry (same policy as the runtime observers).
        match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(super) fn lock_tombstones(&self) -> MutexGuard<'_, VecDeque<SessionId>> {
        match self.tombstone_order.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn resolve_model(&self, workspace: &Path, alias: &str) -> Result<ResolvedModel, ServerError> {
        let (config, _) = load_models(
            self.config.models_path.as_deref(),
            workspace,
            &self.config.config_dir,
        )?;
        Ok(config.resolve(alias)?)
    }

    /// The shared mint: resolves the model, builds the actor, spawns the
    /// sequencer (pre-filled + continuing at `last_seq + 1` when resuming),
    /// publishes `session.start` (fresh only), spawns the runtime with the
    /// history hooks, registers the handle. `resume_workspace` is the
    /// workspace a resume request named; the FRESH path passes its own.
    async fn spawn_session(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        resumed: Option<super::store::LoadedSession>,
        resume_workspace: &Path,
    ) -> Result<SessionId, ServerError> {
        // Resume keeps the stored model/profile/workspace (the stored row is
        // the authority); a fresh session honors the request's.
        // Resume keeps the stored model/profile/workspace (the stored row is
        // the authority); a fresh session honors the request's.
        let session_id = match &resumed {
            Some(loaded) => loaded.meta.id,
            // NB: NOT Ulid::default() — that is the nil ULID; every fresh
            // session must mint its own id.
            None => Ulid::new(),
        };
        let (workspace, alias, profile) = match &resumed {
            Some(loaded) => (
                PathBuf::from(&loaded.meta.workspace),
                loaded.meta.model.clone(),
                loaded.meta.profile.as_deref(),
            ),
            None => {
                let navigator = profile == Some(NAVIGATOR_PROFILE);
                let alias = model.unwrap_or(if navigator { "cognition" } else { "default" });
                (workspace.to_path_buf(), alias.to_string(), profile)
            }
        };
        let navigator = profile == Some(NAVIGATOR_PROFILE);
        let resolved = self.resolve_model(resume_workspace, &alias)?;
        let api_key = auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )?;
        let session_cancel = CancellationToken::new();
        let mut actor = build_actor(
            &workspace,
            &self.config.config_dir,
            resolved,
            api_key,
            session_cancel.child_token(),
            navigator,
        );
        if let Some(loaded) = &resumed {
            actor.transcript = restore_transcript(&loaded.transcript, navigator);
            actor.restore_pending_ask();
        }

        let next_seq = resumed
            .as_ref()
            .map(|loaded| loaded.meta.last_seq + 1)
            .unwrap_or(0);
        // The bus is pre-filled with the stored conversation, minus every
        // earlier `session.end`: the resumed stream must not look closed.
        // (`next_seq` above still counts those ends, so seqs never repeat.)
        let bus = SessionBus::new(
            resumed
                .as_ref()
                .map(|loaded| {
                    loaded
                        .envelopes
                        .iter()
                        .filter(|envelope| envelope.kind != "session.end")
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
        );
        let is_resume = resumed.is_some();

        // History hooks: a persisting emitter + a turn-end transcript
        // snapshot, only when the store is enabled.
        let history = self.history.clone();
        let sequencer = match &history {
            Some(store) => {
                // Acknowledged: a session whose row cannot be written would
                // silently lose every later event and snapshot.
                store
                    .record_session(
                        session_id,
                        &workspace.display().to_string(),
                        &alias,
                        profile,
                    )
                    .await
                    .map_err(ServerError::History)?;
                Sequencer::spawn(
                    session_id,
                    super::store::PersistingEmitter::new(session_id, bus, (**store).clone()),
                    next_seq,
                )
            }
            None => Sequencer::spawn(session_id, bus, next_seq),
        };

        if !is_resume {
            // session.start is seq 0, sequenced before the runtime exists.
            let _ = sequencer.publish(start_candidate(&workspace));
        }
        // THEN the runtime task; the handle is retained so /stop can await
        // the interrupted turn's events before the session.end fence.
        let (inbox_tx, inbox) = ActorInbox::channel();
        let on_turn_end: Option<crate::bus::runtime::TurnEndHook> = history.as_ref().map(|store| {
            let store = store.clone();
            Box::new(move |transcript: &[ChatMessage]| {
                store.save_transcript(session_id, transcript.to_vec());
            }) as crate::bus::runtime::TurnEndHook
        });
        let runtime_task = Arc::new(Mutex::new(Some(runtime::spawn(
            session_id,
            actor,
            inbox,
            sequencer.clone(),
            session_cancel.clone(),
            on_turn_end,
        ))));

        // A resumed id replaces its tombstone slot: also drop it from the
        // eviction queue so a later close cannot evict the now-live id.
        self.lock_tombstones().retain(|other| *other != session_id);
        // The conversation hub knows this session (navigator profiles
        // only): a poster that reuses the POST-message core in-process —
        // same validation, same event-before-action sequencing, only the
        // sender is the navigator itself — and a subscriber task that
        // feeds the session's events into the digest. Failed posts
        // (stopping, full inbox) surface as the error string the feed
        // logs; the poster holds its own handle clone so no registry lock
        // is ever held across an await.
        let feed_watch = navigator
            .then_some(self.conversation.as_ref())
            .flatten()
            .map(|feed| {
                feed.register_session(
                    session_id,
                    wake_poster(
                        SessionHandle {
                            inbox_tx: inbox_tx.clone(),
                            sequencer: sequencer.clone(),
                            session_cancel: session_cancel.clone(),
                            runtime_task: runtime_task.clone(),
                            actor_id: ROOT_ACTOR_ID.to_string(),
                            lifecycle: Lifecycle::Active,
                            actor_state: "idle".to_string(),
                            workspace: workspace.clone(),
                            model: alias.clone(),
                        },
                        session_id,
                    ),
                );
                (feed.clone(), sequencer.clone(), session_cancel.clone())
            });
        self.lock_sessions().insert(
            session_id,
            SessionSlot::Live(SessionHandle {
                inbox_tx,
                sequencer,
                session_cancel,
                runtime_task,
                actor_id: ROOT_ACTOR_ID.to_string(),
                lifecycle: Lifecycle::Active,
                actor_state: "idle".to_string(),
                workspace: workspace.clone(),
                model: alias.clone(),
            }),
        );
        if let Some((feed, sequencer, session_cancel)) = feed_watch {
            spawn_feed_watch(feed, sequencer, session_cancel, session_id);
        }
        Ok(session_id)
    }
}

#[cfg(test)]
pub(super) fn test_config(bind: &str, allow_non_loopback: bool) -> ServerConfig {
    ServerConfig {
        bind: bind.parse().expect("test bind addr"),
        models_path: None,
        config_dir: PathBuf::from("/tmp/rust-agent-test"),
        allow_non_loopback,
        bearer_token: None,
        history_db: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given the default loopback bind and a 0.0.0.0 bind without the flag
    // When ServerConfig::validate runs
    // Then loopback is accepted and 0.0.0.0 is refused with NonLoopbackBind
    #[test]
    fn loopback_bind_is_accepted_and_wildcard_refused_without_flag() {
        assert!(test_config("127.0.0.1:7310", false).validate().is_ok());
        assert!(matches!(
            test_config("0.0.0.0:7310", false).validate(),
            Err(ServerError::NonLoopbackBind(_))
        ));
    }

    // Given a non-loopback bind WITH --allow-non-loopback
    // When ServerConfig::validate runs
    // Then it is accepted (documented at-your-own-risk; still no auth/TLS)
    #[test]
    fn non_loopback_bind_is_accepted_with_the_explicit_flag() {
        assert!(test_config("0.0.0.0:7310", true).validate().is_ok());
        assert!(test_config("192.168.1.10:7310", true).validate().is_ok());
    }
}
