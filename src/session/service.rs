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
use crate::agent::context::{ContextLimits, ContextSnapshot, ContextTracker};
use crate::agent::definitions::{AgentDefinition, AgentId, AgentRoots, Agents};
use crate::agent::instructions::Instructions;
use crate::agent::selection::AgentSelection;
use crate::agent::skills::Skills;
use crate::bus::command::Command;
use crate::bus::event::EventKind;
use crate::bus::event::ROOT_ACTOR_ID;
use crate::bus::inbox::ActorInbox;
use crate::bus::runtime;
use crate::bus::sequencer::Candidate;
use crate::bus::sequencer::Sequencer;
use crate::bus::SessionBus;
use crate::error::ServerError;
use crate::provider::config::{load_models, ApiKind, ModelConfig};
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
    /// The alias and saved resolution the session talks to now: they change
    /// when `/model` switches it.
    pub selected: Arc<Mutex<SelectedModel>>,
    /// Typed decisions on permission requests, read by the runtime.
    pub permissions: mpsc::Sender<crate::bus::inbox::PermissionAnswer>,
    /// The runtime's view of its actor: pending `ask`, running turn.
    pub status: Arc<runtime::RuntimeStatus>,
    /// What the session's model offers as variants; replaced when an offer
    /// that was unknown at the start becomes known.
    pub offer: Arc<Mutex<Arc<resolve::Offer>>>,
}

/// The model selection of a live session, as the service sees it.
#[derive(Debug, Clone)]
pub struct SelectedModel {
    pub alias: String,
    /// The saved resolution (identity, window, where the window came from)
    /// the session runs on.
    pub resolution: Option<Arc<crate::provider::catalog::ModelResolution>>,
    /// 1 for the first selection of the conversation, +1 for each switch.
    pub revision: u64,
}

impl SessionHandle {
    fn selected(&self) -> SelectedModel {
        match self.selected.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The alias the session talks to now.
    pub fn model(&self) -> String {
        self.selected().alias
    }

    /// The saved resolution the session runs on.
    pub fn resolution(&self) -> Option<Arc<crate::provider::catalog::ModelResolution>> {
        self.selected().resolution
    }

    /// The session's current variant offer.
    pub(crate) fn current_offer(&self) -> Arc<resolve::Offer> {
        match self.offer.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub(crate) fn replace_offer(&self, offer: resolve::Offer) {
        let mut guard = match self.offer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Arc::new(offer);
    }
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("actor_id", &self.actor_id)
            .field("lifecycle", &self.lifecycle)
            .field("actor_state", &self.actor_state)
            .field("workspace", &self.workspace)
            .field("model", &self.model())
            .finish_non_exhaustive()
    }
}

/// What a resume restores from the store: the row and the last transcript
/// snapshot. The envelopes stay in the store.
struct ResumedSession {
    meta: super::store::StoredSession,
    transcript: Vec<ChatMessage>,
    compaction: Option<crate::agent::compaction::Compaction>,
}

/// Frozen `GET /v1/sessions/:id` body.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub actor_state: String,
    pub workspace: String,
    pub model: String,
    /// The primary agent the session runs as.
    pub agent: String,
    /// The model variant in use; `null` for none.
    pub variant: Option<String>,
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
    catalog: crate::provider::catalog::ModelCatalog,
    /// Test seam: parks a command run between its work and the publication
    /// of its result, to interleave lifecycle changes deterministically.
    #[cfg(test)]
    pub(super) publish_barrier: Arc<Mutex<Option<Arc<super::run_command::Barrier>>>>,
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
        let catalog = crate::provider::catalog::ModelCatalog::new(Some(config.data_dir.clone()));
        Ok(Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
            history: Arc::new(history),
            hooks,
            gate: Arc::new(tokio::sync::Mutex::new(())),
            catalog,
            #[cfg(test)]
            publish_barrier: Arc::default(),
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

    /// The permission requests of a stored session that never got a
    /// `permission.resolved`: `(request id, tool call id)`, oldest first.
    async fn unresolved_permissions(
        &self,
        id: SessionId,
    ) -> Result<Vec<(Ulid, String)>, ServerError> {
        use crate::bus::event::{PermissionRequested, PermissionResolved};
        let events = self
            .history
            .permission_events(id)
            .await
            .map_err(ServerError::History)?;
        let mut open: Vec<(Ulid, String)> = Vec::new();
        for envelope in events {
            if envelope.kind == EventKind::PermissionRequested.as_str() {
                if let Ok(request) = serde_json::from_value::<PermissionRequested>(envelope.payload)
                {
                    open.push((request.request_id, request.tool_call_id));
                }
            } else if let Ok(done) = serde_json::from_value::<PermissionResolved>(envelope.payload)
            {
                open.retain(|(request_id, _)| *request_id != done.request_id);
            }
        }
        Ok(open)
    }

    /// Answers the permission request `request_id` of a live session with
    /// `decision`. The request must be the one the session waits on:
    /// [`ServerError::PermissionStale`] for one that was answered, cancelled,
    /// expired or never existed. Returns once the runtime accepted the
    /// decision; the resumed turn then runs on its own and reports on the
    /// session stream (`permission.resolved`, then the turn's events).
    pub async fn answer_permission(
        &self,
        session_id: SessionId,
        request_id: Ulid,
        decision: crate::agent::permissions::PermissionDecision,
    ) -> Result<(), ServerError> {
        let handle = self.routable_handle(session_id).await?;
        // Claimed at admission: a second answer for the same request is
        // refused at once, not after the resumed turn.
        if !handle.status.claim_permission(request_id) {
            return Err(ServerError::PermissionStale);
        }
        let (reply, accepted) = tokio::sync::oneshot::channel();
        if handle
            .permissions
            .try_send(crate::bus::inbox::PermissionAnswer {
                request_id,
                decision,
                reply,
            })
            .is_err()
        {
            handle.status.release_permission(request_id);
            return Err(ServerError::PermissionStale);
        }
        match accepted.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => {
                handle.status.release_permission(request_id);
                Err(ServerError::PermissionStale)
            }
            // The runtime ended before it read the decision.
            Err(_) => Err(ServerError::SessionStopping),
        }
    }

    /// Creates a session: resolves models.json + the key and the context
    /// window (no chat call: the actor is built but not run until a message
    /// arrives; the provider's model list is read only when the alias has no
    /// window of its own),
    /// mints tokens, spawns the sequencer FIRST, publishes `session.start`
    /// (seq 0), THEN spawns the runtime task, and registers the handle.
    pub async fn create_session(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
    ) -> Result<SessionId, ServerError> {
        self.create_session_as(workspace, model, profile, None)
            .await
    }

    /// [`create_session`](Self::create_session) running as the primary agent
    /// `agent` (`build` when `None`).
    pub async fn create_session_as(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        agent: Option<&str>,
    ) -> Result<SessionId, ServerError> {
        self.create_session_with(workspace, model, profile, VariantChoice::Default, agent)
            .await
    }

    /// [`create_session`] with the variant to start on: the alias default,
    /// none, or a variant the alias offers (anything else is an error). The
    /// variant is saved with the session.
    pub async fn create_session_with_variant(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        variant: VariantChoice,
    ) -> Result<SessionId, ServerError> {
        self.create_session_with(workspace, model, profile, variant, None)
            .await
    }

    /// [`create_session_with_variant`](Self::create_session_with_variant)
    /// running as the primary agent `agent` (`build` when `None`).
    pub async fn create_session_with(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        variant: VariantChoice,
        agent: Option<&str>,
    ) -> Result<SessionId, ServerError> {
        self.spawn_session(workspace, model, profile, agent, None, workspace, &variant)
            .await
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
        self.create_or_resume_session_as(workspace, model, profile, None, resume)
            .await
    }

    /// [`create_or_resume_session`](Self::create_or_resume_session) with the
    /// primary agent a NEW session runs as. A resumed session keeps the agent
    /// it was stored with: `agent` is ignored for it.
    pub async fn create_or_resume_session_as(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        agent: Option<&str>,
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
            let Some((meta, transcript, compaction)) = store
                .load_for_resume(id)
                .await
                .map_err(ServerError::History)?
            else {
                return Err(ServerError::SessionNotFound(id.to_string()));
            };
            let loaded = ResumedSession {
                meta,
                transcript,
                compaction,
            };
            self.spawn_session(
                workspace,
                None,
                None,
                None,
                Some(loaded),
                workspace,
                &VariantChoice::Default,
            )
            .await?;
            return Ok((id, true));
        }
        let id = self
            .create_session_with(workspace, model, profile, VariantChoice::Default, agent)
            .await?;
        Ok((id, false))
    }

    /// The session's context snapshot: the estimate of its next request by
    /// category, the budget, and the provider's count for the last one. The
    /// same value the last `context.updated` carried.
    pub async fn context(&self, id: SessionId) -> Result<ContextSnapshot, ServerError> {
        let handle = self.routable_handle(id).await?;
        handle
            .status
            .context_snapshot()
            .ok_or_else(|| ServerError::SessionNotFound(id.to_string()))
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
            model: handle.model(),
            agent: handle.status.selection.current().id.to_string(),
            variant: handle.status.selected_variant(),
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
            agent: meta
                .primary_agent
                .unwrap_or_else(|| AgentId::build().to_string()),
            variant: meta.variant,
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

    /// Finds the session's skills once and logs what became of each file.
    async fn load_skills(&self, workspace: &Path, id: SessionId, resumed: bool) -> Skills {
        let roots = self.config.rules_roots.clone();
        let dir = workspace.to_path_buf();
        let skills = tokio::task::spawn_blocking(move || Skills::load(&roots, &dir))
            .await
            .unwrap_or_default();
        super::rules_log::log_skills(id, resumed, &skills);
        skills
    }

    /// Finds the agents of a session once: the built-ins and every agent file.
    async fn load_agents(&self, workspace: &Path, id: SessionId, resumed: bool) -> Arc<Agents> {
        let roots = AgentRoots {
            home: self.config.rules_roots.home.clone(),
            config_dir: Some(self.config.config_dir.clone()),
        };
        let dir = workspace.to_path_buf();
        let agents = tokio::task::spawn_blocking(move || Agents::load(&roots, &dir))
            .await
            .unwrap_or_default();
        super::rules_log::log_agents(id, resumed, &agents);
        Arc::new(agents)
    }

    /// The agent a session starts with. A new session takes the requested
    /// one (`build` by default), which must be primary-capable. A resumed one
    /// takes its stored agent (`build` for a row without one); one that no
    /// longer exists or can no longer be selected is an error, never a
    /// silent `build`.
    fn pick_agent(
        agents: &Agents,
        session_id: SessionId,
        requested: Option<&str>,
        resumed: Option<&ResumedSession>,
    ) -> Result<Arc<AgentDefinition>, ServerError> {
        if let Some(loaded) = resumed {
            let saved = loaded
                .meta
                .primary_agent
                .as_deref()
                .unwrap_or(crate::agent::definitions::BUILD);
            let unavailable = |reason: &str| ServerError::SavedAgentUnavailable {
                session: session_id.to_string(),
                agent: saved.to_string(),
                reason: reason.to_string(),
            };
            let definition = agents
                .get(saved)
                .ok_or_else(|| unavailable("no agent definition with this id was found"))?;
            if !definition.mode.is_primary() {
                return Err(unavailable("it is no longer a primary agent"));
            }
            return Ok(definition.clone());
        }
        let wanted = requested.unwrap_or(crate::agent::definitions::BUILD);
        match agents.get(wanted) {
            Some(definition) if definition.mode.is_primary() => Ok(definition.clone()),
            _ => Err(ServerError::UnknownAgent {
                agent: wanted.to_string(),
                available: primary_list(agents),
            }),
        }
    }

    /// The shared mint: resolves the model, builds the actor, spawns the
    /// sequencer (continuing at `last_seq + 1` when resuming),
    /// publishes `session.start` (fresh only), spawns the runtime with the
    /// history hooks, registers the handle. `resume_workspace` is the
    /// workspace a resume request named; the FRESH path passes its own.
    // Private, with exactly two callers (fresh and resume) that pass every
    // selection through unchanged; a parameter struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_session(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        agent: Option<&str>,
        resumed: Option<ResumedSession>,
        resume_workspace: &Path,
        variant: &VariantChoice,
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
        // The agent is chosen before anything is built or written: an unknown
        // agent fails a fresh session, an unavailable saved one fails the
        // resume, and neither leaves a row behind.
        let agents = self
            .load_agents(&workspace, session_id, resumed.is_some())
            .await;
        let role = Self::pick_agent(&agents, session_id, agent, resumed.as_ref())?;
        let (resolved, api_key, snapshot, save_late, models_file) = self
            .resolve_for_spawn(session_id, resume_workspace, &alias, resumed.as_ref())
            .await?;
        // A resume keeps the variant the row saved; a fresh session takes the
        // request's choice. Either way it is checked against what the alias
        // offers now, before anything is written.
        let (selected, offered) = self
            .select_variant(
                session_id,
                &alias,
                &resolved,
                &api_key,
                &models_file,
                match &resumed {
                    Some(loaded) => {
                        VariantStart::Saved(loaded.meta.variant.as_deref().map(|name| {
                            resolve::SavedVariant {
                                name,
                                override_json: loaded.meta.variant_override.as_deref(),
                            }
                        }))
                    }
                    None => VariantStart::Choice(variant),
                },
            )
            .await?;
        // A resume with a readable offer keeps the saved override current
        // with the definition it now has.
        if let (Some(loaded), Some(chosen)) = (&resumed, &selected) {
            let current = serde_json::to_string(&chosen.1).ok();
            if offered.known && current != loaded.meta.variant_override {
                self.history
                    .set_variant(session_id, Some(chosen))
                    .await
                    .map_err(ServerError::History)?;
            }
        }
        let limits = ContextLimits::from_resolution(
            &alias,
            selected.as_ref().map(|(name, _)| name.as_str()),
            &snapshot,
            resolved.model.max_output_tokens,
        )
        .map_err(|e| ServerError::ImpossibleBudget {
            alias: alias.clone(),
            reason: e.to_string(),
        })?;
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
            actor.compaction = loaded.compaction.clone();
            // The task the role guard judges against: the driver's latest
            // stored words, not a navigator wake.
            actor.driver_task = self
                .history
                .latest_driver_message(session_id)
                .await
                .map_err(ServerError::History)?;
        }
        actor.rules = self
            .load_rules(&workspace, session_id, resumed.is_some())
            .await;
        let skills = self
            .load_skills(&workspace, session_id, resumed.is_some())
            .await;
        actor.set_skills(skills);
        actor.guard = super::guard::jev_guard(
            self.config.models_path.as_deref(),
            &workspace,
            &self.config.config_dir,
        );
        self.hooks.configure_actor(profile, &mut actor);
        // A permission request does not outlive the process that asked it:
        // its calls are settled as interrupted and never run again.
        let expired = match &resumed {
            Some(_) => {
                actor.restore_pending_ask();
                actor.settle_unanswered_calls();
                self.unresolved_permissions(session_id).await?
            }
            None => Vec::new(),
        };

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
            .record_session_resolved(
                session_id,
                &workspace.display().to_string(),
                &alias,
                profile,
                Some(&snapshot),
                selected.as_ref(),
                // Written with the row: a resumed row already has its agent.
                (!is_resume && !role.id.is_build()).then(|| role.id.as_str()),
            )
            .await
            .map_err(ServerError::History)?;
        if save_late {
            self.save_legacy_snapshot(session_id, &snapshot).await?;
        }
        let sequencer = Sequencer::spawn(
            session_id,
            super::store::PersistingEmitter::new(session_id, bus, (*history).clone()),
            next_seq,
        );

        for (request_id, tool_call_id) in expired {
            let payload = serde_json::to_value(crate::bus::event::PermissionResolved {
                request_id,
                tool_call_id,
                resolution: crate::bus::event::PermissionResolution::Expired,
            })
            .unwrap_or(serde_json::Value::Null);
            let _ = sequencer.publish(Candidate::new(EventKind::PermissionResolved, payload));
        }

        if !is_resume {
            // session.start is seq 0, sequenced before the runtime exists.
            let _ = sequencer.publish(start_candidate(&workspace));
        }
        // THEN the runtime task; the handle is retained so /stop can await
        // the interrupted turn's events before the session.end fence.
        let (inbox_tx, inbox) = ActorInbox::channel();
        let permissions = inbox.permission_sender();
        let saver_history = history.clone();
        let on_turn_end: Option<crate::bus::runtime::TurnEndHook> =
            Some(Box::new(move |transcript: &[ChatMessage]| {
                history.save_transcript(session_id, transcript.to_vec());
            }));
        let status = Arc::new(runtime::RuntimeStatus::with_selection(AgentSelection::new(
            agents, role,
        )));
        status.init_variant(selected);
        // A compaction is saved with the transcript, in one acknowledged write.
        status.install_checkpoint_saver(Arc::new(move |transcript, compaction| {
            let history = saver_history.clone();
            Box::pin(async move {
                history
                    .save_checkpoint(session_id, transcript, &compaction)
                    .await
            })
        }));
        // The selection history is the stored `model.changed` events: a
        // resumed conversation goes on counting from the last one.
        let revision = if is_resume {
            self.history
                .model_events(session_id)
                .await
                .map_err(ServerError::History)?
                .last()
                .and_then(|event| event.payload["revision"].as_u64())
                .unwrap_or(1)
        } else {
            1
        };
        let selected_model = Arc::new(Mutex::new(SelectedModel {
            alias: alias.clone(),
            resolution: Some(Arc::new(snapshot.clone())),
            revision,
        }));
        let offer = Arc::new(Mutex::new(Arc::new(offered)));
        status.install_model_switcher(self.model_switcher(
            session_id,
            workspace.clone(),
            selected_model.clone(),
            offer.clone(),
        ));
        let limits = limits.with_sources(&actor.rules, &actor.skills);
        // The role is on the actor before the first estimate: a session that
        // starts as `plan` counts plan's role before any turn runs.
        actor.role = Some(status.selection.current());
        status.install_context(
            ContextTracker::new(limits, actor.context_breakdown()),
            actor.system_prompt.clone(),
        );
        let runtime_task = Arc::new(Mutex::new(Some(runtime::spawn_with_status(
            session_id,
            actor,
            inbox,
            sequencer.clone(),
            session_cancel.clone(),
            on_turn_end,
            status.clone(),
        ))));

        let handle = SessionHandle {
            inbox_tx,
            permissions,
            sequencer,
            session_cancel,
            runtime_task,
            actor_id: ROOT_ACTOR_ID.to_string(),
            lifecycle: Lifecycle::Active,
            actor_state: "idle".to_string(),
            workspace: workspace.clone(),
            selected: selected_model,
            status,
            offer,
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

/// The ids of the primary agents, comma separated, for an error text.
pub(super) fn primary_list(agents: &Agents) -> String {
    agents
        .primary()
        .map(|a| a.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

mod resolve;
pub(crate) use resolve::exact_variant;
pub use resolve::VariantChoice;
use resolve::VariantStart;
pub use resolve::CLEAR_WORD;

#[cfg(test)]
pub(crate) mod tests;
