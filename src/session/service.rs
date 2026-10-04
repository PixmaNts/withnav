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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::actor::{build_actor, build_child_actor, start_candidate};
use super::config::SessionConfig;
use super::hooks::{NoHooks, SessionHooks, StartedSession};
use super::message::{accept, MessageAccepted};
use super::sender::SessionSender;
use crate::agent::actor::AgentActor;
use crate::agent::context::{ContextLimits, ContextSnapshot, ContextTracker};
use crate::agent::definitions::{AgentDefinition, AgentId, AgentRoots, Agents};
use crate::agent::instructions::Instructions;
use crate::agent::permissions::Grants;
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

/// The file in the data dir that keeps the "allow folder" grants of every workspace.
const GRANTS_FILE: &str = "grants.json";

/// A session id on the wire and in the registry.
pub type SessionId = Ulid;

/// Debug-build seam for deterministic child admission integration tests.
#[cfg(debug_assertions)]
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct ChildPreparationBarrier {
    pub reached: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
}

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
    pub(crate) skills: Arc<Skills>,
    pub(crate) publications: Arc<Mutex<Vec<CancellationToken>>>,
    pub incarnation: Ulid,
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
    /// Owner of this session when it is a background child.
    pub parent_session_id: Option<SessionId>,
}

pub use crate::bus::event::{SkillEntry, SkillsBody};

/// The model selection of a live session, as the service sees it: the alias,
/// its saved resolution, the variant chosen on it and what it offers are one
/// value, so a reader never combines the alias of one selection with the
/// variant of another. A model switch installs all of it under one lock.
#[derive(Debug, Clone)]
pub struct SelectedModel {
    pub(crate) execution: Option<Arc<EffectiveExecution>>,
    pub alias: String,
    /// The saved resolution (identity, window, where the window came from)
    /// the session runs on.
    pub resolution: Option<Arc<crate::provider::catalog::ModelResolution>>,
    /// 1 for the first selection of the conversation, +1 for each switch.
    pub revision: u64,
    /// The variant selected on the alias (the one a change waiting for the
    /// running turn will apply included), with its override object.
    pub variant: runtime::SelectedVariant,
    /// What the alias offers as variants; replaced when an offer that was
    /// unknown at the start becomes known.
    pub(crate) offer: Arc<resolve::Offer>,
}

#[derive(Clone)]
pub(crate) struct EffectiveExecution {
    pub(crate) automatic_permission_review: bool,
    pub(crate) scratch_choice: Vec<PathBuf>,
    pub(crate) resolved: crate::provider::config::ResolvedModel,
    pub(crate) api_key: String,
}

type ChildPolicy = (
    BTreeMap<String, bool>,
    bool,
    bool,
    Option<String>,
    Vec<PathBuf>,
);
impl std::fmt::Debug for EffectiveExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EffectiveExecution { .. }")
    }
}

impl SelectedModel {
    /// The name of the selected variant.
    pub fn variant_name(&self) -> Option<String> {
        self.variant.as_ref().map(|(name, _)| name.clone())
    }
}

impl SessionHandle {
    /// The model selection, read once: alias, resolution and revision always
    /// belong together.
    pub(crate) fn selected(&self) -> SelectedModel {
        self.lock_selected().clone()
    }

    /// The alias the session talks to now.
    pub fn model(&self) -> String {
        self.selected().alias
    }

    /// Holds the selection while a reader looks at the other things that
    /// change with it (the context gauge). No `.await` while it is held.
    pub(crate) fn lock_selected(&self) -> MutexGuard<'_, SelectedModel> {
        match self.selected.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("incarnation", &self.incarnation)
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

struct ChildReservation {
    slots: Arc<Mutex<HashMap<SessionId, usize>>>,
    parent: SessionId,
    committed: bool,
}
struct Publication(CancellationToken);
impl Drop for Publication {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
// Child ids become visible before runtime registration. Publication owns
// teardown until this guard completes, so lifecycle callers cannot delete
// durable ownership underneath registration or failed-start cleanup.
pub(crate) struct ChildOperation {
    child: SessionId,
    operations: Arc<Mutex<HashMap<SessionId, CancellationToken>>>,
    done: CancellationToken,
}
impl Drop for ChildOperation {
    fn drop(&mut self) {
        self.done.cancel();
        self.operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.child);
    }
}
impl Drop for ChildReservation {
    fn drop(&mut self) {
        if !self.committed {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(count) = slots.get_mut(&self.parent) {
                *count = count.saturating_sub(1);
            }
        }
    }
}

struct PreparedSession {
    initial_override: Option<super::store::AgentOverride>,
    execution: Arc<EffectiveExecution>,
    session_id: SessionId,
    workspace: PathBuf,
    alias: String,
    profile: Option<String>,
    resumed: Option<ResumedSession>,
    actor: AgentActor,
    agents: Arc<Agents>,
    role: Arc<AgentDefinition>,
    snapshot: crate::provider::catalog::ModelResolution,
    selected: runtime::SelectedVariant,
    offered: resolve::Offer,
    limits: ContextLimits,
    session_cancel: CancellationToken,
    expired: Vec<(Ulid, String)>,
    save_late: bool,
    child_parent: Option<SessionId>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_status: Option<String>,
    #[serde(skip_serializing_if = "is_promptable")]
    pub promptable: bool,
}

fn is_promptable(value: &bool) -> bool {
    *value
}

/// The session service: the registry plus the immutable config. Cheap to
/// clone (all fields are `Arc`).
#[derive(Clone, Debug)]
pub struct Service {
    #[cfg(debug_assertions)]
    skill_loading_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    delivery_failure_countdown: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(debug_assertions)]
    pub(crate) shutdown_snapshot_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    pub(crate) watchdog_claim_write_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    pub(crate) watchdog_ownership_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    pub(crate) lifecycle_claim_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    pub(crate) watchdog_stop_failure: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(debug_assertions)]
    pub(crate) watchdog_teardown_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    pub(crate) watchdog_test: Arc<Mutex<Option<super::subagents::supervisor::TestWatchdog>>>,
    #[cfg(debug_assertions)]
    pub(crate) root_watchdog_test:
        Arc<Mutex<Option<super::subagents::root_watchdog::TestRootWatchdog>>>,
    _instance: Arc<super::instance::Instance>,
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
    pub(super) admission_gate: Arc<tokio::sync::Mutex<()>>,
    pub(super) subagent_slots: Arc<Mutex<HashMap<SessionId, usize>>>,
    pub(super) unpublished_children: Arc<Mutex<std::collections::HashSet<SessionId>>>,
    pub(crate) child_operations: Arc<Mutex<HashMap<SessionId, CancellationToken>>>,
    #[cfg(debug_assertions)]
    child_registration_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    child_cleanup_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    catalog: crate::provider::catalog::ModelCatalog,
    #[cfg(debug_assertions)]
    child_preparation_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    child_publication_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    resume_registration_barrier: Arc<Mutex<Option<ChildPreparationBarrier>>>,
    #[cfg(debug_assertions)]
    child_start_failure: Arc<Mutex<Option<bool>>>,
    /// Test seam: parks a command run between its work and the publication
    /// of its result, to interleave lifecycle changes deterministically.
    /// Test seam: parks a model switch after its store write and before its
    /// installation.
    #[cfg(test)]
    pub(super) install_barrier: Arc<Mutex<Option<Arc<super::run_command::Barrier>>>>,
    #[cfg(test)]
    pub(super) publish_barrier: Arc<Mutex<Option<Arc<super::run_command::Barrier>>>>,
}

impl Service {
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn fail_delivery_commit(&self, ordinal: usize) {
        self.delivery_failure_countdown
            .store(ordinal, std::sync::atomic::Ordering::Release);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_shutdown_snapshot(&self, barrier: ChildPreparationBarrier) {
        *self
            .shutdown_snapshot_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub async fn watchdog_natural_retained(&self, child: SessionId) {
        if let Some(handle) = self.handle(child) {
            handle.status.watchdog.natural_retained.notified().await;
        }
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_watchdog_claim_write(&self, barrier: ChildPreparationBarrier) {
        *self
            .watchdog_claim_write_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_watchdog_ownership(&self, barrier: ChildPreparationBarrier) {
        *self
            .watchdog_ownership_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_lifecycle_claim(&self, barrier: ChildPreparationBarrier) {
        *self
            .lifecycle_claim_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn fail_watchdog_stop_write(&self) {
        self.watchdog_stop_failure
            .store(true, std::sync::atomic::Ordering::Release);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_watchdog_teardown(&self, barrier: ChildPreparationBarrier) {
        *self
            .watchdog_teardown_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    /// Called under admission: teardown retains ownership until its final durable write.
    pub(crate) fn own_child_operation(&self, child: SessionId) -> ChildOperation {
        let done = CancellationToken::new();
        let previous = self
            .child_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(child, done.clone());
        debug_assert!(previous.is_none(), "child already has a lifecycle owner");
        ChildOperation {
            child,
            operations: self.child_operations.clone(),
            done,
        }
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_negative_result_eligibility(
        &self,
        id: SessionId,
        barrier: ChildPreparationBarrier,
    ) -> Result<(), ServerError> {
        let handle = self
            .handle(id)
            .ok_or_else(|| ServerError::SessionNotFound(id.to_string()))?;
        handle
            .status
            .pause_negative_eligibility(barrier.reached, barrier.release);
        Ok(())
    }
    pub(super) async fn wait_child_operation(&self, id: SessionId) {
        let pending = {
            let _admission = self.admission_gate.lock().await;
            self.child_operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .cloned()
        };
        // The publication worker never needs the lifecycle gate, and this
        // wait must not hold admission while the worker finishes its I/O.
        if let Some(pending) = pending {
            pending.cancelled().await;
        }
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_child_registration(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_registration_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_child_cleanup(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_cleanup_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn session_has_runtime(&self, id: SessionId) -> bool {
        self.handle(id).is_some()
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn fail_child_start(&self, publication: bool) {
        *self
            .child_start_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(publication);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_child_preparation(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_preparation_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_child_publication(&self, barrier: ChildPreparationBarrier) {
        *self
            .child_publication_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn session_is_cancelled(&self, id: SessionId) -> bool {
        self.handle(id)
            .is_some_and(|h| h.session_cancel.is_cancelled())
    }
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_resume_registration(&self, barrier: ChildPreparationBarrier) {
        *self
            .resume_registration_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(barrier);
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn reserved_child_count(&self, parent: SessionId) -> usize {
        self.subagent_slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&parent)
            .copied()
            .unwrap_or(0)
    }

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
        let mut history =
            super::store::HistoryStore::open(&config.history_db).map_err(ServerError::History)?;
        let instance = Arc::new(
            super::instance::Instance::acquire(&config.history_db).map_err(ServerError::History)?,
        );
        history.set_owner(instance.id);
        history
            .recover_children(&instance.directory)
            .map_err(ServerError::History)?;
        let seeded_slots = history
            .active_child_counts()
            .map_err(ServerError::History)?;
        let catalog = crate::provider::catalog::ModelCatalog::new(Some(config.data_dir.clone()));
        Ok(Self {
            #[cfg(debug_assertions)]
            delivery_failure_countdown: Arc::default(),
            #[cfg(debug_assertions)]
            shutdown_snapshot_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            watchdog_claim_write_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            watchdog_ownership_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            lifecycle_claim_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            watchdog_stop_failure: Arc::default(),
            #[cfg(debug_assertions)]
            watchdog_teardown_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            watchdog_test: Arc::default(),
            #[cfg(debug_assertions)]
            root_watchdog_test: Arc::default(),
            _instance: instance,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
            history: Arc::new(history),
            hooks,
            gate: Arc::new(tokio::sync::Mutex::new(())),
            admission_gate: Arc::new(tokio::sync::Mutex::new(())),
            subagent_slots: Arc::new(Mutex::new(seeded_slots)),
            unpublished_children: Arc::default(),
            child_operations: Arc::default(),
            #[cfg(debug_assertions)]
            child_registration_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            child_cleanup_barrier: Arc::default(),
            catalog,
            #[cfg(debug_assertions)]
            child_preparation_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            skill_loading_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            child_publication_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            resume_registration_barrier: Arc::default(),
            #[cfg(debug_assertions)]
            child_start_failure: Arc::default(),
            #[cfg(test)]
            install_barrier: Arc::default(),
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

    /// Debug-build observation seam for resume and child inheritance tests.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn driver_task_probe(&self, id: SessionId) -> Option<String> {
        let handle = self.handle(id)?;
        let task = handle
            .status
            .driver_task_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        task
    }

    /// The sessions the registry holds (live or stopping). Client
    /// connects/disconnects never touch the registry: only session creation
    /// inserts and each session occupies one entry, so a connect-then-drop
    /// loop cannot grow it.
    pub fn session_count(&self) -> usize {
        self.lock_sessions().len()
    }

    /// Builds and registers a fresh background session owned by `parent`.
    /// It shares discovery inputs and workspace, but not transcript or root
    /// tools; the caller commits its child link before starting a turn.
    pub async fn create_child_session(
        &self,
        parent: SessionId,
        agent: &str,
        prompt: &str,
    ) -> Result<SessionId, ServerError> {
        self.create_child_with_delivery(parent, agent, prompt, "next_turn")
            .await
    }

    pub async fn create_child_with_delivery(
        &self,
        parent: SessionId,
        agent: &str,
        prompt: &str,
        delivery: &str,
    ) -> Result<SessionId, ServerError> {
        if !matches!(delivery, "next_turn" | "steer" | "interrupt") {
            return Err(ServerError::MalformedBody(
                "invalid child delivery mode".into(),
            ));
        }
        let service = self.clone();
        let (agent, prompt) = (agent.to_string(), prompt.to_string());
        let delivery = delivery.to_string();
        tokio::spawn(async move {
            service
                .admit_child(parent, &agent, &prompt, &delivery)
                .await
        })
        .await
        .map_err(|e| ServerError::History(e.to_string()))?
    }

    async fn child_admission<'a>(
        &'a self,
        cancel: &CancellationToken,
    ) -> Result<tokio::sync::MutexGuard<'a, ()>, ServerError> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ServerError::SessionStopping),
            guard = self.admission_gate.lock() => Ok(guard),
        }
    }

    async fn admit_child(
        &self,
        parent: SessionId,
        agent: &str,
        prompt: &str,
        delivery: &str,
    ) -> Result<SessionId, ServerError> {
        let parent_handle = self.routable_handle(parent).await?;
        if parent_handle.parent_session_id.is_some() {
            return Err(ServerError::ChildReadOnly);
        }
        let parent_role = parent_handle.status.selection.current();
        let agents = self
            .load_agents(&parent_handle.workspace, parent, false)
            .await;
        let definition = agents
            .get(agent)
            .filter(|d| d.mode.is_subagent())
            .cloned()
            .ok_or_else(|| ServerError::UnknownAgent {
                agent: agent.to_string(),
                available: agents
                    .all()
                    .filter(|d| d.mode.is_subagent())
                    .map(|d| d.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;
        let settings = crate::settings::load(&self.config.config_dir)
            .map_err(|e| ServerError::SubagentSettings(e.to_string()))?;
        let watchdog_interval = settings
            .subagents_watchdog_interval_secs()
            .map_err(ServerError::SubagentSettings)?;
        let cap = settings
            .subagents_max_concurrent()
            .map_err(ServerError::SubagentSettings)?;
        let _spawn_lock = self.child_admission(&parent_handle.session_cancel).await?;
        let parent_handle = self.routable_handle(parent).await?;
        {
            let mut slots = self
                .subagent_slots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let current = slots.get(&parent).copied().unwrap_or(0);
            if current >= cap {
                return Err(ServerError::SubagentCapacity { capacity: cap });
            }
            slots.insert(parent, current + 1);
        }
        // Concurrent calls in this process reserve capacity atomically; the
        // persistent active count includes children from other processes.
        let inherited = parent_handle.selected();
        let parent_alias = inherited.alias.clone();
        let alias = definition.model.as_deref().unwrap_or(&parent_alias);
        let mut policy = BTreeMap::new();
        policy.insert("read".into(), true);
        for tool in ["grep", "glob", "fetch", "skill"] {
            policy.insert(tool.into(), true);
        }
        let mut allowed = BTreeMap::new();
        // A plan-owned child inherits plan's no-write capability ceiling even
        // when its definition is full-access. We use the stricter capability
        // ceiling rather than duplicating the advisory parent role guard: the
        // child has no write/edit/bash dispatch path, so it cannot launder
        // writes past the plan parent's restriction.
        for tool in [
            "read", "write", "edit", "bash", "grep", "glob", "fetch", "skill", "ask", "task",
        ] {
            let enabled = child_tool_allowed(tool, &definition, &parent_role);
            policy.insert(tool.to_string(), enabled);
            allowed.insert(tool.to_string(), enabled);
        }
        let incarnation = parent_handle.incarnation;
        drop(_spawn_lock);
        let reservation = ChildReservation {
            slots: self.subagent_slots.clone(),
            parent,
            committed: false,
        };
        let prepared = self
            .prepare_session_kind(
                &parent_handle.workspace,
                Some(alias),
                None,
                Some(agent),
                None,
                &parent_handle.workspace,
                &VariantChoice::Default,
                Some((
                    allowed,
                    definition.access == crate::agent::definitions::AgentAccess::ReadOnly
                        || parent_role.access == crate::agent::definitions::AgentAccess::ReadOnly,
                    parent_handle
                        .selected()
                        .execution
                        .as_ref()
                        .is_some_and(|execution| execution.automatic_permission_review),
                    self.history
                        .latest_driver_message(parent)
                        .await
                        .map_err(ServerError::History)?,
                    parent_handle
                        .selected()
                        .execution
                        .as_ref()
                        .map(|execution| execution.scratch_choice.clone())
                        .unwrap_or_default(),
                )),
                Some(parent),
                (self.assignments_configured()? || definition.model.is_none()).then_some(inherited),
            )
            .await;
        let prepared = prepared?;
        #[cfg(debug_assertions)]
        {
            let barrier = self
                .child_preparation_barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(barrier) = barrier {
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
        }
        let _admission = self.child_admission(&parent_handle.session_cancel).await?;
        let current = self.routable_handle(parent).await?;
        if current.incarnation != incarnation || current.session_cancel.is_cancelled() {
            return Err(ServerError::SessionStopping);
        }
        let publication = CancellationToken::new();
        {
            let mut pending = current
                .publications
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.retain(|token| !token.is_cancelled());
            pending.push(publication.clone());
        }
        let _publication = Publication(publication);
        let child = prepared.session_id;
        let done = CancellationToken::new();
        self.child_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(child, done.clone());
        let _child_operation = ChildOperation {
            child,
            operations: self.child_operations.clone(),
            done,
        };
        drop(_admission);
        #[cfg(debug_assertions)]
        {
            let barrier = self
                .child_publication_barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(barrier) = barrier {
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
        }
        if parent_handle.session_cancel.is_cancelled() {
            return Err(ServerError::SessionStopping);
        }
        let child = prepared.session_id;
        self.unpublished_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(child);
        if let Err(error) = self.store_prepared(&prepared).await {
            self.unpublished_children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&child);
            return Err(error);
        }
        let frozen = serde_json::to_string(&policy).unwrap_or_else(|_| "{}".into());
        let mut spawned_payload = serde_json::to_value(crate::bus::event::SubagentSpawned {
            parent_session_id: parent,
            child_session_id: child,
            agent: agent.to_string(),
            goal: crate::tools::bound_output(prompt, 1024).0,
            tool_call_id: "server".into(),
        })
        .map_err(|e| ServerError::History(e.to_string()))?;
        spawned_payload["delivery"] = serde_json::json!(delivery);
        let history = self.history.clone();
        let agent = agent.to_string();
        let goal = prompt.to_string();
        let cancel = parent_handle.session_cancel.clone();
        if let Err(error) = parent_handle
            .sequencer
            .commit_acked(
                Candidate::new(EventKind::SubagentSpawned, spawned_payload.clone()),
                Box::new(move |envelope| {
                    Box::pin(async move {
                        if cancel.is_cancelled() {
                            return Err("parent cancelled before child commit".into());
                        }
                        history
                            .create_child_link(child, agent, goal, frozen, cap, envelope)
                            .await
                    })
                }),
            )
            .await
        {
            self.history
                .delete(child)
                .await
                .map_err(ServerError::History)?;
            self.unpublished_children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&child);
            return Err(ServerError::History(error));
        }
        let mut reservation = reservation;
        reservation.committed = true;
        #[cfg(debug_assertions)]
        {
            let barrier = self
                .child_registration_barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(barrier) = barrier {
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
        }
        let start = async {
            #[cfg(debug_assertions)]
            if self
                .child_start_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                == Some(&true)
            {
                self.child_start_failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                return Err(ServerError::History(
                    "injected child publication failure".into(),
                ));
            }
            self.publish_session(prepared).await?;
            if parent_handle.session_cancel.is_cancelled() {
                return Err(ServerError::SessionStopping);
            }
            if let Some(handle) = self.handle(child) {
                handle
                    .sequencer
                    .publish_acked(Candidate::new(EventKind::SubagentSpawned, spawned_payload))
                    .await
                    .map_err(|e| ServerError::History(e.to_string()))?;
                #[cfg(debug_assertions)]
                if self
                    .child_start_failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    == Some(false)
                {
                    return Err(ServerError::History("injected child start failure".into()));
                }
                accept(&handle, Command::child_prompt(child, prompt.to_string())).await?;
            } else {
                return Err(ServerError::SessionStopping);
            }
            Ok(())
        }
        .await;
        if let Err(error) = start {
            if let Some(handle) = self.mark_stopping(child) {
                #[cfg(debug_assertions)]
                {
                    let barrier = self
                        .child_cleanup_barrier
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(barrier) = barrier {
                        barrier.reached.notify_one();
                        barrier.release.notified().await;
                    }
                }
                self.fence_and_remove(child, handle, "child start failed")
                    .await;
            }
            self.finish_child(
                parent,
                child,
                "failed",
                Some(error.to_string()),
                String::new(),
            )
            .await?;
            self.unpublished_children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&child);
            return Err(error);
        }
        self.unpublished_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&child);
        self.start_child_watchdog(child, watchdog_interval);
        Ok(child)
    }

    pub async fn child_metadata(
        &self,
        id: SessionId,
    ) -> Result<Option<super::store::ChildLink>, ServerError> {
        self.history
            .child_link(id)
            .await
            .map_err(ServerError::History)
    }

    pub(super) async fn finish_child(
        &self,
        parent: SessionId,
        child: SessionId,
        status: &str,
        reason: Option<String>,
        result: String,
    ) -> Result<(), ServerError> {
        if self
            .history
            .finish_child_link(child, status.to_string(), reason, result)
            .await
            .map_err(ServerError::History)?
            .is_some()
        {
            self.release_child_slot(parent);
        }
        self.drain_completions(parent).await;
        if let Some(owner) = self.handle(parent) {
            if owner.lifecycle == Lifecycle::Active {
                let notices = self
                    .history
                    .unconsumed_completions(parent)
                    .await
                    .map_err(ServerError::History)?;
                let mode = notices
                    .iter()
                    .map(|(_, p, _)| p["delivery"].as_str().unwrap_or("next_turn"))
                    .find(|m| *m == "interrupt")
                    .or_else(|| {
                        notices
                            .iter()
                            .map(|(_, p, _)| p["delivery"].as_str().unwrap_or("next_turn"))
                            .find(|m| *m == "steer")
                    })
                    .unwrap_or("next_turn");
                let notice = notices
                    .iter()
                    .find(|(_, p, _)| p["delivery"] == mode)
                    .map(|(id, _, _)| *id)
                    .unwrap_or(child);
                owner.status.wake_delivery_mode(mode, notice);
            }
        }
        Ok(())
    }

    pub(crate) async fn drain_completions(&self, parent: SessionId) {
        let Some(owner) = self.handle(parent) else {
            return;
        };
        let pending = match self.history.pending_completions(parent).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::error!(%error, "completion outbox read failed");
                return;
            }
        };
        for (child, payload) in pending {
            let kind = if payload["notice_kind"] == "watchdog" {
                EventKind::SubagentWatchdog
            } else {
                EventKind::SubagentDone
            };
            let history = self.history.clone();
            if let Err(error) = owner
                .sequencer
                .commit_acked(
                    Candidate::new(kind, payload),
                    Box::new(move |envelope| {
                        Box::pin(async move { history.deliver_completion(child, envelope).await })
                    }),
                )
                .await
            {
                if error != "completion already delivered" {
                    tracing::debug!(%error, "completion retained for parent resume");
                    break;
                }
            }
        }
    }

    fn release_child_slot(&self, parent: SessionId) {
        let mut slots = self
            .subagent_slots
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = slots.get_mut(&parent) {
            *count = count.saturating_sub(1);
        }
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
    pub async fn send(&self, mut command: Command) -> Result<MessageAccepted, ServerError> {
        let handle = self.routable_handle(command.session_id).await?;
        if handle.parent_session_id.is_some() {
            return Err(ServerError::ChildReadOnly);
        }
        command.validate_content()?;
        if command
            .content
            .iter()
            .any(|block| matches!(block, crate::bus::command::ContentBlock::LoadedSkill { .. }))
        {
            return Err(ServerError::MalformedBody(
                "loaded skill content is server-only".into(),
            ));
        }
        let names: Vec<_> = command
            .content
            .iter()
            .filter_map(|block| match block {
                crate::bus::command::ContentBlock::Skill { name } => Some(name.clone()),
                _ => None,
            })
            .collect();
        if names.len() > 1 {
            return Err(ServerError::MalformedBody(
                "select one skill per message".into(),
            ));
        }
        if let Some(name) = names.first() {
            if !command.from_driver()
                || handle.status.selection.current().tools.get("skill") == Some(&false)
            {
                return Err(ServerError::MalformedBody(
                    "this agent cannot use the skill tool".into(),
                ));
            }
            #[cfg(debug_assertions)]
            {
                let barrier = self
                    .skill_loading_barrier
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(barrier) = barrier {
                    barrier.reached.notify_one();
                    barrier.release.notified().await;
                }
            }
            let result = handle.skills.load_listed(name).await;
            if result.status != crate::tools::ToolStatus::Ok {
                return Err(ServerError::MalformedBody(
                    result
                        .error
                        .map(|error| format!("{}: {}", error.code, error.message))
                        .unwrap_or(result.output),
                ));
            }
            for block in &mut command.content {
                if matches!(block, crate::bus::command::ContentBlock::Skill { .. }) {
                    *block = crate::bus::command::ContentBlock::LoadedSkill {
                        name: name.clone(),
                        content: result.output.clone(),
                    };
                }
            }
        }
        accept(&handle, command).await
    }

    /// Debug-build seam to hold loading across an agent switch in deterministic tests.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn pause_skill_loading(&self, barrier: ChildPreparationBarrier) {
        *self
            .skill_loading_barrier
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(barrier);
    }

    /// Lists only the session snapshot's catalog-visible skills and current tool policy.
    pub async fn skills(&self, id: SessionId) -> Result<SkillsBody, ServerError> {
        let handle = self.routable_handle(id).await?;
        let allowed = handle.status.selection.current().tools.get("skill") != Some(&false);
        Ok(SkillsBody {
            skills: if allowed {
                handle
                    .skills
                    .listed()
                    .into_iter()
                    .map(|skill| SkillEntry {
                        name: skill.name.clone(),
                        description: skill
                            .description
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" "),
                    })
                    .collect()
            } else {
                vec![]
            },
            disabled_reason: (!allowed).then(|| "this agent cannot use the skill tool".into()),
        })
    }

    pub async fn continue_child(
        &self,
        parent: SessionId,
        child: SessionId,
    ) -> Result<(), ServerError> {
        self.continue_child_with_token(parent, child, None).await
    }

    pub async fn continue_child_with_token(
        &self,
        parent: SessionId,
        child: SessionId,
        token: Option<runtime::PauseToken>,
    ) -> Result<(), ServerError> {
        let owner = self.routable_handle(parent).await?;
        if owner.parent_session_id.is_some() {
            return Err(ServerError::ChildReadOnly);
        }
        let handle = self.routable_handle(child).await?;
        if handle.parent_session_id != Some(parent)
            && !(parent == child && handle.parent_session_id.is_none())
        {
            return Err(ServerError::ChildReadOnly);
        }
        if handle.status.pending_permission().is_some() || handle.status.selection.ask_pending() {
            return Err(ServerError::ChildNotParked);
        }
        if handle
            .status
            .awaiting_fresh_driver
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ServerError::ChildNotParked);
        }
        if parent == child && handle.status.watchdog.root_hold().is_some() {
            let token = token.ok_or(ServerError::ChildNotParked)?;
            if !handle.status.watchdog.resolve_root(token, || {
                runtime::publish_pause_resolution(&handle.sequencer, token, "continued");
                handle.status.wake_continue();
            }) {
                return Err(ServerError::ChildNotParked);
            }
            return Ok(());
        }
        if token.is_some() {
            return Err(ServerError::ChildNotParked);
        }
        if !handle.status.watchdog.resume() && !handle.status.continue_budget() {
            return Err(ServerError::ChildNotParked);
        }
        Ok(())
    }

    pub async fn steer_child(
        &self,
        parent: SessionId,
        child: SessionId,
        message: &str,
        interrupt: bool,
    ) -> Result<(), ServerError> {
        let owner = self.routable_handle(parent).await?;
        if owner.parent_session_id.is_some() {
            return Err(ServerError::ChildReadOnly);
        }
        let handle = self.routable_handle(child).await?;
        if handle.parent_session_id != Some(parent) {
            return Err(ServerError::ChildReadOnly);
        }
        let link = self
            .child_metadata(child)
            .await?
            .ok_or_else(|| ServerError::SessionNotFound(child.to_string()))?;
        if !matches!(link.status.as_str(), "running" | "starting") {
            return Err(ServerError::ChildNotParked);
        }
        if message.trim().is_empty() || message.len() > 16 * 1024 {
            return Err(ServerError::MalformedBody(
                "steering message must be nonempty and at most 16 KiB".into(),
            ));
        }
        if !handle.status.steer(message.to_string(), interrupt) {
            return Err(ServerError::ChildNotParked);
        }
        Ok(())
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
                    if request.child_session_id.is_none() {
                        open.push((request.request_id, request.tool_call_id));
                    }
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
        if handle.parent_session_id.is_some() {
            return Err(ServerError::ChildReadOnly);
        }
        // Claimed at admission: a second answer for the same request is
        let owned = {
            self.lock_sessions()
                .values()
                .find(|child| {
                    child.parent_session_id == Some(session_id)
                        && child.lifecycle == Lifecycle::Active
                        && child.status.pending_permission() == Some(request_id)
                })
                .cloned()
        };
        let handle = if handle.status.pending_permission() == Some(request_id) {
            handle
        } else if let Some(child) = owned {
            child
        } else {
            let foreign = self.lock_sessions().values().any(|child| {
                child.parent_session_id.is_some()
                    && child.status.pending_permission() == Some(request_id)
            });
            return Err(if foreign {
                ServerError::ChildReadOnly
            } else {
                ServerError::PermissionStale
            });
        };
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
            if self
                .history
                .child_link(id)
                .await
                .map_err(ServerError::History)?
                .is_some()
            {
                return Err(ServerError::ChildReadOnly);
            }
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
        // Read under the selection lock: a switch installs the gauge together
        // with the selection.
        let _selection = handle.lock_selected();
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
            let mut summary = self.stored_summary(id).await?;
            if let Some(link) = self
                .history
                .child_link(id)
                .await
                .map_err(ServerError::History)?
            {
                summary.parent_session_id = Some(link.parent_id.to_string());
                summary.child_status = Some(link.status);
                summary.promptable = false;
            }
            return Ok(summary);
        };
        let current_seq = handle
            .sequencer
            .current_seq()
            .await
            .map(|next| next.saturating_sub(1))
            .unwrap_or(0);
        let selected = handle.selected();
        Ok(SessionSummary {
            session_id: id.to_string(),
            actor_state: handle.actor_state.clone(),
            workspace: handle.workspace.display().to_string(),
            variant: selected.variant_name(),
            model: selected.alias,
            agent: handle.status.selection.current().id.to_string(),
            current_seq,
            parent_session_id: handle.parent_session_id.map(|id| id.to_string()),
            child_status: if handle.parent_session_id.is_some() {
                self.history
                    .child_link(id)
                    .await
                    .map_err(ServerError::History)?
                    .map(|link| link.status)
            } else {
                None
            },
            promptable: handle.parent_session_id.is_none(),
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
            parent_session_id: None,
            child_status: None,
            promptable: true,
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
        self.spawn_session_kind(
            workspace,
            model,
            profile,
            agent,
            resumed,
            resume_workspace,
            variant,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_session_kind(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        agent: Option<&str>,
        resumed: Option<ResumedSession>,
        resume_workspace: &Path,
        variant: &VariantChoice,
        child_policy: Option<ChildPolicy>,
        child_parent: Option<SessionId>,
        inherited: Option<SelectedModel>,
    ) -> Result<SessionId, ServerError> {
        let prepared = self
            .prepare_session_kind(
                workspace,
                model,
                profile,
                agent,
                resumed,
                resume_workspace,
                variant,
                child_policy,
                child_parent,
                inherited,
            )
            .await?;
        self.store_prepared(&prepared).await?;
        self.publish_session(prepared).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_session_kind(
        &self,
        workspace: &Path,
        model: Option<&str>,
        profile: Option<&str>,
        agent: Option<&str>,
        resumed: Option<ResumedSession>,
        resume_workspace: &Path,
        variant: &VariantChoice,
        child_policy: Option<ChildPolicy>,
        child_parent: Option<SessionId>,
        inherited: Option<SelectedModel>,
    ) -> Result<PreparedSession, ServerError> {
        // Resume keeps the stored model/profile/workspace (the stored row is
        // the authority); a fresh session honors the request's.
        let session_id = match &resumed {
            Some(loaded) => loaded.meta.id,
            // NB: NOT Ulid::default() — that is the nil ULID; every fresh
            // session must mint its own id.
            None => Ulid::new(),
        };
        let (workspace, mut alias, profile) = match &resumed {
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
        let configured = self.assignments_configured()?;
        let settings = crate::settings::load(&self.config.config_dir)
            .map_err(|e| ServerError::History(e.to_string()))?;
        let wanted = if configured && agent.is_none() && resumed.is_none() && child_parent.is_none()
        {
            Some(settings.default_agent().map_err(ServerError::History)?)
        } else {
            agent
        };
        let role = if child_parent.is_some() {
            let wanted = agent.unwrap_or_default();
            agents
                .get(wanted)
                .filter(|definition| definition.mode.is_subagent())
                .cloned()
                .ok_or_else(|| ServerError::UnknownAgent {
                    agent: wanted.to_string(),
                    available: agents
                        .all()
                        .filter(|definition| definition.mode.is_subagent())
                        .map(|definition| definition.id.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                })?
        } else {
            Self::pick_agent(&agents, session_id, wanted, resumed.as_ref())?
        };
        let mut inherited = inherited;
        let mut assignment_variant = variant.clone();
        let initial_override = if configured {
            if let Some(loaded) = &resumed {
                if let Some(saved) = loaded.meta.agent_overrides.get(role.id.as_str()) {
                    if saved
                        .model
                        .as_ref()
                        .is_some_and(|model| model != &loaded.meta.model)
                    {
                        return Err(ServerError::History(
                            "saved agent override disagrees with effective model".into(),
                        ));
                    }
                }
            }
            let assignment = self.assigned_agent_model(
                &workspace,
                &agents,
                role.id.as_str(),
                child_parent.is_some(),
            )?;
            if resumed.is_none() {
                if child_parent.is_some() {
                    if let Some((name, choice)) = assignment {
                        alias = name;
                        assignment_variant = choice;
                        inherited = None;
                    }
                } else if let Some(explicit) = model {
                    alias = explicit.into();
                } else if let Some((name, choice)) = assignment {
                    alias = name;
                    if *variant == VariantChoice::Default {
                        assignment_variant = choice;
                    }
                }
            }
            (resumed.is_none()
                && child_parent.is_none()
                && (model.is_some() || *variant != VariantChoice::Default))
                .then(|| super::store::AgentOverride {
                    model: model.map(str::to_owned),
                    variant: variant.clone().into(),
                })
        } else {
            // Internal legacy creation path until the final catalog contract cutover.
            None
        };
        let (resolved, api_key, snapshot, save_late, models_file) =
            if let Some(selection) = &inherited {
                let execution = selection
                    .execution
                    .as_ref()
                    .expect("live parent has execution configuration");
                let resolved = execution.resolved.clone();
                let models_file = self
                    .config
                    .models_path
                    .clone()
                    .unwrap_or_else(|| resume_workspace.join("models.json"));
                let snapshot = selection
                    .resolution
                    .as_ref()
                    .expect("live parent has a resolution")
                    .as_ref()
                    .clone();
                let api_key = execution.api_key.clone();
                (resolved, api_key, snapshot, false, models_file)
            } else {
                self.resolve_for_spawn(
                    session_id,
                    if configured {
                        &workspace
                    } else {
                        resume_workspace
                    },
                    &alias,
                    resumed.as_ref(),
                )
                .await?
            };
        // A resume keeps the variant the row saved; a fresh session takes the
        // request's choice. Either way it is checked against what the alias
        // offers now, before anything is written.
        let (selected, offered) = if let Some(selection) = &inherited {
            (selection.variant.clone(), (*selection.offer).clone())
        } else {
            self.select_variant(
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
                    None => VariantStart::Choice(&assignment_variant),
                },
            )
            .await?
        };
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
        let execution = Arc::new(EffectiveExecution {
            scratch_choice: match &child_policy {
                Some((_, _, _, _, choice)) => choice.clone(),
                None => match crate::settings::load(&self.config.config_dir)
                    .map_err(|e| e.to_string())
                    .and_then(|settings| settings.permissions_scratch_dirs())
                {
                    Ok(paths) => paths,
                    Err(error) => {
                        crate::agent::scratch::warn_once(&error);
                        Vec::new()
                    }
                },
            },
            automatic_permission_review: match &child_policy {
                Some((_, _, enabled, _, _)) => *enabled,
                None => match crate::settings::load(&self.config.config_dir)
                    .map_err(|e| e.to_string())
                    .and_then(|settings| settings.permissions_automatic_review())
                {
                    Ok(enabled) => enabled,
                    Err(error) => {
                        tracing::warn!("automatic permission review disabled: {error}");
                        false
                    }
                },
            },
            resolved: resolved.clone(),
            api_key: api_key.clone(),
        });
        let root_tools_enabled = child_policy.is_none() && role.mode.is_primary();
        let mut actor = match &child_policy {
            Some((allowed, read_only, _, _, _)) => build_child_actor(
                &workspace,
                &self.config.config_dir,
                resolved,
                api_key,
                session_cancel.child_token(),
                allowed.clone(),
                *read_only,
            ),
            None => build_actor(
                &workspace,
                &self.config.config_dir,
                resolved,
                api_key,
                session_cancel.child_token(),
            ),
        };
        // "Allow folder" answers are kept per workspace, across sessions. A
        // subagent keeps its own in memory, as its parent's policy decides.
        if child_policy.is_none() {
            actor.grants = Grants::persisted(self.config.data_dir.join(GRANTS_FILE), &workspace);
        }
        if root_tools_enabled && role.mode.is_primary() {
            actor.registry.register(Box::new(
                crate::tools::decide_subagent::DecideSubagent::new(self.clone(), session_id),
            ));
            actor
                .registry
                .register(Box::new(crate::tools::steer_subagent::SteerSubagent::new(
                    self.clone(),
                    session_id,
                )));
            actor
                .registry
                .register(Box::new(crate::tools::task::TaskTool::new(
                    self.clone(),
                    session_id,
                )));
        }
        if let Some(loaded) = &resumed {
            actor.transcript = loaded.transcript.clone();
            actor.compaction = loaded.compaction.clone();
            // Reasoning of the models the conversation moved away from stays
            // out of the projection, as before the stop.
            actor.reasoning_floor = loaded
                .meta
                .reasoning_floor
                .map_or(0, |floor| usize::try_from(floor).unwrap_or(usize::MAX))
                .min(actor.transcript.len());
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
        actor.permission_judge = super::guard::permission_judge(
            self.config.models_path.as_deref(),
            &workspace,
            &self.config.config_dir,
        );
        actor.automatic_permission_review = execution.automatic_permission_review;
        actor.protected_config_spelling = Some(self.config.config_dir.clone());
        actor.protected_config_root =
            crate::agent::permissions::resolve_protected_root(&self.config.config_dir).ok();
        actor.protected_config_unresolved = actor.protected_config_root.is_none();
        if actor.protected_config_unresolved {
            crate::agent::scratch::warn_once(
                "config root unresolved; native writes require driver approval",
            );
        }
        for path in &execution.scratch_choice {
            if actor
                .protected_config_root
                .as_ref()
                .is_some_and(|root| root.starts_with(path) || path.starts_with(root))
            {
                crate::agent::scratch::warn_once("scratch overlaps protected config directory");
                continue;
            }
            match crate::agent::scratch::Identity::capture(path) {
                Ok(identity) => {
                    actor
                        .tool_context
                        .writable_roots
                        .push(identity.path.clone());
                    actor.scratch_roots.push(identity);
                }
                Err(error) => crate::agent::scratch::warn_once(&error),
            }
        }
        if let Some((_, _, _, driver_task, _)) = &child_policy {
            actor.driver_request_inherited = true;
            actor.driver_task = driver_task.clone();
        }
        if let Ok((models, _)) = crate::provider::config::load_models(
            self.config.models_path.as_deref(),
            &workspace,
            &self.config.config_dir,
        ) {
            let configured_layer = settings
                .layer_assignment(crate::provider::assignments::Layer::Metacognition)
                .map_err(ServerError::History)?;
            let reviewer_present = if configured_layer.is_some() {
                // Layer consumer wiring will reuse its shared assigned helper here.
                matches!(
                    crate::provider::assignments::AssignmentResolver::new(&models, &settings)
                        .layer_assignment(crate::provider::assignments::Layer::Metacognition),
                    Ok(crate::provider::assignments::LayerAssignment::Enabled(_))
                )
            } else {
                models.resolve("metacognition").is_ok()
            };
            actor.permission_reviewer_unavailable = if reviewer_present {
                "no key"
            } else {
                "no alias"
            };
            actor.permission_reviewer = crate::navigator::bootstrap::LazyCommenter::new(
                &models,
                "metacognition",
                &self.config.config_dir,
                &self.catalog,
                Arc::new(crate::provider::openai::ClientCache::new()),
                self.config.data_dir.join("navai.jsonl"),
            )
            .ok()
            .map(|reviewer| {
                Arc::new(reviewer) as Arc<dyn crate::agent::permission_gate::PermissionReviewer>
            });
        }
        self.hooks.configure_actor(profile, &mut actor);
        if child_policy.is_some() {
            actor.max_steps = crate::agent::run_loop::max_steps();
        }
        if let Some((allowed, _, _, _, _)) = &child_policy {
            actor
                .registry
                .retain(|name| allowed.get(name).copied().unwrap_or(false));
        }
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

        Ok(PreparedSession {
            initial_override,
            execution,
            session_id,
            workspace,
            alias,
            profile: profile.map(str::to_string),
            resumed,
            actor,
            agents,
            role,
            snapshot,
            selected,
            offered,
            limits,
            session_cancel,
            expired,
            save_late,
            child_parent,
        })
    }

    async fn store_prepared(&self, prepared: &PreparedSession) -> Result<(), ServerError> {
        self.history
            .record_session_resolved(
                prepared.session_id,
                &prepared.workspace.display().to_string(),
                &prepared.alias,
                prepared.profile.as_deref(),
                Some(&prepared.snapshot),
                prepared.selected.as_ref(),
                (prepared.resumed.is_none() && !prepared.role.id.is_build())
                    .then(|| prepared.role.id.as_str()),
            )
            .await
            .map_err(ServerError::History)?;
        if let Some(value) = &prepared.initial_override {
            if let Err(error) = self
                .history
                .update_agent_override(
                    prepared.session_id,
                    prepared.role.id.as_str(),
                    value.clone(),
                )
                .await
            {
                self.history
                    .delete(prepared.session_id)
                    .await
                    .map_err(ServerError::History)?;
                return Err(ServerError::History(error));
            }
        }
        if prepared.save_late {
            self.save_legacy_snapshot(prepared.session_id, &prepared.snapshot)
                .await?;
        }
        Ok(())
    }

    async fn publish_session(&self, prepared: PreparedSession) -> Result<SessionId, ServerError> {
        let PreparedSession {
            initial_override: _,
            execution,
            session_id,
            workspace,
            alias,
            profile,
            resumed,
            mut actor,
            agents,
            role,
            snapshot,
            selected,
            offered,
            limits,
            session_cancel,
            expired,
            save_late: _,
            child_parent,
        } = prepared;
        let profile = profile.as_deref();
        let next_seq = resumed
            .as_ref()
            .map(|loaded| loaded.meta.last_seq + 1)
            .unwrap_or(0);
        #[cfg(debug_assertions)]
        if resumed.is_some() {
            let barrier = self
                .resume_registration_barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(barrier) = barrier {
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
        }
        // The in-memory ring starts empty on resume: subscribers replay from
        // the store, and the ring is only a bounded fallback cache.
        let bus = SessionBus::default();
        let is_resume = resumed.is_some();

        // History hooks: a persisting emitter + a turn-end transcript
        // snapshot.
        let history = self.history.clone();
        if child_parent.is_some() {
            self.unpublished_children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id);
        }
        let mut emitter = super::store::PersistingEmitter::new(session_id, bus, (*history).clone());
        if let Some((parent_id, parent)) =
            child_parent.and_then(|id| self.handle(id).map(|h| (id, h)))
        {
            emitter = emitter.with_permission_mirror(session_id, parent_id, parent.sequencer);
        }
        let root_watchdog = if child_parent.is_none() {
            Some(self.prepare_root_watchdog(&workspace)?)
        } else {
            None
        };
        let sequencer = Sequencer::spawn(session_id, emitter, next_seq);
        let mut awaiting_fresh_driver = false;
        if is_resume && child_parent.is_none() {
            let events = self
                .history
                .watchdog_events(session_id)
                .await
                .map_err(ServerError::History)?;
            let mut pending = None;
            for event in events {
                if event.kind == "agent.yield" && event.payload["kind"] == "watchdog" {
                    pending = serde_json::from_value::<runtime::PauseToken>(
                        event.payload["details"]["pause_token"].clone(),
                    )
                    .ok();
                }
                if event.kind == "command.result"
                    && event.payload["name"] == "watchdog"
                    && event.payload["data"]["resolution"].is_string()
                {
                    pending = None;
                    awaiting_fresh_driver = matches!(
                        event.payload["data"]["resolution"].as_str(),
                        Some("expired" | "superseded")
                    );
                }
                if event.kind == "command.result"
                    && event.payload["name"] == "watchdog"
                    && event.payload["data"]["driver_committed"] == true
                {
                    awaiting_fresh_driver = false;
                }
            }
            if let Some(token) = pending {
                awaiting_fresh_driver = true;
                runtime::publish_pause_resolution(&sequencer, token, "expired");
            }
        }
        if root_watchdog
            .as_ref()
            .is_some_and(|watchdog| watchdog.unavailable())
            && actor.max_steps == usize::MAX
        {
            actor.max_steps = crate::agent::run_loop::WATCHDOG_FALLBACK_STEPS;
        }

        for (request_id, tool_call_id) in expired {
            let payload = serde_json::to_value(crate::bus::event::PermissionResolved {
                child_session_id: None,
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
        let completion: Option<crate::bus::runtime::CompletionHook> =
            if let Some(parent) = child_parent {
                let service = self.clone();
                Some(Box::new(move |message, outcome| {
                    use crate::bus::runtime::CompletionOutcome;
                    let status = match outcome {
                        CompletionOutcome::Done => "done",
                        CompletionOutcome::Failed => "failed",
                        CompletionOutcome::Stopped => "stopped",
                        CompletionOutcome::Parked => return,
                    };
                    let service = service.clone();
                    tokio::spawn(async move {
                        let Some(handle) = service.handle(session_id) else {
                            return;
                        };
                        let Some((message, outcome)) =
                            handle.status.watchdog.natural_outcome((message, outcome))
                        else {
                            return;
                        };
                        handle.status.watchdog.supervisor_cancel.cancel();
                        let result = message.text.clone();
                        let (text, _) = crate::tools::bound_output(&result, 16 * 1024);
                        let _ = service
                            .finish_child(
                                parent,
                                session_id,
                                status,
                                (outcome != CompletionOutcome::Done).then(|| result.clone()),
                                text,
                            )
                            .await;
                    });
                }))
            } else {
                None
            };
        let status = Arc::new(runtime::RuntimeStatus::with_selection(AgentSelection::new(
            agents, role,
        )));
        status.init_variant(selected.clone());
        if child_parent.is_none() {
            let history = self.history.clone();
            let driver_sequencer = sequencer.clone();
            status.install_driver_commit(Arc::new(move |transcript| {
                let (history, sequencer) = (history.clone(), driver_sequencer.clone());
                Box::pin(async move {
                    history.flush().await?;
                    let candidate = Candidate::new(EventKind::CommandResult, serde_json::json!({
                        "name":"watchdog","ok":true,"operation_id":Ulid::new(),
                        "data":{"driver_committed":true,"message":"Fresh driver instruction checkpointed"}
                    }));
                    sequencer.commit_acked(candidate, Box::new(move |envelope| Box::pin(async move {
                        history.consume_completions_committed(session_id,vec![],transcript,Some(envelope)).await
                    }))).await.map(|_| ()).map_err(|error| error.to_string())
                })
            }));
        }
        status
            .awaiting_fresh_driver
            .store(awaiting_fresh_driver, std::sync::atomic::Ordering::Release);
        status.watchdog_fallback_notice.store(
            root_watchdog
                .as_ref()
                .is_some_and(|watchdog| watchdog.unavailable())
                && actor.max_steps == crate::agent::run_loop::WATCHDOG_FALLBACK_STEPS,
            std::sync::atomic::Ordering::Release,
        );
        if child_parent.is_none() {
            let pending_history = self.history.clone();
            let eligible_history = self.history.clone();
            status.install_boundary_eligible(Arc::new(move || {
                let history = eligible_history.clone();
                Box::pin(async move {
                    Ok(history
                        .unconsumed_completions(session_id)
                        .await?
                        .iter()
                        .any(|(_, payload, _)| payload["delivery"] == "steer"))
                })
            }));
            status.install_pending_notice(Arc::new(move |id| {
                let history = pending_history.clone();
                Box::pin(async move {
                    Ok(history
                        .unconsumed_completions(session_id)
                        .await?
                        .iter()
                        .any(|(child, _, _)| *child == id))
                })
            }));
            let history = self.history.clone();
            let delivery_sequencer = sequencer.clone();
            #[cfg(debug_assertions)]
            let failure_countdown = self.delivery_failure_countdown.clone();
            status.install_delivery(Arc::new(move |mut transcript, mid_turn| {
                let history = history.clone();
                let sequencer = delivery_sequencer.clone();
                #[cfg(debug_assertions)]
                let failure_countdown = failure_countdown.clone();
                Box::pin(async move {
                    history.flush().await?;
                    let notices = history.unconsumed_completions(session_id).await?;
                    if notices.is_empty() {
                        return Ok(None);
                    }
                    let mut consumed = false;
                    for (child, payload, result) in notices {
                        if mid_turn && payload["delivery"] != "steer" { continue; }
                        let mut result = crate::tools::bound_output(&result, 15 * 1024).0;
                        let mut text = serde_json::json!({
                            "untrusted_subagent_output": true,
                            "child_session_id": payload.get("child_session_id").cloned().unwrap_or(serde_json::json!(child)),
                            "agent": payload["agent"],
                            "outcome": payload["outcome"],
                            "result": result,
                            "reason": payload.get("reason"),
                        })
                        .to_string();
                        while text.len() > 16 * 1024 {
                            result = crate::tools::bound_output(&result, result.len() / 2).0;
                            text = serde_json::json!({"untrusted_subagent_output":true,
                                "child_session_id":payload.get("child_session_id").cloned().unwrap_or(serde_json::json!(child)),
                                "agent":payload["agent"],"outcome":payload["outcome"],"result":result,
                                "reason":payload.get("reason")}).to_string();
                        }
                        let event = super::subagents::live_result::candidate(child, &payload, &text);
                        transcript.push(ChatMessage::User { provenance: crate::provider::openai::UserProvenance::Subagent, content: text });
                        let committed_transcript = transcript.clone();
                        let committed_history = history.clone();
                        #[cfg(debug_assertions)]
                        let failure_countdown = failure_countdown.clone();
                        let result = sequencer.commit_acked(event, Box::new(move |envelope| Box::pin(async move {
                            #[cfg(debug_assertions)]
                            {
                                let previous = failure_countdown.fetch_update(std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire, |n| {
                                    if n == usize::MAX { Some(n) } else { n.checked_sub(1) }
                                });
                                if previous == Ok(1) || previous == Ok(usize::MAX) {
                                    failure_countdown.store(usize::MAX, std::sync::atomic::Ordering::Release);
                                    return Err("injected delivery commit failure".into());
                                }
                            }
                            committed_history.consume_completions_committed(session_id, vec![child], committed_transcript, Some(envelope)).await
                        }))).await;
                        if let Err(message) = result {
                            transcript.pop();
                            return Err(crate::agent::boundary::BoundaryError { message, committed: consumed.then_some(transcript) });
                        }
                        consumed = true;
                    }
                    if !consumed { return Ok(None); }
                    Ok(Some(transcript))
                })
            }));
        }
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
            execution: Some(execution),
            alias: alias.clone(),
            resolution: Some(Arc::new(snapshot.clone())),
            revision,
            variant: selected,
            offer: Arc::new(offered),
        }));
        status.install_model_switcher(self.model_switcher(
            session_id,
            workspace.clone(),
            selected_model.clone(),
        ));
        let limits = limits.with_sources(&actor.rules, &actor.skills);
        // The role is on the actor before the first estimate: a session that
        // starts as `plan` counts plan's role before any turn runs.
        actor.role = Some(status.selection.current());
        status.install_context(
            ContextTracker::new(limits, actor.context_breakdown()),
            actor.system_prompt.clone(),
        );
        let skills = actor.skills.clone();
        let runtime_task = Arc::new(Mutex::new(Some(runtime::spawn_with_completion(
            session_id,
            actor,
            inbox,
            sequencer.clone(),
            session_cancel.clone(),
            on_turn_end,
            status.clone(),
            completion,
        ))));

        let incarnation = Ulid::new();
        status.watchdog.install_incarnation(incarnation);
        let handle = SessionHandle {
            skills,
            publications: Arc::default(),
            incarnation,
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
            parent_session_id: child_parent,
        };
        let sender = SessionSender::new(session_id, handle.clone());
        let cancel = handle.session_cancel.clone();
        {
            let _admission = if child_parent.is_none() {
                Some(self.admission_gate.lock().await)
            } else {
                None
            };
            self.lock_sessions().insert(session_id, handle);
        }
        if child_parent.is_none() {
            if let Some(owner) = self.handle(session_id) {
                self.history
                    .drain_permission_mirrors(session_id, owner.sequencer)
                    .await
                    .map_err(ServerError::History)?;
            }
            self.drain_completions(session_id).await;
            if let Some(owner) = self.handle(session_id) {
                owner.status.wake_delivery();
            }
        }
        if let Some(watchdog) = root_watchdog {
            if let Some(owner) = self.handle(session_id) {
                let mut task = owner
                    .status
                    .root_supervisor
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !owner.status.watchdog.supervisor_cancel.is_cancelled() {
                    *task = watchdog.start(
                        owner.status.clone(),
                        owner.sequencer.clone(),
                        owner.session_cancel.clone(),
                    );
                }
            }
        }
        // The host learns of the session once it is routable. A session
        // already closing hands back a subscription that ends at once.
        if child_parent.is_none() {
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
        }
        Ok(session_id)
    }
}

fn child_tool_allowed(tool: &str, child: &AgentDefinition, parent: &AgentDefinition) -> bool {
    let declared = child.tools.get(tool).copied().unwrap_or(true);
    let write_capability = matches!(tool, "write" | "edit" | "bash");
    let parent_ceiling = !write_capability
        || (parent.access != crate::agent::definitions::AgentAccess::ReadOnly
            && parent.id.as_str() != crate::agent::definitions::PLAN);
    let child_access =
        !write_capability || child.access != crate::agent::definitions::AgentAccess::ReadOnly;
    declared
        && parent.tools.get(tool).copied().unwrap_or(true)
        && parent_ceiling
        && child_access
        && !matches!(tool, "ask" | "task" | "steer_subagent")
}

#[cfg(test)]
mod child_policy_tests {
    use super::*;
    use crate::agent::definitions::{AgentAccess, AgentId, AgentMode, AgentSource};

    fn definition(access: AgentAccess, tools: &[(&str, bool)]) -> AgentDefinition {
        AgentDefinition {
            id: AgentId::parse("agent").unwrap(),
            description: String::new(),
            mode: AgentMode::All,
            prompt: String::new(),
            model: None,
            tools: tools
                .iter()
                .map(|(name, allowed)| ((*name).to_string(), *allowed))
                .collect(),
            access,
            source: AgentSource::Builtin,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn child_tool_policy_is_intersected_with_definition_and_parent_ceiling() {
        let child = definition(
            AgentAccess::Full,
            &[("read", false), ("grep", false), ("write", true)],
        );
        let build = definition(AgentAccess::Full, &[]);
        let plan = AgentDefinition {
            id: AgentId::parse("plan").unwrap(),
            ..build.clone()
        };
        let readonly_parent = definition(AgentAccess::ReadOnly, &[]);
        let readonly_child = definition(AgentAccess::ReadOnly, &[]);
        for (tool, child, parent, expected) in [
            ("read", &child, &build, false),
            ("grep", &child, &build, false),
            ("glob", &child, &build, true),
            ("write", &child, &build, true),
            ("write", &child, &readonly_parent, false),
            ("bash", &child, &plan, false),
            ("edit", &readonly_child, &build, false),
            ("fetch", &readonly_child, &build, true),
            ("ask", &build, &build, false),
            ("task", &build, &build, false),
        ] {
            assert_eq!(child_tool_allowed(tool, child, parent), expected, "{tool}");
        }
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
