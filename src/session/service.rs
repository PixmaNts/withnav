//! The session `Service`: a registry + router over per-session sequencer
//! and runtime handles. It processes NO events: it mints sessions (sequencer
//! spawned FIRST, `session.start` published, THEN the runtime task), answers
//! create/get, and hands teardown to [`super::lifecycle`]. No HTTP type
//! appears here, so the CLI and the HTTP adapter drive the same service.
//!
//! Two notions of "gone", which are NOT the same thing:
//!
//! - Runtime stopped: the in-memory actor and sequencer no longer exist, so
//!   nothing can be posted to the session. The registry entry is removed
//!   once `session.end` is stored.
//! - Conversation resumable: the SQLite store ([`super::store`]) holds
//!   the conversation, and is the only history. `POST /v1/sessions` with
//!   `resume` rebuilds a fresh actor and sequencer under the SAME id,
//!   continuing at `last_seq + 1`.
//!
//! `GET /v1/sessions/:id` and the SSE route answer a stored conversation
//! with no registry entry (stopped, or from before a restart) from the
//! store, as "stopped" at its `last_seq`; a message post to it is a 409
//! asking to resume first. Only an id the store does not know is a 404.
//!
//! Lifecycle gate: resume, close and delete take one async mutex for their
//! whole run, so an id never has two runtimes and is never resumed while it
//! is stopping or being deleted (the resume waits, then proceeds).
//!
//! Lock discipline: the registry is a `std::sync::Mutex`
//! whose critical sections are short and purely synchronous. Every `.await`
//! happens AFTER the guard is dropped — clone the [`SessionHandle`] (its
//! `mpsc::Sender` and [`Sequencer`] are `Clone`), release the lock, THEN
//! await. clippy's `await_holding_lock` lint enforces this.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::actor::{build_actor, start_candidate};
use super::config::SessionConfig;
use super::hooks::{NoHooks, SessionHooks, StartedSession};
use super::message::{accept, MessageAccepted};
use super::sender::SessionSender;
use crate::agent::actor::AgentActor;
use crate::agent::instructions::Instructions;
use crate::bus::command::Command;
use crate::bus::event::ROOT_ACTOR_ID;
use crate::bus::inbox::ActorInbox;
use crate::bus::runtime;
use crate::bus::sequencer::Sequencer;
use crate::bus::SessionBus;
use crate::error::ServerError;
use crate::provider::auth;
use crate::provider::config::{load_models, ApiKind, ModelConfig, ResolvedModel};
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

/// What a resume restores from the store: the row and the last transcript
/// snapshot. The envelopes stay in the store.
struct ResumedSession {
    meta: super::store::StoredSession,
    transcript: Vec<ChatMessage>,
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

/// The session service: the registry plus the immutable config. Cheap to
/// clone (all fields are `Arc`).
#[derive(Clone, Debug)]
pub struct Service {
    /// The live sessions of this process; a closed session is removed.
    pub(super) sessions: Arc<Mutex<HashMap<SessionId, SessionHandle>>>,
    pub(super) config: Arc<SessionConfig>,
    /// The conversation-history store: every conversation is persisted.
    history: Arc<super::store::HistoryStore>,
    /// Actor-profile policy injected by the host binary.
    hooks: Arc<dyn SessionHooks>,
    /// The lifecycle gate: resume, close and delete run one at a time, so a
    /// session id never has two runtimes and is never resumed while it is
    /// being stopped or deleted. One client, so one gate for the service.
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl Service {
    /// A service without any profile behaviour ([`NoHooks`]). Fails when
    /// the history database cannot be opened.
    pub fn new(config: SessionConfig) -> Result<Self, ServerError> {
        Self::with_hooks(config, Arc::new(NoHooks))
    }

    /// [`Service::new`] with the host's profile policy and lifecycle
    /// callbacks.
    pub fn with_hooks(
        config: SessionConfig,
        hooks: Arc<dyn SessionHooks>,
    ) -> Result<Self, ServerError> {
        // No fallback: a conversation that cannot be saved is not served.
        let history =
            super::store::HistoryStore::open(&config.history_db).map_err(ServerError::History)?;
        Ok(Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
            history: Arc::new(history),
            hooks,
            gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Takes the lifecycle gate. Hold the guard across the whole of a
    /// resume, close or delete; never call a gated method while holding it.
    pub(super) async fn lifecycle_gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }

    /// The history store.
    pub fn history(&self) -> &super::store::HistoryStore {
        &self.history
    }

    /// The sessions the registry holds (live or stopping). Client
    /// connects/disconnects never touch the registry: only session creation
    /// inserts and each session occupies one entry, so a connect-then-drop
    /// loop cannot grow it.
    pub fn session_count(&self) -> usize {
        self.lock_sessions().len()
    }

    /// A clone of the session's handle, for routing without holding the
    /// lock. `None` for a closed or unknown id.
    pub(crate) fn handle(&self, id: SessionId) -> Option<SessionHandle> {
        self.lock_sessions().get(&id).cloned()
    }

    /// A clone of the handle, gated on the lifecycle:
    /// `Err(SessionStopping)` (409) for a Stopping session,
    /// `Err(ResumeRequired)` (409) for a stopped conversation (stored only),
    /// `Err(SessionNotFound)` (404) for an unknown or deleted id. No lock is
    /// held past the lookup — the caller awaits only on the clone.
    pub(crate) async fn routable_handle(
        &self,
        id: SessionId,
    ) -> Result<SessionHandle, ServerError> {
        match self.lock_sessions().get(&id) {
            Some(handle) if handle.lifecycle == Lifecycle::Active => {
                return Ok(handle.clone());
            }
            Some(_) => return Err(ServerError::SessionStopping),
            None => {}
        }
        let stored = self
            .history
            .exists(id)
            .await
            .map_err(ServerError::History)?;
        Err(if stored {
            ServerError::ResumeRequired
        } else {
            ServerError::SessionNotFound(id.to_string())
        })
    }

    /// Accepts one command into its session: the session must be live and
    /// active, the target the root agent and the content text-only. The
    /// caller names the source actor. `message.received` is sequenced before
    /// the command reaches the inbox, and only for an accepted command.
    pub async fn send(&self, command: Command) -> Result<MessageAccepted, ServerError> {
        let handle = self.routable_handle(command.session_id).await?;
        accept(&handle, command).await
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
    /// - stopped, or only stored → rebuilt from the store under the SAME id
    ///   with its stored model and profile: the sequencer continues at
    ///   `last_seq + 1`, NO new `session.start`, the actor's transcript
    ///   restored from the snapshot (a subscriber replays the past
    ///   conversation from the store);
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
            // A resume waits for any stop or delete of this id to finish,
            // and holds the gate until the new runtime is registered.
            let _gate = self.lifecycle_gate().await;
            // Live + active: the conversation never left.
            if let Some(handle) = self.lock_sessions().get(&id) {
                if handle.lifecycle == Lifecycle::Active {
                    return Ok((id, true));
                }
            }
            // Stopped, or only on disk: rebuild under the same id from the
            // stored row.
            let store = &self.history;
            // The closing session's last events (`session.end` included) may
            // still be queued: the load must see them, or the resumed
            // sequence could restart below a seq a client already saw.
            store.flush().await.map_err(ServerError::History)?;
            let Some((meta, transcript)) = store
                .load_for_resume(id)
                .await
                .map_err(ServerError::History)?
            else {
                return Err(ServerError::SessionNotFound(id.to_string()));
            };
            let loaded = ResumedSession { meta, transcript };
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
    /// 404; a stored conversation with no registry entry answers "stopped"
    /// at its stored `last_seq`. `current_seq` = last EMITTED seq: for a
    /// live session it is the sequencer head minus one.
    pub async fn session_summary(&self, id: SessionId) -> Result<SessionSummary, ServerError> {
        // Clone out of the guard; the head query below is a plain sequencer
        // round-trip (the manager reads no events).
        let Some(handle) = self.handle(id) else {
            return self.stored_summary(id).await;
        };
        let current_seq = handle
            .sequencer
            .current_seq()
            .await
            .map(|next| next.saturating_sub(1))
            .unwrap_or(0);
        Ok(SessionSummary {
            session_id: id.to_string(),
            actor_state: handle.actor_state.clone(),
            workspace: handle.workspace.display().to_string(),
            model: handle.model.clone(),
            current_seq,
        })
    }

    /// The summary of a conversation that only the store holds (stopped, or
    /// from before a restart): stopped at its stored `last_seq`.
    async fn stored_summary(&self, id: SessionId) -> Result<SessionSummary, ServerError> {
        let meta = self
            .history
            .meta(id)
            .await
            .map_err(ServerError::History)?
            .ok_or_else(|| ServerError::SessionNotFound(id.to_string()))?;
        Ok(SessionSummary {
            session_id: id.to_string(),
            actor_state: "stopped".to_string(),
            workspace: meta.workspace,
            model: meta.model,
            current_seq: meta.last_seq,
        })
    }

    pub(super) fn lock_sessions(&self) -> MutexGuard<'_, HashMap<SessionId, SessionHandle>> {
        // Poison recovery via into_inner: a panicking writer must not wedge
        // the registry (same policy as the runtime observers).
        match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The aliases a cognition session can use: chat models with tools,
    /// from the same models.json a new session would load.
    pub fn list_models(&self, workspace: &Path) -> Result<Vec<(String, ModelConfig)>, ServerError> {
        let (config, _) = load_models(
            self.config.models_path.as_deref(),
            workspace,
            &self.config.config_dir,
        )?;
        Ok(config
            .models
            .into_iter()
            .filter(|(_, m)| m.api == ApiKind::Chat && m.supports_tools)
            .collect())
    }

    pub fn default_model(&self, profile: Option<&str>) -> Option<&str> {
        self.hooks.default_model(profile)
    }

    fn resolve_model(&self, workspace: &Path, alias: &str) -> Result<ResolvedModel, ServerError> {
        let (config, _) = load_models(
            self.config.models_path.as_deref(),
            workspace,
            &self.config.config_dir,
        )?;
        Ok(config.resolve(alias)?)
    }

    /// Reads the session's local instructions once and logs what became of
    /// each source. The snapshot lives on the actor for the whole session.
    async fn load_rules(&self, workspace: &Path, id: SessionId, resumed: bool) -> Instructions {
        let roots = self.config.rules_roots.clone();
        let dir = workspace.to_path_buf();
        let rules = tokio::task::spawn_blocking(move || Instructions::load(&roots, &dir))
            .await
            .unwrap_or_default();
        super::rules_log::log(id, resumed, &rules);
        rules
    }

    /// The shared mint: resolves the model, builds the actor, spawns the
    /// sequencer (continuing at `last_seq + 1` when resuming),
    /// publishes `session.start` (fresh only), spawns the runtime with the
    /// history hooks, registers the handle. `resume_workspace` is the
    /// workspace a resume request named; the FRESH path passes its own.
    async fn spawn_session(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        resumed: Option<ResumedSession>,
        resume_workspace: &Path,
    ) -> Result<SessionId, ServerError> {
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
                let alias = model
                    .or_else(|| self.hooks.default_model(profile))
                    .unwrap_or("default");
                (workspace.to_path_buf(), alias.to_string(), profile)
            }
        };
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
        );
        if let Some(loaded) = &resumed {
            actor.transcript = loaded.transcript.clone();
        }
        actor.rules = self
            .load_rules(&workspace, session_id, resumed.is_some())
            .await;
        self.hooks.configure_actor(profile, &mut actor);
        if resumed.is_some() {
            actor.restore_pending_ask();
        }

        let next_seq = resumed
            .as_ref()
            .map(|loaded| loaded.meta.last_seq + 1)
            .unwrap_or(0);
        // The in-memory ring starts empty on resume: subscribers replay from
        // the store, and the ring is only a bounded fallback cache.
        let bus = SessionBus::default();
        let is_resume = resumed.is_some();

        // History hooks: a persisting emitter + a turn-end transcript
        // snapshot.
        let history = self.history.clone();
        // Acknowledged: a session whose row cannot be written would
        // silently lose every later event and snapshot.
        history
            .record_session(
                session_id,
                &workspace.display().to_string(),
                &alias,
                profile,
            )
            .await
            .map_err(ServerError::History)?;
        let sequencer = Sequencer::spawn(
            session_id,
            super::store::PersistingEmitter::new(session_id, bus, (*history).clone()),
            next_seq,
        );

        if !is_resume {
            // session.start is seq 0, sequenced before the runtime exists.
            let _ = sequencer.publish(start_candidate(&workspace));
        }
        // THEN the runtime task; the handle is retained so /stop can await
        // the interrupted turn's events before the session.end fence.
        let (inbox_tx, inbox) = ActorInbox::channel();
        let on_turn_end: Option<crate::bus::runtime::TurnEndHook> =
            Some(Box::new(move |transcript: &[ChatMessage]| {
                history.save_transcript(session_id, transcript.to_vec());
            }));
        let runtime_task = Arc::new(Mutex::new(Some(runtime::spawn(
            session_id,
            actor,
            inbox,
            sequencer.clone(),
            session_cancel.clone(),
            on_turn_end,
        ))));

        let handle = SessionHandle {
            inbox_tx,
            sequencer,
            session_cancel,
            runtime_task,
            actor_id: ROOT_ACTOR_ID.to_string(),
            lifecycle: Lifecycle::Active,
            actor_state: "idle".to_string(),
            workspace: workspace.clone(),
            model: alias,
        };
        let sender = SessionSender::new(session_id, handle.clone());
        let cancel = handle.session_cancel.clone();
        self.lock_sessions().insert(session_id, handle);
        // The host learns of the session once it is routable. A session
        // already closing hands back a subscription that ends at once.
        if let Ok(subscription) = self.subscribe(session_id, None).await {
            self.hooks.started(
                StartedSession {
                    id: session_id,
                    profile: profile.map(str::to_string),
                    workspace,
                    resumed: is_resume,
                },
                sender,
                subscription,
                cancel,
            );
        }
        Ok(session_id)
    }
}

#[cfg(test)]
pub(crate) mod tests;
