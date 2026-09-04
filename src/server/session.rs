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
//! allow: SIZE_OK — ticket 05's approved module map fixes the file set
//! (mod/routes/sse/websocket/session); the manager's registry + lifecycle +
//! actor construction must stay in this one file.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::agent::actor::AgentActor;
use crate::bus::command::Command;
use crate::bus::event::{EventKind, SessionStart, ROOT_ACTOR_ID};
use crate::bus::inbox::ActorInbox;
use crate::bus::runtime;
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::bus::SessionBus;
use crate::error::{ConfigError, ServerError};
use crate::provider::auth;
use crate::provider::config::{resolve_models_path, ModelsConfig, ResolvedModel};
use crate::tools::{Registry, ToolContext};

/// Retained closed-session tombstones before the OLDEST is evicted (FIFO).
/// Live sessions are never evicted — they are active. Only the tombstone
/// pool is bounded, so clients cannot grow the registry unboundedly through
/// create/stop churn.
pub const MAX_TOMBSTONES: usize = 128;

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
#[derive(Clone, Debug)]
pub struct SessionHandle {
    /// Bounded per-actor command transport (todo 7 posts here).
    pub inbox_tx: mpsc::Sender<Command>,
    /// The session's ONE authoritative event sequencer.
    pub sequencer: Sequencer,
    /// Fires on stop/shutdown; the runtime task tears the session down.
    pub session_cancel: CancellationToken,
    /// Always `agent:main` this slice (the single root actor).
    pub actor_id: String,
    pub lifecycle: Lifecycle,
    /// Last state the manager knows about ("idle" at creation).
    pub actor_state: String,
    pub workspace: PathBuf,
    /// The alias the session was created with.
    pub model: String,
}

/// A closed session's retained record (P1-3 tombstone). Holds ONLY what a
/// late GET needs — NO `inbox_tx`, NO `Sequencer`, NO cancellation token:
/// the routing resources are released when the live slot is replaced.
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
}

impl AppState {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            tombstone_order: Arc::new(Mutex::new(VecDeque::new())),
            config: Arc::new(config),
        }
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

    /// Creates a session: resolves models.json + the key (NO live provider
    /// call — the actor is built but not run until a message arrives),
    /// mints tokens, spawns the sequencer FIRST, publishes `session.start`
    /// (seq 0), THEN spawns the runtime task, and registers the handle.
    pub fn create_session(
        &self,
        workspace: &Path,
        model: Option<&str>,
    ) -> Result<SessionId, ServerError> {
        let alias = model.unwrap_or("default");
        let resolved = self.resolve_model(workspace, alias)?;
        let api_key = auth::resolve_key(
            &self.config.config_dir,
            &resolved.provider_name,
            resolved.provider.api_key_env.as_deref(),
        )?;

        let session_id = Ulid::new();
        let session_cancel = CancellationToken::new();
        let actor = build_actor(workspace, &self.config.config_dir, resolved, api_key, {
            session_cancel.child_token()
        });

        // (1) sequencer FIRST — it must exist before anyone publishes.
        let sequencer = Sequencer::spawn(session_id, SessionBus::new());
        // (2) session.start is seq 0, sequenced before the runtime exists.
        let _ = sequencer.publish(start_candidate(workspace));
        // (3) THEN the runtime task.
        let (inbox_tx, inbox) = ActorInbox::channel();
        let _runtime_task = runtime::spawn(
            session_id,
            actor,
            inbox,
            sequencer.clone(),
            session_cancel.clone(),
        );

        self.lock_sessions().insert(
            session_id,
            SessionSlot::Live(SessionHandle {
                inbox_tx,
                sequencer,
                session_cancel,
                actor_id: ROOT_ACTOR_ID.to_string(),
                lifecycle: Lifecycle::Active,
                actor_state: "idle".to_string(),
                workspace: workspace.to_path_buf(),
                model: alias.to_string(),
            }),
        );
        Ok(session_id)
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
    /// `Stopping` FIRST (so concurrent requests see it), then the sequencer's
    /// `CloseSession` fence is fired — the SEQUENCER sequences
    /// `session.end{final_seq}`; the manager reads NO event — and the live
    /// slot is replaced by a bounded [`Tombstone`]. Idempotent: an
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

    /// Fire the CloseSession fence (the sequencer emits + owns the
    /// session.end event), cancel the session token, then swap the live
    /// slot for the bounded tombstone. No lock is held across any await.
    async fn fence_and_tombstone(&self, id: SessionId, handle: SessionHandle, reason: &str) {
        // The fence assigns session.end exactly this seq: head = next
        // unassigned seq (FIFO ingress guarantees the head read lands after
        // every already-published event).
        let final_seq = handle.sequencer.current_seq().await.unwrap_or(0);
        let _ = handle.sequencer.close_session(reason);
        handle.session_cancel.cancel();
        self.retain_tombstone(
            id,
            Tombstone {
                final_seq,
                workspace: handle.workspace.clone(),
                model: handle.model.clone(),
            },
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(bind: &str, allow_non_loopback: bool) -> ServerConfig {
        ServerConfig {
            bind: bind.parse().expect("test bind addr"),
            models_path: None,
            config_dir: PathBuf::from("/tmp/rust-agent-test"),
            allow_non_loopback,
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
}
