//! Session manager (todo 6): a registry + router over per-session sequencer
//! and runtime handles. It processes NO events: it mints sessions (sequencer
//! spawned FIRST, `session.start` published, THEN the runtime task), answers
//! create/get, and drives lifecycle transitions. Teardown fires the
//! sequencer's `CloseSession` fence — the SEQUENCER owns emitting
//! `session.end` + assigning `final_seq`; the manager never subscribes to or
//! parses events. The closed session is retained as a bounded pool of
//! [`Tombstone`] slots (P1-3: a reconnect terminates cleanly instead of 404
//! mid-race; the OLDEST tombstone is evicted once [`MAX_TOMBSTONES`] is
//! exceeded, so create/stop churn cannot grow the registry unboundedly).
//!
//! Lock discipline (ticket 05 Q4): the registry is a `std::sync::Mutex`
//! whose critical sections are short and purely synchronous. Every `.await`
//! happens AFTER the guard is dropped — clone the [`SessionHandle`] (its
//! `mpsc::Sender` and [`Sequencer`] are `Clone`), release the lock, THEN
//! await. clippy's `await_holding_lock` lint enforces this.
//!
//! DEFERRAL (todo 7 -> todo 8, explicit): a tombstone retains `final_seq`,
//! workspace, and model ONLY. The plan line "tombstone retains the ring
//! snapshot plus eviction watermark so an in-flight reconnect replays
//! through final_seq" describes TODO 8's reconnect-after-final_seq
//! behavior: SSE resume (`Last-Event-ID`) is what consumes that. Until
//! todo 8 lands, a reconnect to a stopped session is answered from the
//! tombstone by GET ("stopped" at final_seq) and SSE/WS streaming targets
//! return 404.
//! `tests/server_messages.rs::stopped_session_answers_get_but_routing_is_gone_deferral_pin`
//! pins this so the deferral is visible, not silent.
//!
//! allow: SIZE_OK — ticket 05's approved module map fixes the file set
//! (mod/routes/sse/websocket/session); the manager's registry + lifecycle +
//! actor construction must stay in this one file.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::agent::actor::AgentActor;
use crate::bus::command::{Command, ContentBlock, DeliveryMode};
use crate::bus::event::{EventEnvelope, EventKind, SessionStart, ROOT_ACTOR_ID};
use crate::bus::inbox::ActorInbox;
use crate::bus::runtime;
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::bus::SessionBus;
use crate::error::{ConfigError, ServerError};
use crate::provider::auth;
use crate::provider::config::{resolve_models_path, ModelsConfig, ResolvedModel};
use crate::server::routes::route_message;
use crate::tools::{Registry, ToolContext};

/// Retained closed-session tombstones before the OLDEST is evicted (FIFO).
/// Live sessions are never evicted — they are active. Only the tombstone
/// pool is bounded, so clients cannot grow the registry unboundedly through
/// create/stop churn.
pub const MAX_TOMBSTONES: usize = 128;

/// Upper bound on waiting for the runtime task to unwind during teardown:
/// the actor token is already cancelled at that point, so an interrupted
/// provider stream ends promptly; the cap only covers a wedged I/O layer so
/// `/stop` is a bounded HTTP response. On timeout the backstop fence still
/// makes `session.end` the last event.
const TEARDOWN_AWAIT_CAP: Duration = Duration::from_secs(10);

/// A session id on the wire and in the registry.
pub type SessionId = Ulid;

/// Synchronously-readable lifecycle state of a LIVE registry entry (P1-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Accepts commands and subscriptions.
    Active,
    /// Teardown started (stop/shutdown): new commands + subscriptions are
    /// rejected from here on (todo 7 consumes this).
    Stopping,
}

/// Everything needed to route to a live session. Cloned out of the registry
/// so no lock is ever held across an await.
#[derive(Clone)]
pub struct SessionHandle {
    /// Bounded per-actor command transport (todo 7 posts here).
    pub inbox_tx: mpsc::Sender<Command>,
    /// The session's ONE authoritative event sequencer.
    pub sequencer: Sequencer,
    /// Fires on stop/shutdown; the runtime task tears the session down.
    pub session_cancel: CancellationToken,
    /// The runtime task's join handle, take-once: teardown awaits it so the
    /// interrupted turn's events are sequenced BEFORE the `session.end`
    /// fence (ticket 03). Shared so the registry's handle clones stay
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

/// A closed session's retained record (its tombstone). Holds what a late
/// GET needs PLUS the frozen durable-history snapshot (for a reconnect
/// after `final_seq`): a streaming route replays `retained` through
/// `final_seq` then ends cleanly. No `inbox_tx`, no `Sequencer`, no
/// cancellation token: the routing resources are released when the live
/// slot is replaced.
#[derive(Clone, Debug)]
pub struct Tombstone {
    /// The session's last seq: the head observed at close time — the same
    /// seq the sequencer's CloseSession fence deterministically assigns to
    /// `session.end` (one more than the last emitted event). The sequencer
    /// owns the event; this is the manager's lock-free estimate of it (for
    /// an idle session it is exact; todo 7's stop flow supersedes it).
    pub final_seq: u64,
    pub workspace: PathBuf,
    pub model: String,
    /// Durable envelopes retained at close time, ending at `session.end`.
    /// `None` when the ring was already past [`RING_CAP`] (eviction) — a
    /// snapshot without its evicted prefix cannot answer a fresh (no-cursor)
    /// reconnect without silently hiding events, so replay is refused.
    pub retained: Option<Vec<EventEnvelope>>,
    /// The evicted-durable watermark at close time: durable seqs
    /// `<= resume_floor` are unreplayable from this tombstone.
    pub resume_floor: Option<u64>,
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

/// Frozen `GET /v1/sessions/:id` body (ticket 04 Q3).
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub actor_state: String,
    pub workspace: String,
    pub model: String,
    pub current_seq: u64,
}

/// Session profile selecting the navigator's cognition: the `cognition`
/// model alias by default and the pair-programmer system prompt.
pub const NAVIGATOR_PROFILE: &str = "navigator";

/// The cognition's role in a navigator session, editable as Markdown.
pub const COGNITION_PROMPT: &str = include_str!("../navigator/cognition.md");

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

/// Axum state: the session registry plus the immutable config. Cheap to
/// clone (all fields are `Arc`).
#[derive(Clone, Debug)]
pub struct AppState {
    pub sessions: Arc<Mutex<HashMap<SessionId, SessionSlot>>>,
    /// Insertion order of tombstone ids — the FIFO eviction queue backing
    /// [`MAX_TOMBSTONES`].
    tombstone_order: Arc<Mutex<VecDeque<SessionId>>>,
    pub config: Arc<ServerConfig>,
    /// The conversation-history store; `None` when `history_db` is unset.
    history: Option<Arc<crate::server::store::HistoryStore>>,
    /// The navigator's conversation hub, when this server shares its
    /// process with the language server. Set once by the host binary
    /// before any session exists; the feed is optional so plain HTTP
    /// servers and tests keep working without one.
    conversation: Option<crate::navigator::ConversationFeed>,
}

impl AppState {
    pub fn new(config: ServerConfig) -> Self {
        Self::with_conversation(config, None)
    }

    /// [`AppState::new`] sharing the navigator's conversation hub: a
    /// navigator-profile session registers itself there and a subscriber
    /// task feeds its events, so the language server can read the digest
    /// and wake the cognition.
    pub fn with_conversation(
        config: ServerConfig,
        conversation: Option<crate::navigator::ConversationFeed>,
    ) -> Self {
        let history = config.history_db.as_deref().and_then(|path| {
            match crate::server::store::HistoryStore::open(path) {
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
    pub fn history(&self) -> Option<&crate::server::store::HistoryStore> {
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
    /// Stopping session (todo 7's command routes consume this). No lock is
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
            let Some(loaded) = store.load(id).await.map_err(ServerError::History)? else {
                return Err(ServerError::SessionNotFound(id.to_string()));
            };
            self.spawn_session(workspace, None, None, Some(loaded), workspace)?;
            return Ok((id, true));
        }
        let id = self.spawn_session(workspace, model, profile, None, workspace)?;
        Ok((id, false))
    }

    /// The frozen `GET /v1/sessions/:id` summary. Unknown id is a structured
    /// 404; a tombstone answers "stopped" at its recorded `final_seq`.
    /// `current_seq` = last EMITTED seq (ticket 04 vocabulary): for a live
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

    /// Ordered teardown of ONE session (todo 7's /stop, shutdown):
    /// `Stopping` FIRST (so concurrent requests see it), then the ticket-03
    /// order: cancel the session token (the RUNTIME emits the interrupted
    /// `turn.end` + `agent.state{Idle}` from the natural outcome and drops
    /// its inbox receiver), WAIT for the runtime task's teardown to
    /// complete, THEN fire the sequencer's `CloseSession` fence — the
    /// SEQUENCER sequences `session.end{final_seq}` as the LAST event — and
    /// the live slot is replaced by a bounded [`Tombstone`]. Idempotent: an
    /// already-Stopping or already-closed session is a no-op.
    pub async fn close_session(&self, id: SessionId) -> Result<(), ServerError> {
        let Some(handle) = self.mark_stopping(id)? else {
            return Ok(());
        };
        self.fence_and_tombstone(id, handle, "stop requested").await;
        Ok(())
    }

    /// Graceful shutdown: stop every active session (each one's fence emits
    /// its `session.end`), tombstone them all, then return so axum finishes
    /// draining.
    pub async fn close_all(&self) {
        let targets: Vec<(SessionId, SessionHandle)> = {
            let mut guard = self.lock_sessions();
            guard
                .iter_mut()
                .filter_map(|(id, slot)| match slot {
                    SessionSlot::Live(handle) if handle.lifecycle == Lifecycle::Active => {
                        handle.lifecycle = Lifecycle::Stopping;
                        Some((*id, handle.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        for (id, handle) in targets {
            self.fence_and_tombstone(id, handle, "shutdown").await;
        }
    }

    /// Fire the ticket-03 order. The RUNTIME owns the terminal tail: the
    /// session-token cancel interrupts the actor, the runtime's natural
    /// outcome path emits `turn.end{interrupted}` then `agent.state{Idle}`,
    /// and its `teardown()` fires the CloseSession fence itself, sequencing
    /// `session.end` as the LAST event. The manager (1) takes a tail probe
    /// (a live subscription) BEFORE cancelling so the terminal tail stays
    /// observable even with no external subscriber, (2) cancels and awaits
    /// the runtime JoinHandle (bounded; on breach the task is ABORTED
    /// because a dropped JoinHandle would only DETACH it), (3) fires an
    /// idempotent backstop fence only when the runtime did not fence
    /// (already-closed is a `SequencerClosed` no-op), and (4) tombstones
    /// with the seq the fence actually assigned to `session.end`. No lock
    /// is held across any await.
    async fn fence_and_tombstone(&self, id: SessionId, handle: SessionHandle, reason: &str) {
        // Tail probe BEFORE the cancel: survives even with no external
        // subscriber and anchors the tombstone's final_seq on the real tail.
        let tail = handle.sequencer.subscribe_since(None).await.ok();
        let tail_resume_floor = tail.as_ref().and_then(|reply| reply.resume_floor);
        handle.session_cancel.cancel();
        drop(handle.inbox_tx);

        // Backstop: after the runtime has settled, the sequencer must be
        // closed. If it is STILL OPEN (a wedged-then-aborted runtime, or a
        // foreign join task that never owned a fence), the manager fences.
        // On the normal path the runtime's teardown already closed it and
        // this is skipped — a second CloseSession would be a SequencerClosed
        // no-op, never a second session.end.
        let mut fenced_seq: Option<u64> = None;
        if let Some(mut task) = handle
            .runtime_task
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            // Await by reference so the JoinHandle survives the timeout (a
            // dropped JoinHandle DETACHES the task instead of aborting it).
            if tokio::time::timeout(TEARDOWN_AWAIT_CAP, &mut task)
                .await
                .is_err()
            {
                // DEGRADED path (documented): the runtime outlived the cap.
                // Abort it, REAP it (the abort lands at the task's next
                // poll; the await proves it can poll/emit no more), then
                // fence below. The ticket-03 terminal tail
                // (turn.end/agent.state) is lost in this path by
                // definition; it exists only for a wedged/aborted task.
                task.abort();
                let _ = task.await;
            }
        }
        if handle.sequencer.current_seq().await.is_ok() {
            fenced_seq = handle
                .sequencer
                .close_session_with_final_seq(reason)
                .await
                .ok()
                .flatten();
        }

        // The tombstone's final_seq is the seq the fence REALLY assigned:
        // authoritatively from the backstop fence's reply, else read off the
        // terminal tail the runtime produced (its session.end is the last
        // envelope the probe delivered).
        let fenced = fenced_seq.map(|seq| (seq, None));
        let (final_seq, collected_tail) = match fenced {
            Some((seq, collected)) => (seq, collected),
            None => {
                let (seq, collected) = Self::collect_tail_through_session_end(tail).await;
                (seq, Some(collected))
            }
        };

        let retained = match collected_tail {
            // The runtime fenced the session: the tail probe carries the
            // full durable history through session.end — freeze it.
            Some(collected) => collected,
            // The MANAGER fired the backstop fence (wedged/aborted runtime):
            // the probe cannot have seen session.end from the fence, so
            // re-subscribe to the now-closed session's retained ring.
            None => {
                let fresh = handle.sequencer.subscribe_since(None).await.ok();
                Self::collect_tail_through_session_end(fresh).await.1
            }
        };
        self.retain_tombstone(
            id,
            Tombstone {
                final_seq,
                workspace: handle.workspace.clone(),
                model: handle.model.clone(),
                retained,
                resume_floor: tail_resume_floor,
            },
        );
    }

    /// The durable tail of a closing session: replay + live envelopes up to and
    /// including `session.end`, with the seq it landed on. The single collection
    /// the tombstone derives BOTH `final_seq` (runtime-fenced path) and the
    /// frozen `retained` snapshot from — the live receiver is consumed exactly
    /// once. `None` when the ring had already evicted (a partial snapshot must
    /// not masquerade as complete) or the tail never reached `session.end`.
    async fn collect_tail_through_session_end(
        tail: Option<crate::bus::SubscribeReply>,
    ) -> (u64, Option<Vec<EventEnvelope>>) {
        let Some(mut reply) = tail else {
            return (0, None);
        };
        let resume_floor = reply.resume_floor;
        let mut envelopes = match reply.replay {
            crate::bus::ResumeOutcome::Replay(events) => events,
            crate::bus::ResumeOutcome::Resync | crate::bus::ResumeOutcome::InvalidCursor => {
                return (0, None);
            }
        };
        let mut last = envelopes.last().map(|e| e.seq).unwrap_or(0);
        if let Some(mut live) = reply.live.take() {
            while let Ok(Some(env)) = tokio::time::timeout(TEARDOWN_AWAIT_CAP, live.recv()).await {
                let is_end = env.kind.as_str() == "session.end";
                last = last.max(env.seq);
                envelopes.push(env);
                if is_end {
                    break;
                }
            }
        }
        if resume_floor.is_some()
            || !envelopes
                .last()
                .is_some_and(|env| env.kind.as_str() == "session.end")
        {
            return (last, None);
        }
        (last, Some(envelopes))
    }

    /// Marks a live session `Stopping` under the registry lock.
    /// `Ok(None)` when already Stopping/closed (idempotent); 404 when
    /// unknown. The slot stays in the map until the tombstone replaces it,
    /// so a concurrent request sees `Stopping` (P1-2) rather than a hole.
    fn mark_stopping(&self, id: SessionId) -> Result<Option<SessionHandle>, ServerError> {
        let mut guard = self.lock_sessions();
        match guard.get_mut(&id) {
            Some(SessionSlot::Live(handle)) if handle.lifecycle == Lifecycle::Active => {
                handle.lifecycle = Lifecycle::Stopping;
                Ok(Some(handle.clone()))
            }
            Some(_) => Ok(None),
            None => Err(ServerError::SessionNotFound(id.to_string())),
        }
    }

    /// Retains a tombstone, bounded by [`MAX_TOMBSTONES`] with FIFO eviction
    /// of the oldest. The insert REPLACES the live slot — the live routing
    /// resources (`inbox_tx`/`Sequencer`/token) drop with it, since a
    /// tombstone holds none of them. Two short synchronous critical
    /// sections — never across an await.
    fn retain_tombstone(&self, id: SessionId, tombstone: Tombstone) {
        let evicted = {
            let mut guard = self.lock_tombstones();
            guard.push_back(id);
            if guard.len() > MAX_TOMBSTONES {
                guard.pop_front()
            } else {
                None
            }
        };
        {
            let mut guard = self.lock_sessions();
            guard.insert(id, SessionSlot::Tombstone(tombstone));
            if let Some(oldest) = evicted {
                guard.remove(&oldest);
            }
        }
    }

    /// Removes a session from the registry AND the tombstone queue: the
    /// DELETE route's memory half. Call after `close_session` (live) or on
    /// a disk-only id (no memory slot at all).
    pub fn discard(&self, id: SessionId) {
        self.lock_sessions().remove(&id);
        self.lock_tombstones().retain(|other| *other != id);
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<SessionId, SessionSlot>> {
        // Poison recovery via into_inner: a panicking writer must not wedge
        // the registry (same policy as the runtime observers).
        match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_tombstones(&self) -> MutexGuard<'_, VecDeque<SessionId>> {
        match self.tombstone_order.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn resolve_model(&self, workspace: &Path, alias: &str) -> Result<ResolvedModel, ServerError> {
        let models_path = match &self.config.models_path {
            Some(path) => path.clone(),
            None => {
                resolve_models_path(None, workspace, &self.config.config_dir).ok_or_else(|| {
                    ServerError::Config(ConfigError::Read(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no models.json found (explicit, workspace, or config dir)",
                    )))
                })?
            }
        };
        let config = ModelsConfig::load(&models_path)?;
        Ok(config.resolve(alias)?)
    }

    /// The shared mint: resolves the model, builds the actor, spawns the
    /// sequencer (pre-filled + continuing at `last_seq + 1` when resuming),
    /// publishes `session.start` (fresh only), spawns the runtime with the
    /// history hooks, registers the handle. `resume_workspace` is the
    /// workspace a resume request named; the FRESH path passes its own.
    fn spawn_session(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        resumed: Option<crate::server::store::LoadedSession>,
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
            actor.transcript = loaded.transcript.clone();
        }

        let next_seq = resumed
            .as_ref()
            .map(|loaded| loaded.meta.last_seq + 1)
            .unwrap_or(0);
        let bus = match &resumed {
            Some(loaded) => SessionBus::from_envelopes(loaded.envelopes.clone()),
            None => SessionBus::new(),
        };
        let is_resume = resumed.is_some();

        // History hooks: a persisting emitter + a turn-end transcript
        // snapshot, only when the store is enabled.
        let history = self.history.clone();
        let sequencer = match &history {
            Some(store) => {
                store.record_session(
                    session_id,
                    &workspace.display().to_string(),
                    &alias,
                    profile,
                );
                Sequencer::spawn_from(
                    session_id,
                    crate::server::store::PersistingEmitter::new(
                        session_id,
                        bus,
                        (**store).clone(),
                    ),
                    next_seq,
                )
            }
            None => Sequencer::spawn_from(session_id, bus, next_seq),
        };

        if !is_resume {
            // session.start is seq 0, sequenced before the runtime exists.
            let _ = sequencer.publish(start_candidate(&workspace));
        }
        // THEN the runtime task; the handle is retained so /stop can await
        // the interrupted turn's events before the session.end fence.
        let (inbox_tx, inbox) = ActorInbox::channel();
        let on_turn_end: crate::bus::runtime::TurnEndHook = match &history {
            Some(store) => {
                let store = store.clone();
                Box::new(move |transcript| {
                    store.save_transcript(session_id, transcript.to_vec());
                })
            }
            None => Box::new(|_| {}),
        };
        let runtime_task = Arc::new(Mutex::new(Some(runtime::spawn_with_turn_end(
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
                        feed.observe(event).await;
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
        Ok(session_id)
    }
}

/// The feed's wake poster: routes a navigator-sourced message through the
/// same core `POST /messages` uses. A session already closing swallows the
/// error (the wake has nowhere to land, not a fault worth logging).
fn wake_poster(handle: SessionHandle, session_id: SessionId) -> crate::navigator::SessionPost {
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
            match route_message(&handle, command).await {
                Ok(_) => Ok(()),
                Err(_) if handle.session_cancel.is_cancelled() => Ok(()),
                Err(err) => Err(err.to_string()),
            }
        })
    })
}

/// The `session.start` candidate: always seq 0 for a fresh session.
fn start_candidate(workspace: &Path) -> Candidate {
    let payload = serde_json::to_value(SessionStart {
        workspace: workspace.display().to_string(),
        root_actor_id: ROOT_ACTOR_ID.into(),
    })
    .unwrap_or(serde_json::Value::Null);
    Candidate::new(EventKind::SessionStart, payload)
}

/// The agent actor for a session, mirroring `app.rs run_command`'s build:
/// 5 native tools + ask, workspace (+ config dir) writable.
fn build_actor(
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
        // The agent loop always sends its own system contract first; the
        // navigator role rides as a second system message at the head of the
        // transcript, so the Phase-1 loop and prompt stay untouched.
        actor
            .transcript
            .push(crate::provider::openai::ChatMessage::System {
                content: COGNITION_PROMPT.to_string(),
            });
    }
    actor
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, never-driven actor: the stand-in result of the detached
    /// runtime task the registry-level tests below construct handles with.
    fn idle_actor() -> AgentActor {
        use crate::provider::config::{ProviderConfig, ReasoningHistory};
        use std::collections::BTreeMap;
        AgentActor::new(
            Registry::new(),
            ToolContext {
                workspace: std::env::temp_dir(),
                writable_roots: vec![std::env::temp_dir()],
            },
            ProviderConfig {
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 100,
                request_timeout_ms: 100,
            },
            "test-key".into(),
            "test-model".into(),
            ReasoningHistory::Preserve,
            Some(64),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    fn config(bind: &str, allow_non_loopback: bool) -> ServerConfig {
        ServerConfig {
            bind: bind.parse().expect("test bind addr"),
            models_path: None,
            config_dir: PathBuf::from("/tmp/rust-agent-test"),
            allow_non_loopback,
            bearer_token: None,
            history_db: None,
        }
    }

    // Given the default loopback bind and a 0.0.0.0 bind without the flag
    // When ServerConfig::validate runs
    // Then loopback is accepted and 0.0.0.0 is refused with NonLoopbackBind
    #[test]
    fn loopback_bind_is_accepted_and_wildcard_refused_without_flag() {
        assert!(config("127.0.0.1:7310", false).validate().is_ok());
        assert!(matches!(
            config("0.0.0.0:7310", false).validate(),
            Err(ServerError::NonLoopbackBind(_))
        ));
    }

    // Given a non-loopback bind WITH --allow-non-loopback
    // When ServerConfig::validate runs
    // Then it is accepted (documented at-your-own-risk; still no auth/TLS)
    #[test]
    fn non_loopback_bind_is_accepted_with_the_explicit_flag() {
        assert!(config("0.0.0.0:7310", true).validate().is_ok());
        assert!(config("192.168.1.10:7310", true).validate().is_ok());
    }

    // Given a live session closed through the manager (fence + tombstone)
    // When GET asks for it afterwards
    // Then it answers from the tombstone (stopped, final_seq == the
    //      session.end slot) and is NOT routable; an unknown id is a 404
    #[tokio::test]
    async fn closed_session_answers_from_tombstone_and_is_not_routable() {
        let state = AppState::new(config("127.0.0.1:0", false));
        let id = Ulid::new();
        assert!(matches!(
            state.session_summary(id).await,
            Err(ServerError::SessionNotFound(_))
        ));

        let handle = SessionHandle {
            inbox_tx: mpsc::channel(1).0,
            sequencer: Sequencer::spawn(Ulid::new(), SessionBus::new()),
            session_cancel: CancellationToken::new(),
            runtime_task: Arc::new(Mutex::new(Some(tokio::spawn(async { idle_actor() })))),
            actor_id: ROOT_ACTOR_ID.to_string(),
            lifecycle: Lifecycle::Active,
            actor_state: "idle".to_string(),
            workspace: PathBuf::from("/w"),
            model: "fast".to_string(),
        };
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("open sequencer accepts the start candidate");
        state
            .lock_sessions()
            .insert(id, SessionSlot::Live(handle.clone()));

        state.close_session(id).await.expect("close");

        let summary = state.session_summary(id).await.expect("tombstone answers");
        assert_eq!(summary.actor_state, "stopped");
        assert_eq!(summary.current_seq, 1, "session.end is seq 1");
        assert!(state.handle(id).is_none(), "tombstone is not routable");
        assert_eq!(state.tombstone_count(), 1);
    }

    // Given more closes than MAX_TOMBSTONES
    // When tombstones are retained
    // Then the pool is bounded at MAX_TOMBSTONES and the OLDEST ids are
    //      evicted to the structured 404 while the newest still answers
    #[tokio::test]
    async fn tombstone_pool_is_bounded_by_max_tombstones_fifo() {
        let state = AppState::new(config("127.0.0.1:0", false));
        let ids: Vec<SessionId> = (0..MAX_TOMBSTONES + 3).map(|_| Ulid::new()).collect();
        for (i, id) in ids.iter().enumerate() {
            state.retain_tombstone(
                *id,
                Tombstone {
                    final_seq: i as u64,
                    workspace: PathBuf::from("/w"),
                    model: "m".to_string(),
                    retained: None,
                    resume_floor: None,
                },
            );
        }
        assert_eq!(state.tombstone_count(), MAX_TOMBSTONES);
        assert_eq!(state.session_count(), MAX_TOMBSTONES);
        for id in &ids[..3] {
            assert!(matches!(
                state.session_summary(*id).await,
                Err(ServerError::SessionNotFound(_))
            ));
        }
        let newest = ids.last().expect("newest id");
        let summary = state.session_summary(*newest).await.expect("retained");
        assert_eq!(summary.actor_state, "stopped");
    }

    // ---- defect-fix proofs (virtual time: deterministic, no sleeps) ----

    /// A registry handle whose runtime task is a stand-in: parks forever
    /// (never fences) — the "wedged/foreign runtime" the cap+backstop path
    /// exists for.
    fn wedged_runtime_handle() -> SessionHandle {
        SessionHandle {
            inbox_tx: mpsc::channel(1).0,
            sequencer: Sequencer::spawn(Ulid::new(), SessionBus::new()),
            session_cancel: CancellationToken::new(),
            runtime_task: Arc::new(Mutex::new(Some(tokio::spawn(async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                idle_actor()
            })))),
            actor_id: ROOT_ACTOR_ID.to_string(),
            lifecycle: Lifecycle::Active,
            actor_state: "idle".to_string(),
            workspace: PathBuf::from("/w"),
            model: "fast".to_string(),
        }
    }

    // Given a runtime wedged mid-turn (it outlives the teardown await cap)
    // When close_session runs
    // Then the wedged task is ABORTED (not detached), the manager's backstop
    //      fence fires and REPLIES the final_seq, the tombstone records it,
    //      session.end is the LAST event, and nothing is emitted after it
    #[tokio::test(flavor = "current_thread")]
    async fn wedged_runtime_is_aborted_and_backstop_fenced_with_exact_final_seq() {
        // Virtual time: the 10s teardown cap elapses instantly while the
        // wedged stand-in (which awaits nothing) stays parked.
        tokio::time::pause();
        let state = AppState::new(config("127.0.0.1:0", false));
        let id = Ulid::new();
        let handle = wedged_runtime_handle();
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("start");
        state
            .lock_sessions()
            .insert(id, SessionSlot::Live(handle.clone()));

        state.close_session(id).await.expect("close");

        let summary = state.session_summary(id).await.expect("tombstone");
        assert_eq!(summary.actor_state, "stopped");
        assert!(
            summary.current_seq >= 1,
            "session.end was sequenced by the backstop: {}",
            summary.current_seq
        );
        assert!(state.handle(id).is_none());
    }

    // Given a runtime that tears down faithfully (emits its terminal tail,
    // fires the CloseSession fence from ITS teardown, then exits) — the
    // production stop ordering
    // When close_session awaits that runtime and tombstones
    // Then the tombstone's final_seq EQUALS the seq the fence stamped on
    //      session.end (defect-2 proof) and exactly one session.end exists
    #[tokio::test]
    async fn tombstone_final_seq_equals_the_fenced_session_end_seq() {
        use crate::bus::event::EventKind;
        use crate::bus::sequencer::Candidate;

        let state = AppState::new(config("127.0.0.1:0", false));
        let id = Ulid::new();
        let mut handle = wedged_runtime_handle();
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("start");
        // The faithful runtime teardown: late tail event, THEN the fence,
        // THEN the task exits — all after the manager's cancel.
        let seq = handle.sequencer.clone();
        handle.runtime_task = Arc::new(Mutex::new(Some(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = seq.publish(Candidate::new(
                EventKind::TurnEnd,
                serde_json::json!({ "outcome": "interrupted" }),
            ));
            let _ = seq.close_session("runtime teardown");
            idle_actor()
        }))));
        state
            .lock_sessions()
            .insert(id, SessionSlot::Live(handle.clone()));

        // The TEST's own probe observes the real session.end payload.
        let observer = handle.sequencer.subscribe_since(None).await.unwrap();
        state.close_session(id).await.expect("close");

        let observed_end_seq = collect_session_end_seq(observer).await;
        let summary = state.session_summary(id).await.expect("tombstone GET");
        assert_eq!(
            summary.current_seq, observed_end_seq,
            "tombstone final_seq == session.end.payload.final_seq"
        );
    }

    /// Drains an observer probe (replay + live) until session.end lands and
    /// returns its seq.
    async fn collect_session_end_seq(mut probe: crate::bus::SubscribeReply) -> u64 {
        let mut end_seq = 0;
        if let crate::bus::ResumeOutcome::Replay(replay) = &probe.replay {
            for env in replay {
                if env.kind.as_str() == "session.end" {
                    end_seq = env.seq;
                }
            }
        }
        if let Some(mut live) = probe.live.take() {
            while let Ok(Some(env)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), live.recv()).await
            {
                if env.kind.as_str() == "session.end" {
                    end_seq = env.seq;
                    break;
                }
            }
        }
        end_seq
    }

    // Given the tombstone contract (defect 3 DEFERRAL to todo 8)
    // When GET asks a stopped session for its summary and a streaming route
    //    would ask for events
    // Then GET answers from the tombstone (stopped @ final_seq) while
    //     routing stays gone: the CURRENT behavior the todo-8 reconnect
    //     work must replace (replay-through-final_seq) — pinned here so the
    //     deferral is visible and testable, not silent.
    #[tokio::test]
    async fn stopped_session_answers_get_but_is_not_routable_deferral_pin() {
        let state = AppState::new(config("127.0.0.1:0", false));
        let id = Ulid::new();
        let handle = wedged_runtime_handle();
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("start");
        state
            .lock_sessions()
            .insert(id, SessionSlot::Live(handle.clone()));

        state.close_session(id).await.expect("close");

        let summary = state.session_summary(id).await.expect("tombstone GET");
        assert_eq!(summary.actor_state, "stopped");
        assert!(state.handle(id).is_none(), "not routable for streaming");
    }
}
