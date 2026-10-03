//! Per-actor runtime task: the sole holder of `&mut AgentActor`,
//! driving `inbox batch -> run_agent` and translating the terminal
//! [`RunOutcome`] into bus events.
//!
//! Ordering contract: turn.start -> llm.delta / llm.response / tool.pre_call /
//! tool.post_call (interleaved as the step produces them) -> terminal
//! turn-end events -> back to the inbox wait. `task.done` fires exactly once
//! per task, on [`RunOutcome::Done`] only. A natural turn end returns to the
//! inbox wait; `session.end` is sequenced ONLY at teardown via
//! [`Sequencer::close_session`] — never on a natural turn end.

mod compact;
mod deferred;
mod delivery;
mod steering;
pub use delivery::Delivery;
pub use delivery::PendingNotice;
mod model;

use std::sync::{Arc, Mutex};
use std::time::Instant;

pub use compact::{
    deferred_wire, CheckpointSaver, CompactReply, CompactRequest, Compacted, Control,
    FOCUS_MAX_CHARS,
};
pub use model::{
    BoxFuture, Install, ModelReply, ModelRequest, ModelSwitch, ModelSwitcher, ModelTarget,
    PENDING_MODEL_ERROR,
};

use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::agent::actor::{AgentActor, AgentState};
use crate::agent::context::{Breakdown, ContextSnapshot, ContextTracker};
use crate::agent::permissions::PermissionDecision;
use crate::agent::run_loop::{
    continue_run, resolve_permission, run_agent, AgentEvent, ResolveError, RunOutcome,
};
use crate::bus::event::{
    AgentStateChanged, AgentYield, EventKind, PermissionResolution, PermissionResolved, TaskDone,
    TaskStatus, ToolPostCall, ToolPreCall, ToolSource, TurnEnd, TurnEndOutcome, TurnError,
    TurnStart, ROOT_ACTOR_ID,
};
use crate::bus::inbox::{ActorInbox, Inbound};
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::error::AgentError;

/// The turn-end transcript hook: called with the actor's transcript after
/// each turn's terminal events (the history store's snapshot seam).
pub type TurnEndHook = Box<dyn Fn(&[crate::provider::openai::ChatMessage]) + Send>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionOutcome {
    Done,
    Failed,
    Stopped,
    Parked,
}
pub type CompletionHook = Box<dyn Fn(crate::provider::AssistantMessage, CompletionOutcome) + Send>;

/// Cancel reason recorded on `session.end` when the session token fires.
const CANCEL_REASON: &str = "cancel";
/// Cancel reason recorded on `session.end` when the inbox senders all drop.
const END_OF_STREAM_REASON: &str = "inbox_closed";
/// Wire cap for `turn.end{error}.message` (bounded, no secrets).
const ERROR_SUMMARY_CAP: usize = 256;

/// A selected model variant: its name and override object.
pub type SelectedVariant = Option<(String, crate::provider::config::Variant)>;

/// The variant a session runs with. A change made while a turn runs waits in
/// `pending` and takes effect when the next turn starts, so one turn never
/// mixes two variants.
#[derive(Debug, Default)]
struct VariantSlot {
    active: SelectedVariant,
    pending: Option<SelectedVariant>,
    running: bool,
}

/// The permission request the actor waits on, and whether a decision for it
/// was already taken in.
#[derive(Debug, Default)]
struct PermissionSlot {
    id: Option<Ulid>,
    claimed: bool,
}

/// What the runtime shares with the session service about its actor, so the
/// service can decide whether a session command may run without reaching
/// into the actor.
#[derive(Debug, Default)]
pub struct RuntimeStatus {
    pending_notice: Mutex<Option<delivery::NoticeCell>>,
    steering: Arc<steering::Steering>,
    budget_parked: std::sync::atomic::AtomicBool,
    delivery: Mutex<Option<delivery::DeliveryCell>>,
    variant: Mutex<VariantSlot>,
    /// The session's context gauge: written by the runtime at each boundary,
    /// read on demand by the service.
    context: Mutex<Option<ContextTracker>>,
    /// The profile's instructions, which the role message carries after the
    /// agent's prompt; needed to re-estimate the role when the agent changes.
    profile_prompt: Mutex<Option<String>>,
    /// The id of the permission request the actor waits on, if any, so the
    /// service can refuse a stale decision without waiting for the runtime.
    pending_permission: Mutex<PermissionSlot>,
    /// The primary agent the session runs as, and the agents it may switch
    /// to. A turn reads it at its start.
    pub selection: crate::agent::selection::AgentSelection,
    /// Compaction requests to the runtime, and how it saves their result.
    controls: compact::Controls,
}

impl RuntimeStatus {
    pub fn install_pending_notice(&self, callback: PendingNotice) {
        *self
            .pending_notice
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(delivery::NoticeCell(callback));
    }
    async fn notice_pending(&self, id: Ulid) -> bool {
        let callback = self
            .pending_notice
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|c| c.0.clone());
        match callback {
            Some(callback) => callback(id).await.unwrap_or(false),
            None => false,
        }
    }
    pub fn steer(&self, text: String, interrupt: bool) -> bool {
        let id = Ulid::new();
        self.steering
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(steering::Operation {
                id,
                content: text,
                interrupt,
            });
        self.controls
            .tx
            .send(if interrupt {
                Control::Interrupt(id)
            } else {
                Control::Steer
            })
            .is_ok()
    }
    pub fn continue_budget(&self) -> bool {
        if self
            .budget_parked
            .compare_exchange(
                true,
                false,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.controls.tx.send(Control::Continue).is_ok()
    }
    pub fn wake_delivery(&self) {
        let _ = self.controls.tx.send(Control::Delivery);
    }
    pub fn wake_delivery_mode(&self, mode: &str, notice: Ulid) {
        let control = match mode {
            "steer" => Control::DeliverySteer,
            "interrupt" => Control::DeliveryInterrupt(notice),
            _ => Control::Delivery,
        };
        let _ = self.controls.tx.send(control);
    }
    pub fn install_delivery(&self, delivery: Delivery) {
        *self.delivery.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(delivery::DeliveryCell(delivery));
    }
    /// A status that runs as the agent `selection` names.
    pub fn with_selection(selection: crate::agent::selection::AgentSelection) -> Self {
        Self {
            selection,
            ..Self::default()
        }
    }

    /// The agent asked the human something and waits for the answer.
    pub fn pending_ask(&self) -> bool {
        self.selection.ask_pending()
    }

    /// The id of the permission request waiting for a decision.
    pub fn pending_permission(&self) -> Option<Ulid> {
        self.permission_slot().id
    }

    /// Takes the right to answer the request `id`: true for the first caller
    /// while that request waits, false for any other, whether the request is
    /// gone or another answer already holds it. Check and claim happen under
    /// one lock, so two concurrent answers cannot both win.
    pub fn claim_permission(&self, id: Ulid) -> bool {
        let mut slot = self.permission_slot();
        if slot.id == Some(id) && !slot.claimed {
            slot.claimed = true;
            return true;
        }
        false
    }

    /// Gives a claim back, for an answer the runtime did not take.
    pub fn release_permission(&self, id: Ulid) {
        let mut slot = self.permission_slot();
        if slot.id == Some(id) {
            slot.claimed = false;
        }
    }

    fn set_pending_permission(&self, id: Option<Ulid>) {
        *self.permission_slot() = PermissionSlot { id, claimed: false };
    }

    fn permission_slot(&self) -> std::sync::MutexGuard<'_, PermissionSlot> {
        match self.pending_permission.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, VariantSlot> {
        match self.variant.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Sets the variant the session starts with, before its first turn.
    pub fn init_variant(&self, variant: SelectedVariant) {
        self.slot().active = variant;
    }

    /// The variant the session runs with between turns.
    fn active_variant(&self) -> SelectedVariant {
        self.slot().active.clone()
    }

    /// Supplies how the runtime saves a compaction: the transcript and the
    /// checkpoint in one acknowledged write.
    pub fn install_checkpoint_saver(&self, saver: CheckpointSaver) {
        self.controls.set_saver(saver);
    }

    /// Asks the runtime to compact the conversation and waits for its
    /// answer: the result when it ran now, `Deferred` when a turn is running
    /// and it runs when that turn ends. `None` when the runtime is gone.
    pub async fn request_compact(
        &self,
        focus: Option<String>,
        operation_id: Ulid,
    ) -> Option<CompactReply> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.controls
            .tx
            .send(Control::Compact(CompactRequest {
                focus,
                operation_id,
                reply,
            }))
            .ok()?;
        answer.await.ok()
    }

    /// Supplies how the runtime resolves a model alias and commits a switch
    /// to it, so the bus never imports the session service or the store.
    pub fn install_model_switcher(&self, switcher: ModelSwitcher) {
        self.controls.set_switcher(switcher);
    }

    /// Asks the runtime to move the conversation to the model `alias` and
    /// waits for its answer: the result when it ran now, `Deferred` when a
    /// turn is running and it runs when that turn ends. `None` when the
    /// runtime is gone.
    pub async fn request_model_switch(
        &self,
        alias: String,
        operation_id: Ulid,
    ) -> Option<ModelReply> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.controls
            .tx
            .send(Control::Model(ModelRequest {
                alias,
                operation_id,
                reply,
            }))
            .ok()?;
        answer.await.ok()
    }

    /// Replaces the variant the session runs with, dropping a change that was
    /// waiting for the running turn: a model switch starts on its own default.
    fn replace_variant(&self, variant: SelectedVariant) {
        let mut slot = self.slot();
        slot.active = variant;
        slot.pending = None;
    }

    /// Installs the context tracker of a new model selection.
    fn replace_context(&self, tracker: ContextTracker) {
        *self.lock_context() = Some(tracker);
    }

    /// The variant name that is selected: the pending one when a change is
    /// waiting for the running turn to end, else the active one.
    pub fn selected_variant(&self) -> Option<String> {
        let slot = self.slot();
        let chosen = slot.pending.as_ref().unwrap_or(&slot.active);
        chosen.as_ref().map(|(name, _)| name.clone())
    }

    /// Selects `variant` (`None` clears it). Returns `true` when it takes
    /// effect now, `false` when a turn is running and it waits for the next
    /// turn.
    pub fn select_variant(&self, variant: SelectedVariant) -> bool {
        let mut slot = self.slot();
        if slot.running {
            slot.pending = Some(variant);
            false
        } else {
            slot.active = variant;
            slot.pending = None;
            true
        }
    }

    /// A turn starts: a waiting variant change is applied, and the agent and
    /// the variant the whole turn runs with are returned. The agent is
    /// `None` for a status that sends no role.
    async fn begin_turn(
        &self,
    ) -> (
        Option<Arc<crate::agent::definitions::AgentDefinition>>,
        SelectedVariant,
    ) {
        let role = self.selection.begin_turn().await;
        self.set_pending_permission(None);
        let mut slot = self.slot();
        if let Some(pending) = slot.pending.take() {
            slot.active = pending;
        }
        slot.running = true;
        (role, slot.active.clone())
    }

    /// The turn is over: an agent or variant change may apply.
    fn end_turn(&self, actor: &AgentActor) {
        self.selection.end_turn(actor.has_pending());
        self.set_pending_permission(actor.pending_permission().map(|request| request.id));
        self.slot().running = false;
    }

    /// Installs the session's context tracker before the runtime starts.
    /// Without one the runtime publishes no `context.updated`.
    pub fn install_context(&self, tracker: ContextTracker, profile_prompt: Option<String>) {
        *self.lock_context() = Some(tracker);
        *match self.profile_prompt.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        } = profile_prompt;
    }

    /// Parks the runtime after its next compaction and before the result is
    /// published.
    #[cfg(test)]
    pub(crate) fn arm_switch_barrier(&self) -> Arc<compact::ResultBarrier> {
        self.controls.arm_switch_barrier()
    }

    /// Parks the runtime after its next compaction and before the result is
    /// published.
    #[cfg(test)]
    pub(crate) fn arm_result_barrier(&self) -> Arc<compact::ResultBarrier> {
        self.controls.arm_barrier()
    }

    /// Re-estimates the system category for a session now running as `role`;
    /// the new snapshot when the estimate changed. Only between turns: the
    /// other categories stay as the last turn left them.
    pub fn set_context_role(
        &self,
        role: &crate::agent::definitions::AgentDefinition,
    ) -> Option<ContextSnapshot> {
        let profile = match self.profile_prompt.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
        .clone();
        let message =
            crate::agent::actor::compose_role_message(Some(&role.prompt), profile.as_deref());
        let mut tracker = self.lock_context();
        let tracker = tracker.as_mut()?;
        let mut breakdown = tracker.snapshot().breakdown;
        breakdown.system = crate::agent::context::system_tokens(message.as_deref());
        tracker.update(breakdown).cloned()
    }

    /// The latest context snapshot, `None` for a session without a tracker.
    pub fn context_snapshot(&self) -> Option<ContextSnapshot> {
        self.lock_context().as_ref().map(|t| t.snapshot().clone())
    }

    fn lock_context(&self) -> std::sync::MutexGuard<'_, Option<ContextTracker>> {
        match self.context.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Shows `variant` in the gauge; the new snapshot when the name changed.
    pub fn set_context_variant(&self, variant: Option<String>) -> Option<ContextSnapshot> {
        self.lock_context()
            .as_mut()
            .and_then(|t| t.set_variant(variant).cloned())
    }

    /// Records the provider's count for `step`.
    fn record_usage(&self, step: Ulid, usage: crate::provider::Usage) {
        if let Some(tracker) = self.lock_context().as_mut() {
            tracker.record_usage(step, usage);
        }
    }

    /// Moves the gauge to `breakdown`; the new snapshot when it changed.
    fn update_context(&self, breakdown: Breakdown) -> Option<ContextSnapshot> {
        self.lock_context()
            .as_mut()
            .and_then(|t| t.update(breakdown).cloned())
    }

    /// Records that the session starts with `actor`'s unanswered `ask`, if any.
    pub fn init_ask(&self, actor: &AgentActor) {
        self.selection.set_ask_pending(actor.has_pending());
    }
}

/// One turn's publishing state: identity (turn, task, current step, last
/// response message) plus the publishing seam. The run observer mutates it;
/// the runtime reads it back once the run future is dropped.
///
/// Publishing has collapse semantics: on the FIRST publish failure the error
/// is retained for the turn driver and later publishes become silent no-ops
/// (the session is collapsed; the runtime tears it down).
struct TurnPublisher {
    sequencer: Sequencer,
    status: Arc<RuntimeStatus>,
    turn_id: Ulid,
    task_id: Ulid,
    step_id: Ulid,
    message_id: Ulid,
    /// The tool call being dispatched and when it started; native tools run
    /// one at a time, so one slot is enough.
    pending_tool: Option<(String, Instant)>,
    error: Option<crate::error::BusError>,
}

impl TurnPublisher {
    fn new(sequencer: Sequencer, status: Arc<RuntimeStatus>, turn_id: Ulid, task_id: Ulid) -> Self {
        Self {
            sequencer,
            status,
            turn_id,
            task_id,
            step_id: Ulid::new(),
            message_id: Ulid::new(),
            pending_tool: None,
            error: None,
        }
    }

    /// Publishes `payload` as `kind`, correlated to the turn's task.
    fn publish(&mut self, kind: EventKind, payload: serde_json::Value) {
        if self.error.is_some() {
            return;
        }
        let candidate = Candidate::new(kind, payload).with_correlation_id(self.task_id);
        if let Err(e) = self.sequencer.publish(candidate) {
            self.error = Some(e);
        }
    }

    /// Moves the context gauge to `breakdown` and publishes `context.updated`
    /// when it changed.
    fn publish_context(&mut self, breakdown: Breakdown) {
        if let Some(snapshot) = self.status.update_context(breakdown) {
            let payload = to_value(&snapshot);
            self.publish(EventKind::ContextUpdated, payload);
        }
    }

    fn take_error(&mut self) -> Option<crate::error::BusError> {
        self.error.take()
    }

    /// Translates one run event onto the bus. Pure: never feeds back into
    /// the loop's control flow.
    fn observe(&mut self, event: AgentEvent<'_>) {
        match event {
            AgentEvent::StepStarted => self.step_id = Ulid::new(),
            AgentEvent::Delta(delta) => {
                let payload = to_value(&crate::bus::event::LlmDelta {
                    turn_id: self.turn_id,
                    step_id: self.step_id,
                    delta,
                });
                self.publish(EventKind::LlmDelta, payload);
            }
            AgentEvent::ContextEstimated(breakdown) => self.publish_context(breakdown),
            AgentEvent::Response(message) => {
                if let Some(usage) = message.usage {
                    self.status.record_usage(self.step_id, usage);
                }
                self.message_id = Ulid::new();
                let payload = to_value(&crate::bus::event::LlmResponse {
                    turn_id: self.turn_id,
                    step_id: self.step_id,
                    message: message.clone(),
                    usage: message.usage,
                    finish_reason: message.finish_reason.clone(),
                });
                self.publish(EventKind::LlmResponse, payload);
            }
            AgentEvent::PreCall {
                tool_call_id,
                name,
                args,
            } => {
                self.pending_tool = Some((tool_call_id.to_string(), Instant::now()));
                let payload = to_value(&ToolPreCall {
                    tool_call_id: tool_call_id.to_string(),
                    name: name.to_string(),
                    source: ToolSource::Native,
                    args: args.clone(),
                    actor_id: ROOT_ACTOR_ID.to_string(),
                });
                self.publish(EventKind::ToolPreCall, payload);
            }
            AgentEvent::PostCall {
                tool_call_id,
                name,
                result,
            } => {
                let duration_ms = match self.pending_tool.take() {
                    Some((id, started)) if id == tool_call_id => {
                        Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
                    }
                    _ => None,
                };
                let payload = to_value(&ToolPostCall {
                    tool_call_id: tool_call_id.to_string(),
                    name: name.to_string(),
                    status: result.status,
                    output: result.output.clone(),
                    metadata: with_duration(result.metadata.clone(), duration_ms),
                });
                self.publish(EventKind::ToolPostCall, payload);
            }
        }
    }
}

/// Adds `duration_ms` to a tool result's metadata, keeping its other keys.
/// `Null` becomes an object; any other non-object value is left untouched.
fn with_duration(metadata: serde_json::Value, duration_ms: Option<u64>) -> serde_json::Value {
    let Some(ms) = duration_ms else {
        return metadata;
    };
    match metadata {
        serde_json::Value::Null => serde_json::json!({ "duration_ms": ms }),
        serde_json::Value::Object(mut map) => {
            map.insert("duration_ms".to_string(), ms.into());
            serde_json::Value::Object(map)
        }
        other => other,
    }
}

/// The runtime task: one per actor. Waits for an inbox batch or session
/// cancellation, drives `run_agent`, publishes events, and tears the session
/// down permanently on cancellation / stop / end-of-stream. Returns the
/// actor so a caller (test or session manager) can assert its final state.
///
/// `on_turn_end(&[ChatMessage])`, when given, is called after each turn's
/// terminal events (turn.end/task.done/agent.state) with the actor's
/// transcript snapshot at that point — the history store's hook.
pub fn spawn(
    session_id: Ulid,
    actor: AgentActor,
    inbox: ActorInbox,
    sequencer: Sequencer,
    session_cancel: CancellationToken,
    on_turn_end: Option<TurnEndHook>,
) -> tokio::task::JoinHandle<AgentActor> {
    spawn_with_status(
        session_id,
        actor,
        inbox,
        sequencer,
        session_cancel,
        on_turn_end,
        Arc::new(RuntimeStatus::default()),
    )
}

/// [`spawn`] with a status the caller keeps to read the actor's pending
/// `ask` flag. The status reflects the actor as soon as this returns, before
/// the runtime task has run.
pub fn spawn_with_status(
    session_id: Ulid,
    actor: AgentActor,
    inbox: ActorInbox,
    sequencer: Sequencer,
    session_cancel: CancellationToken,
    on_turn_end: Option<TurnEndHook>,
    status: Arc<RuntimeStatus>,
) -> tokio::task::JoinHandle<AgentActor> {
    spawn_with_completion(
        session_id,
        actor,
        inbox,
        sequencer,
        session_cancel,
        on_turn_end,
        status,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_with_completion(
    session_id: Ulid,
    actor: AgentActor,
    inbox: ActorInbox,
    sequencer: Sequencer,
    session_cancel: CancellationToken,
    on_turn_end: Option<TurnEndHook>,
    status: Arc<RuntimeStatus>,
    completion: Option<CompletionHook>,
) -> tokio::task::JoinHandle<AgentActor> {
    status.init_ask(&actor);
    tokio::spawn(runtime_loop(
        session_id,
        actor,
        inbox,
        sequencer,
        session_cancel,
        on_turn_end.unwrap_or_else(|| Box::new(|_| {})),
        status,
        completion,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn runtime_loop(
    _session_id: Ulid,
    mut actor: AgentActor,
    mut inbox: ActorInbox,
    sequencer: Sequencer,
    session_cancel: CancellationToken,
    on_turn_end: TurnEndHook,
    status: Arc<RuntimeStatus>,
    completion: Option<CompletionHook>,
) -> AgentActor {
    let on_turn_end: SharedHook = Arc::new(Mutex::new(on_turn_end));
    let mut active_task_id: Option<Ulid> = None;
    let mut control = status.controls.take_receiver();
    let mut delivery_waiting = false;
    let boundary_status = status.clone();
    actor.safe_boundary = Some(Arc::new(move |transcript| {
        let status = boundary_status.clone();
        Box::pin(async move {
            let mut transcript = status.steering.apply(transcript);
            let callback = status
                .delivery
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|c| c.0.clone());
            if let Some(callback) = callback {
                if let Some(updated) = callback(transcript.clone(), true).await? {
                    transcript = updated;
                }
            }
            Ok(transcript)
        })
    }));

    loop {
        let inbound = tokio::select! {
            biased;
            _ = session_cancel.cancelled() => {
                actor.cancel.cancel();
                break;
            }
            Some(control) = recv_control(&mut control) => {
                // Between turns: run it now, publish its result, then answer.
                match control {
                    Control::Steer => {
                        if actor.has_pending() || status.budget_parked.load(std::sync::atomic::Ordering::Acquire)
                            || status.steering.queue.lock().unwrap_or_else(|e| e.into_inner()).is_empty() { continue; }
                        actor.transcript = status.steering.apply(actor.transcript.clone());
                        Some(Inbound::Continue)
                    }
                    Control::Interrupt(id) | Control::Replace(id) => {
                        if let Some(request) = actor.pending_permission().cloned() {
                            actor.interrupt_pending_permission();
                            publish_resolved(&sequencer, active_task_id, &request, PermissionResolution::Cancelled);
                        } else if actor.has_pending() {
                            actor.pending = None;
                            actor.settle_unanswered_calls();
                        }
                        actor.cancel = session_cancel.child_token();
                        let Some(transcript) = status.steering.replacement(id,actor.transcript.clone()) else { continue; };
                        actor.transcript = status.steering.apply(transcript);
                        Some(Inbound::Continue)
                    }
                    Control::Continue => Some(Inbound::Continue),
                    Control::Delivery => { delivery_waiting = true; continue; }
                    Control::DeliverySteer => { delivery_waiting=true; continue; }
                    Control::DeliveryInterrupt(id) => { if status.notice_pending(id).await { delivery_waiting=true; } continue; }
                    Control::Compact(request) => {
                        let outcome = compact::run(
                            &mut actor,
                            &status,
                            &sequencer,
                            &session_cancel,
                            request.focus.as_deref(),
                        )
                        .await;
                        compact::answer(&status, &sequencer, request, outcome).await;
                        continue;
                    }
                    Control::Model(request) => {
                        let outcome = model::run(
                            &mut actor,
                            &status,
                            &sequencer,
                            &session_cancel,
                            &request.alias,
                            request.operation_id,
                        )
                        .await;
                        model::answer(&status, &sequencer, request, outcome).await;
                        continue;
                    }
                }
            }
            inbound = async {
                // A command held behind an ask or a permission request goes
                // in once nothing is pending.
                if !actor.has_pending() {
                    inbox.release_held();
                }
                inbox.next_inbound().await
            } => inbound,
            _ = async {
                if delivery_waiting && !actor.has_pending() { return; }
                std::future::pending::<()>().await;
            } => {
                delivery_waiting = true;
                if actor.has_pending() { continue; }
                delivery_waiting = false;
                match delivery::apply(&mut actor, &status, &sequencer).await {
                    Ok(true) => Some(Inbound::Completion),
                    Ok(false) => continue,
                    Err(error) => { tracing::error!(%error, "subagent wake delivery failed"); break; }
                }
            }

        };

        let Some(inbound) = inbound else {
            break;
        };

        // A decision resumes the held call; a batch of commands starts a
        // turn the usual way.
        let resume: Option<(Ulid, PermissionDecision)> = match inbound {
            Inbound::Continue => None,
            Inbound::Completion => {
                active_task_id = Some(Ulid::new());
                None
            }
            Inbound::Batch(_) => {
                let held = actor.pending_permission().cloned();
                let applied = inbox.apply_batch_to_actor(&mut actor);
                // Only commands that were held: nothing changed, no turn.
                let Some(first) = applied.first_applied else {
                    continue;
                };
                if active_task_id.is_none() {
                    active_task_id = Some(first);
                }
                // Text never answers a request: it settles it as cancelled.
                if let Some(request) = held {
                    if actor.pending_permission().is_none() {
                        publish_resolved(
                            &sequencer,
                            active_task_id,
                            &request,
                            PermissionResolution::Cancelled,
                        );
                    }
                }
                None
            }
            Inbound::Answer(answer) => {
                let held = actor
                    .pending_permission()
                    .filter(|request| request.id == answer.request_id)
                    .cloned();
                let Some(request) = held else {
                    let _ = answer
                        .reply
                        .send(Err(crate::agent::permissions::PermissionError::Stale));
                    continue;
                };
                let decision =
                    crate::agent::permissions::offered_decision(&request.agent, answer.decision);
                publish_resolved(
                    &sequencer,
                    active_task_id,
                    &request,
                    PermissionResolution::from(&decision),
                );
                let _ = answer.reply.send(Ok(()));
                Some((answer.request_id, decision))
            }
        };
        // The turn runs as the agent and the variant selected now; a later
        if let Err(error) = delivery::apply(&mut actor, &status, &sequencer).await {
            tracing::error!(%error, "subagent transcript delivery failed");
            break;
        }
        // change waits for the turn to end.
        let (role, variant) = status.begin_turn().await;
        status
            .budget_parked
            .store(false, std::sync::atomic::Ordering::Release);
        actor.role = role;
        actor.variant = variant.map(|(_, variant)| variant);
        // A request that would not fit this budget is compacted first.
        actor.input_budget = status.context_snapshot().map(|s| s.input_budget);

        let task_id = active_task_id.unwrap_or_default();
        let turn_id = Ulid::new();
        let mut turn = TurnPublisher::new(sequencer.clone(), status.clone(), turn_id, task_id);

        turn.publish(
            EventKind::TurnStart,
            to_value(&TurnStart {
                turn_id,
                actor_id: ROOT_ACTOR_ID.to_string(),
                task_id,
            }),
        );

        // The gauge with the new message in it, before the first request.
        turn.publish_context(actor.context_breakdown());

        let mut deferred: deferred::Queue = Vec::new();
        let started = Instant::now();
        // Session cancellation must interrupt a MID-FLIGHT turn too: the
        // select cancels the ACTOR token (cloned handle avoids aliasing the
        // `&mut actor` the future holds), the in-flight provider call
        // unwinds as Canceled -> Interrupted, the terminal events below fire
        // from the natural outcome, and the loop breaks into teardown. The
        // block scope drops the future (+ its &mut actor borrow) BEFORE the
        // outcome handling below assigns actor.state.
        let actor_cancel = actor.cancel.clone();
        let mut interrupted_for_input = false;
        let mut replacement = None;
        let outcome = {
            let mut observe = |event: AgentEvent<'_>| turn.observe(event);
            let turn_fut = async {
                let first = match resume {
                    None => run_agent(&mut actor, &mut observe).await,
                    Some((request_id, decision)) => {
                        match resolve_permission(&mut actor, request_id, decision, &mut observe)
                            .await
                        {
                            Ok(outcome) => Ok(outcome),
                            Err(ResolveError::Agent(err)) => Err(err),
                            // Checked against the pending request above.
                            Err(ResolveError::Rejected(err)) => {
                                tracing::error!(error = %err, "a checked permission decision was rejected");
                                Ok(RunOutcome::Interrupted)
                            }
                        }
                    }
                };
                compact_when_full(&mut actor, &status, &sequencer, &mut observe, first).await
            };
            tokio::pin!(turn_fut);
            loop {
                tokio::select! {
                    biased;
                    _ = session_cancel.cancelled() => {
                        actor_cancel.cancel();
                        break (&mut turn_fut).await;
                    }
                    outcome = &mut turn_fut => break outcome,
                    Some(control) = recv_control(&mut control) => {
                        // A turn is running: accept now, run it at its end.
                        match control {
                            Control::Steer => {}
                            Control::Interrupt(id) => {
                                replacement = Some(id);
                                interrupted_for_input = true;
                                actor_cancel.cancel();
                                break (&mut turn_fut).await;
                            }
                            Control::Replace(_) => {}
                            Control::Continue => {}
                            Control::Delivery => { delivery_waiting = true; }
                            Control::DeliverySteer => { delivery_waiting=true; }
                            Control::DeliveryInterrupt(id) => {
                                if !status.notice_pending(id).await { continue; }
                                delivery_waiting=true;
                                interrupted_for_input=true;
                                actor_cancel.cancel();
                                break (&mut turn_fut).await;
                            }
                            Control::Compact(request) => {
                                let (operation_id, focus) =
                                    (request.operation_id, request.focus.clone());
                                if compact::accept_deferred(&sequencer, request).await {
                                    deferred.push(deferred::Deferred::Compact {
                                        operation_id,
                                        focus,
                                    });
                                }
                            }
                            Control::Model(request) => {
                                let (operation_id, alias) =
                                    (request.operation_id, request.alias.clone());
                                if model::accept_deferred(&sequencer, request).await {
                                    deferred.push(deferred::Deferred::Model {
                                        operation_id,
                                        alias,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        };
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result_message_id = turn.message_id;
        // The turn's last word on the gauge: it also covers a cancelled turn
        // and the usage of a final response.
        turn.publish_context(actor.context_breakdown());
        status.end_turn(&actor);

        // A collapsed session surfaces its first publish failure as the turn
        // error (checked BEFORE translating the natural outcome).
        if let Some(err) = turn.take_error() {
            actor.state = AgentState::Idle;
            emit_turn_end(
                &mut turn,
                turn_id,
                TurnEndOutcome::Error,
                None,
                duration_ms,
                Some(TurnError {
                    code: "bus".to_string(),
                    message: bounded(&err.to_string(), ERROR_SUMMARY_CAP),
                }),
            );
            emit_agent_state(&mut turn, "idle");
            snapshot_transcript(&sequencer, &on_turn_end, &actor.transcript);
            deferred::report_cancelled(&sequencer, &deferred, compact::CANCELLED);
            break;
        }

        match outcome {
            Ok(RunOutcome::Done(message)) => {
                if let Some(done) = &completion {
                    done(message.clone(), CompletionOutcome::Done);
                }
                emit_turn_end(
                    &mut turn,
                    turn_id,
                    TurnEndOutcome::Done,
                    message.usage,
                    duration_ms,
                    None,
                );
                turn.publish(
                    EventKind::TaskDone,
                    to_value(&TaskDone {
                        task_id,
                        actor_id: ROOT_ACTOR_ID.to_string(),
                        result_message_id,
                        status: TaskStatus::Done,
                    }),
                );
                emit_agent_state(&mut turn, "idle");
                active_task_id = None;
            }
            Ok(RunOutcome::Yielded(request)) => {
                if let Some(done) = &completion {
                    done(
                        crate::provider::AssistantMessage {
                            text: request.question.clone(),
                            ..Default::default()
                        },
                        CompletionOutcome::Parked,
                    );
                }
                turn.publish(
                    EventKind::AgentYield,
                    to_value(&AgentYield {
                        actor_id: ROOT_ACTOR_ID.to_string(),
                        turn_id,
                        question: request.question,
                        kind: request.kind,
                        details: request.details,
                    }),
                );
                emit_turn_end(
                    &mut turn,
                    turn_id,
                    TurnEndOutcome::Yielded,
                    None,
                    duration_ms,
                    None,
                );
                emit_agent_state(&mut turn, "yielded");
            }
            Ok(RunOutcome::PermissionRequired(request)) => {
                if let Some(done) = &completion {
                    done(
                        crate::provider::AssistantMessage {
                            text: request.reason.clone(),
                            ..Default::default()
                        },
                        CompletionOutcome::Parked,
                    );
                }
                turn.publish(
                    EventKind::PermissionRequested,
                    permission_requested(&request),
                );
                emit_turn_end(
                    &mut turn,
                    turn_id,
                    TurnEndOutcome::Yielded,
                    None,
                    duration_ms,
                    None,
                );
                emit_agent_state(&mut turn, "yielded");
            }
            Ok(RunOutcome::Interrupted) => {
                if let Some(done) = completion.as_ref().filter(|_| !interrupted_for_input) {
                    done(
                        crate::provider::AssistantMessage {
                            text: "stopped".into(),
                            ..Default::default()
                        },
                        CompletionOutcome::Stopped,
                    );
                }
                emit_turn_end(
                    &mut turn,
                    turn_id,
                    TurnEndOutcome::Interrupted,
                    None,
                    duration_ms,
                    None,
                );
                emit_agent_state(&mut turn, "idle");
            }
            // `compact_when_full` consumes it before the run ends.
            Ok(RunOutcome::NeedsCompaction { .. }) | Ok(RunOutcome::StepBudgetExhausted) => {
                status
                    .budget_parked
                    .store(true, std::sync::atomic::Ordering::Release);
                if let Some(done) = &completion {
                    done(
                        crate::provider::AssistantMessage {
                            text: "step budget exhausted".into(),
                            ..Default::default()
                        },
                        CompletionOutcome::Parked,
                    );
                }
                // The internal variant NEVER leaks: the wire sees a yield of
                // kind "budget" and the actor stays Yielded.
                turn.publish(
                    EventKind::AgentYield,
                    to_value(&AgentYield {
                        actor_id: ROOT_ACTOR_ID.to_string(),
                        turn_id,
                        question: "step budget exhausted".to_string(),
                        kind: Some("budget".to_string()),
                        details: None,
                    }),
                );
                emit_turn_end(
                    &mut turn,
                    turn_id,
                    TurnEndOutcome::Yielded,
                    None,
                    duration_ms,
                    None,
                );
                emit_agent_state(&mut turn, "yielded");
            }
            Err(err) => {
                if let Some(done) = &completion {
                    done(
                        crate::provider::AssistantMessage {
                            text: err.to_string(),
                            ..Default::default()
                        },
                        CompletionOutcome::Failed,
                    );
                }
                // Err sets Idle FIRST, then the events observe it.
                actor.state = AgentState::Idle;
                emit_turn_end(
                    &mut turn,
                    turn_id,
                    TurnEndOutcome::Error,
                    None,
                    duration_ms,
                    Some(TurnError {
                        code: error_code(&err),
                        message: bounded(&err.to_string(), ERROR_SUMMARY_CAP),
                    }),
                );
                emit_agent_state(&mut turn, "idle");
            }
        }

        // Every terminal outcome saves the transcript, a cancelled turn too.
        snapshot_transcript(&sequencer, &on_turn_end, &actor.transcript);

        if interrupted_for_input && !session_cancel.is_cancelled() {
            actor.cancel = session_cancel.child_token();
            if let Some(id) = replacement {
                let _ = status.controls.tx.send(Control::Replace(id));
            }
        } else if !actor.has_pending()
            && !status
                .steering
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        {
            let _ = status.controls.tx.send(Control::Steer);
        }

        if actor.cancel.is_cancelled() || session_cancel.is_cancelled() {
            // The stop wins: a `/compact` accepted during this turn is told
            // it will not happen, before `session.end`.
            deferred::report_cancelled(&sequencer, &deferred, compact::CANCELLED);
            break;
        }

        // The controls that arrived during the turn run now, between turns,
        // in the order they came.
        deferred::run(
            std::mem::take(&mut deferred),
            &mut actor,
            &status,
            &sequencer,
            &session_cancel,
        )
        .await;
    }

    // A stop (or the end of the inbox) while a request waits: its calls never
    // run, and the request ends as cancelled.
    if let Some(request) = actor.pending_permission().cloned() {
        actor.interrupt_pending_permission();
        publish_resolved(
            &sequencer,
            active_task_id,
            &request,
            PermissionResolution::Cancelled,
        );
        snapshot_transcript(&sequencer, &on_turn_end, &actor.transcript);
    }
    teardown(&mut actor, &sequencer);
    actor
}

/// Carries a turn through the automatic compactions it needs: each time the
/// run stops at a safe boundary because the next request would not fit, the
/// conversation is compacted and the SAME turn goes on with the steps it had
/// left. A compaction that cannot make room ends the turn with an error; one
/// that a stop cancelled ends it as interrupted.
async fn compact_when_full(
    actor: &mut AgentActor,
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
    first: Result<RunOutcome, crate::error::AgentError>,
) -> Result<RunOutcome, crate::error::AgentError> {
    let mut outcome = first;
    // The step budget a compaction was last asked at: two in a row at the
    // same budget mean the prompt still does not fit, and no step was taken.
    let mut last_asked = None;
    while let Ok(RunOutcome::NeedsCompaction { steps_left }) = outcome {
        if last_asked == Some(steps_left) {
            actor.state = AgentState::Idle;
            return Err(crate::error::AgentError::Compaction(format!(
                "{} (the prompt still does not fit after a compaction)",
                compact::CAPACITY_HINT
            )));
        }
        last_asked = Some(steps_left);
        let cancel = actor.cancel.clone();
        match compact::run_automatic(actor, status, sequencer, &cancel).await {
            Ok(_) => outcome = continue_run(actor, observe, steps_left).await,
            Err(None) => {
                actor.state = AgentState::Idle;
                return Ok(RunOutcome::Interrupted);
            }
            Err(Some(why)) => {
                actor.state = AgentState::Idle;
                return Err(crate::error::AgentError::Compaction(why));
            }
        }
    }
    outcome
}

/// Publishes the end of a permission request, correlated to the task.
pub(crate) fn publish_resolved(
    sequencer: &Sequencer,
    task_id: Option<Ulid>,
    request: &crate::agent::permissions::PermissionRequest,
    resolution: PermissionResolution,
) {
    let mut candidate = Candidate::new(
        EventKind::PermissionResolved,
        to_value(&PermissionResolved {
            child_session_id: None,
            request_id: request.id,
            tool_call_id: request.tool_call_id.clone(),
            resolution,
        }),
    );
    if let Some(task) = task_id {
        candidate = candidate.with_correlation_id(task);
    }
    let _ = sequencer.publish(candidate);
}

/// The next compaction request; waits forever once there is no channel.
async fn recv_control(
    control: &mut Option<tokio::sync::mpsc::UnboundedReceiver<Control>>,
) -> Option<Control> {
    match control {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

type SharedHook = Arc<Mutex<TurnEndHook>>;

/// Hands the transcript to the turn-end hook THROUGH the sequencer: the
/// sequencer runs the hook after every event this turn already published
/// has been emitted, so the store's FIFO writer sees the snapshot behind the
/// turn's events, never before them. The hook is only a channel send, so the
/// sequencer never waits on SQLite. If the session is already closed no
/// more events can be written and the hook runs here directly.
fn snapshot_transcript(
    sequencer: &Sequencer,
    hook: &SharedHook,
    transcript: &[crate::provider::openai::ChatMessage],
) {
    fn run(hook: &SharedHook, transcript: &[crate::provider::openai::ChatMessage]) {
        let guard = match hook.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        (guard)(transcript);
    }
    let (queued_hook, owned) = (Arc::clone(hook), transcript.to_vec());
    let queued = sequencer.after_events(Box::new(move || run(&queued_hook, &owned)));
    if queued.is_err() {
        run(hook, transcript);
    }
}

fn permission_requested(
    request: &crate::agent::permissions::PermissionRequest,
) -> serde_json::Value {
    to_value(&crate::bus::event::PermissionRequested {
        child_session_id: None,
        request_id: request.id,
        tool_call_id: request.tool_call_id.clone(),
        tool: request.tool.clone(),
        arguments: request.arguments.clone(),
        scope: request.scope.clone(),
        agent: request.agent.clone(),
        reason: request.reason.clone(),
    })
}

fn emit_turn_end(
    turn: &mut TurnPublisher,
    turn_id: Ulid,
    outcome: TurnEndOutcome,
    usage: Option<crate::provider::Usage>,
    duration_ms: u64,
    error: Option<TurnError>,
) {
    turn.publish(
        EventKind::TurnEnd,
        to_value(&TurnEnd {
            turn_id,
            actor_id: ROOT_ACTOR_ID.to_string(),
            outcome,
            usage,
            duration_ms,
            error,
        }),
    );
}

fn emit_agent_state(turn: &mut TurnPublisher, to: &str) {
    turn.publish(
        EventKind::AgentState,
        to_value(&AgentStateChanged {
            actor_id: ROOT_ACTOR_ID.to_string(),
            from: "running".to_string(),
            to: to.to_string(),
            reason: None,
        }),
    );
}

fn to_value<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Bounded, secret-free error summary for the wire.
fn bounded(message: &str, cap: usize) -> String {
    if message.chars().count() <= cap {
        message.to_string()
    } else {
        let out: String = message.chars().take(cap).collect();
        format!("{out}…")
    }
}

/// Coarse typed error-code vocabulary for `turn.end{error}` payloads.
fn error_code(err: &AgentError) -> String {
    match err {
        AgentError::Provider(_) => "provider".to_string(),
        AgentError::Tool(_) => "tool".to_string(),
        AgentError::Config(_) => "config".to_string(),
        AgentError::Sse(_) => "sse".to_string(),
        AgentError::Bus(_) => "bus".to_string(),
        AgentError::Server(_) => "server".to_string(),
        AgentError::Compaction(_) => "compaction".to_string(),
    }
}

/// Session teardown — the ONLY path that fires `session.end`. Permanent by
/// construction: the sequencer task exits behind the fence and the runtime
/// loop returns, so this session never drives the actor again.
fn teardown(actor: &mut AgentActor, sequencer: &Sequencer) {
    actor.state = AgentState::Stopped;
    let reason = if actor.cancel.is_cancelled() {
        CANCEL_REASON
    } else {
        END_OF_STREAM_REASON
    };
    let _ = sequencer.close_session(reason);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::command::{Command, ContentBlock, DeliveryMode};
    use crate::bus::event::EventEnvelope;
    use crate::provider::config::{ProviderConfig, ReasoningHistory};
    use crate::provider::test_support::{http_sse, sse_body};
    use crate::tools::{Registry, ToolContext};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    // ---- mock provider (same technique as the loop.rs tests) ----

    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let s = String::from_utf8_lossy(&buf);
            if let Some(idx) = s.find("\r\n\r\n") {
                let header = &s[..idx];
                let content_len = header
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let body_start = idx + 4;
                if buf.len() >= body_start + content_len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    struct MockServer {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn spawn_mock(responses: Vec<Vec<u8>>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let req_clone = requests.clone();
        tokio::spawn(async move {
            for resp in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req = read_request(&mut sock).await;
                req_clone.lock().unwrap().push(req);
                let _ = sock.write_all(&resp).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        MockServer {
            url: format!("http://{addr}/v1"),
            requests,
        }
    }

    // ---- fixture helpers ----

    fn registry_all() -> Registry {
        let mut r = Registry::new();
        r.register(Box::new(crate::tools::native::ReadTool));
        r.register(Box::new(crate::tools::bash::BashTool));
        r.register(Box::new(crate::tools::control::AskTool));
        r
    }

    fn make_actor(base_url: String, cancel: CancellationToken) -> AgentActor {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        AgentActor::new(
            registry_all(),
            ToolContext {
                workspace: ws.clone(),
                writable_roots: vec![ws],
                readable_roots: Vec::new(),
            },
            ProviderConfig {
                base_url,
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 2000,
                request_timeout_ms: 600_000,
                catalog_format: None,
            },
            "test-key".to_string(),
            "test-model".to_string(),
            ReasoningHistory::Preserve,
            Some(256),
            cancel,
        )
    }

    fn text_command(session_id: Ulid, content: &str) -> Command {
        Command {
            command_id: Ulid::new(),
            session_id,
            source_actor_id: "user:cli".into(),
            target_actor_id: ROOT_ACTOR_ID.into(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text {
                text: content.into(),
            }],
            created_at: std::time::SystemTime::now(),
        }
    }

    /// Scripted harness: a real sequencer over a real SessionBus, a real
    /// runtime task, and a live subscriber registered BEFORE any command is
    /// sent — the collected stream is the session's ordered event log
    /// (`session.start` belongs to the session manager, which this
    /// harness does not use, so turn.start is seq 0 here).
    struct Harness {
        inbox_tx: mpsc::Sender<Command>,
        session_cancel: CancellationToken,
        session_id: Ulid,
        live: mpsc::Receiver<EventEnvelope>,
        handle: tokio::task::JoinHandle<AgentActor>,
    }

    async fn harness(server_url: String) -> Harness {
        harness_with_steps(server_url, None).await
    }

    async fn harness_with_steps(server_url: String, max_steps: Option<usize>) -> Harness {
        let (inbox_tx, inbox) = ActorInbox::channel();
        let session_cancel = CancellationToken::new();
        let mut actor = make_actor(server_url, CancellationToken::new());
        if let Some(n) = max_steps {
            actor.max_steps = n;
        }
        let session_id = Ulid::new();
        let sequencer = Sequencer::spawn(session_id, crate::bus::SessionBus::default(), 0);
        let live = sequencer.subscribe_since(None).await.unwrap().live.unwrap();
        let handle = spawn(
            session_id,
            actor,
            inbox,
            sequencer,
            session_cancel.clone(),
            None,
        );
        Harness {
            inbox_tx,
            session_cancel,
            session_id,
            live,
            handle,
        }
    }

    impl Harness {
        fn send(&self, content: &str) {
            self.inbox_tx
                .try_send(text_command(self.session_id, content))
                .unwrap();
        }

        async fn take_actor(self) -> AgentActor {
            // Drop the senders FIRST: the runtime exits its next_batch wait
            // only when they are gone (or the session token fires).
            drop(self.inbox_tx);
            drop(self.session_cancel);
            self.handle.await.unwrap()
        }
    }

    fn kind_of(env: &EventEnvelope) -> &str {
        env.kind.as_str()
    }

    fn assert_monotonic(envelopes: &[EventEnvelope]) -> Vec<&str> {
        let kinds: Vec<&str> = envelopes.iter().map(kind_of).collect();
        let seqs: Vec<u64> = envelopes.iter().map(|e| e.seq).collect();
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "seqs must be strictly monotonic + gapless: {seqs:?}"
        );
        kinds
    }

    /// Drains the live queue until the predicate matches, absorbing all
    /// events already queued in the SAME batch (same-tick events land
    /// together; the predicate stops at the first match).
    async fn drain_until(
        live: &mut mpsc::Receiver<EventEnvelope>,
        pred: impl Fn(&EventEnvelope) -> bool,
    ) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        while let Some(env) = tokio::time::timeout(Duration::from_secs(5), live.recv())
            .await
            .unwrap()
        {
            let matched = pred(&env);
            out.push(env);
            if matched {
                // Settle: events sequenced in the same tick but still in the
                // sequencer's ingress/fan-out queue land here too.
                tokio::time::sleep(Duration::from_millis(50)).await;
                out.extend(drain_ready(live));
                return out;
            }
        }
        out
    }

    /// Drains whatever is already queued, non-blocking.
    fn drain_ready(live: &mut mpsc::Receiver<EventEnvelope>) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        while let Ok(env) = live.try_recv() {
            out.push(env);
        }
        out
    }

    // ---- (a) Done run: llm.response(final) -> turn.end{done} -> task.done -> agent.state{Idle} ----

    #[tokio::test]
    async fn done_run_emits_final_response_turn_end_task_done_agent_idle_in_order() {
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[done]))]).await;
        let mut h = harness(server.url.clone()).await;
        h.send("say done");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "done"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);
        assert_eq!(
            kinds,
            vec![
                "turn.start",
                "llm.delta",
                "llm.response",
                "turn.end",
                "task.done",
                "agent.state",
            ]
        );
        assert_eq!(envelopes[2].payload["message"]["text"], "all done");
        assert!(envelopes[2].payload["turn_id"].is_string());
        assert_eq!(envelopes[3].payload["outcome"], "done");
        assert_eq!(envelopes[4].payload["status"], "done");
        assert!(envelopes[4].payload["result_message_id"].is_string());
        assert_eq!(envelopes[5].payload["to"], "idle");

        // correlation_id == task_id across every event of the task.
        let task_id = envelopes[4].correlation_id.unwrap();
        for env in &envelopes {
            assert_eq!(env.correlation_id, Some(task_id));
        }
        h.take_actor().await;
    }

    // ---- (b) Yielded run: agent.yield -> turn.end{yielded} -> agent.state{Yielded}, NO task.done ----

    #[tokio::test]
    async fn yielded_run_emits_yield_turn_end_agent_yielded_and_never_task_done() {
        let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"insert SD card\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"thanks, continuing"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[ask])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut h = harness(server.url.clone()).await;
        h.send("need my input");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "agent.state" && e.payload["to"] == "yielded"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);
        assert_eq!(
            kinds,
            vec![
                "turn.start",
                "llm.delta",
                "llm.delta",
                "llm.response",
                "agent.yield",
                "turn.end",
                "agent.state",
            ],
            "two deltas: the scripted chunk streams a tool-call start AND its args"
        );
        assert_eq!(envelopes[4].payload["question"], "insert SD card");
        assert_eq!(envelopes[5].payload["outcome"], "yielded");
        assert_eq!(envelopes[6].payload["to"], "yielded");
        assert!(
            !kinds.contains(&"task.done"),
            "task.done NEVER fires on Yielded"
        );
        let yielded_task_id = envelopes[0].correlation_id;

        // Resume completes the SAME task: the pending ask is answered by the
        // first queued command, the run finishes, task.done fires ONCE with
        // the SAME task_id (stable across yield/resume).
        h.send("done, it is visible");
        let after = drain_until(&mut h.live, |e| kind_of(e) == "task.done").await;
        let resumed = after
            .iter()
            .find(|e| kind_of(e) == "task.done")
            .expect("resumed run ends in task.done");
        assert_eq!(
            resumed.correlation_id, yielded_task_id,
            "task_id STABLE across yield/resume"
        );
        h.take_actor().await;
    }

    // ---- (c) on_tool fires around each dispatch ----

    #[tokio::test]
    async fn tool_events_bracket_each_dispatch_in_order() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut h = harness(server.url.clone()).await;
        h.send("echo");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "done"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);

        let pre = envelopes
            .iter()
            .find(|e| kind_of(e) == "tool.pre_call")
            .expect("pre_call emitted");
        let post = envelopes
            .iter()
            .find(|e| kind_of(e) == "tool.post_call")
            .expect("post_call emitted");
        let response = envelopes
            .iter()
            .find(|e| kind_of(e) == "llm.response")
            .unwrap();
        assert_eq!(pre.payload["tool_call_id"], "call_1");
        assert_eq!(pre.payload["name"], "bash");
        assert_eq!(pre.payload["source"], "native");
        assert_eq!(pre.payload["args"]["command"], "echo hi");
        assert_eq!(post.payload["tool_call_id"], "call_1");
        assert_eq!(post.payload["name"], "bash");
        assert_eq!(post.payload["status"], "ok");
        assert!(
            response.seq < pre.seq && pre.seq < post.seq,
            "llm.response < tool.pre_call < tool.post_call: seqs {} < {} < {}",
            response.seq,
            pre.seq,
            post.seq
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == "llm.response").count(),
            2,
            "one llm.response per model response: {kinds:?}"
        );
        assert!(kinds.contains(&"turn.end"));
        h.take_actor().await;
    }

    #[test]
    fn with_duration_merges_into_objects_and_leaves_other_shapes() {
        use serde_json::{json, Value};
        assert_eq!(
            with_duration(Value::Null, Some(7)),
            json!({"duration_ms": 7})
        );
        assert_eq!(
            with_duration(json!({"exit_code": 1}), Some(7)),
            json!({"exit_code": 1, "duration_ms": 7})
        );
        assert_eq!(with_duration(json!("raw"), Some(7)), json!("raw"));
        assert_eq!(with_duration(json!([1]), Some(7)), json!([1]));
        assert_eq!(with_duration(Value::Null, None), Value::Null);
    }

    #[tokio::test]
    async fn post_call_metadata_carries_duration_ms() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut h = harness(server.url.clone()).await;
        h.send("echo");
        let envelopes = drain_until(&mut h.live, |e| kind_of(e) == "turn.end").await;
        let post = envelopes
            .iter()
            .find(|e| kind_of(e) == "tool.post_call")
            .expect("post_call emitted");
        assert!(
            post.payload["metadata"]["duration_ms"].is_u64(),
            "metadata: {}",
            post.payload["metadata"]
        );
        h.take_actor().await;
    }

    // ---- (d) StepBudgetExhausted -> agent.yield{kind:budget} -> turn.end{yielded}; never leaks ----

    #[tokio::test]
    async fn step_budget_exhausted_surfaces_as_yielded_never_the_internal_variant() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[toolcall])),
        ])
        .await;
        let mut h = harness_with_steps(server.url.clone(), Some(2)).await;
        h.send("loop forever");

        let mut envelopes = drain_until(&mut h.live, |e| kind_of(e) == "agent.state").await;
        envelopes.extend(drain_ready(&mut h.live));

        let kinds = assert_monotonic(&envelopes);
        let yield_env = envelopes
            .iter()
            .find(|e| kind_of(e) == "agent.yield")
            .expect("the budget outcome yields");
        assert_eq!(yield_env.payload["kind"], "budget");
        assert_eq!(kinds.iter().filter(|k| **k == "agent.yield").count(), 1);
        let turn_end = envelopes
            .iter()
            .find(|e| kind_of(e) == "turn.end")
            .expect("turn.end present");
        assert_eq!(turn_end.payload["outcome"], "yielded");
        assert_eq!(turn_end.seq, yield_env.seq + 1, "agent.yield -> turn.end");
        assert_eq!(
            envelopes
                .iter()
                .find(|e| kind_of(e) == "agent.state")
                .expect("agent.state after budget yield")
                .payload["to"],
            "yielded"
        );
        assert!(
            !kinds.contains(&"task.done"),
            "no task.done on budget exhaustion: {kinds:?}"
        );
        assert!(
            envelopes
                .iter()
                .all(|e| e.payload.get("outcome")
                    != Some(&serde_json::json!("step_budget_exhausted"))),
            "internal variant NEVER on the wire"
        );
        h.take_actor().await;
    }

    // ---- (e) Err run: turn.end{error} -> agent.state{Idle}; returned actor.state == Idle before teardown ----

    #[tokio::test]
    async fn error_run_emits_turn_end_error_then_agent_idle_and_actor_ends_idle() {
        let body = br#"{"error":{"message":"model exploded"}}"#;
        let resp = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        let server = spawn_mock(vec![resp.into_bytes()]).await;
        let mut h = harness(server.url.clone()).await;
        h.send("trigger failure");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "error"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);
        let turn_end = envelopes
            .iter()
            .find(|e| kind_of(e) == "turn.end")
            .expect("turn.end present");
        assert_eq!(turn_end.payload["outcome"], "error");
        assert_eq!(turn_end.payload["error"]["code"], "provider");
        assert!(turn_end.payload["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("model exploded"));
        let state = envelopes
            .iter()
            .find(|e| kind_of(e) == "agent.state")
            .expect("agent.state present");
        assert_eq!(state.payload["to"], "idle");
        assert!(!kinds.contains(&"task.done"));

        // Drive teardown and take the actor: it ends Stopped (teardown mark),
        // and the LAST observed pre-teardown state was Idle (set by the Err
        // arm BEFORE the events).
        let actor = h.take_actor().await;
        assert_eq!(actor.state, AgentState::Stopped);
    }

    // ---- (f) Cancel mid-turn via the session token -> ordered terminal events -> permanent teardown ----

    #[tokio::test]
    async fn session_cancel_mid_turn_ends_turn_then_tears_down_permanently() {
        // A stream that never finishes: the runtime parks inside run_agent.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stream_started_tx, stream_started_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let head =
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = sock.write_all(head).await;
            let first =
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial \"}}]}\n\n";
            let _ = sock.write_all(first).await;
            let _ = sock.flush().await;
            let _ = stream_started_tx.send(());
            // Never [DONE]; hold the connection until the client goes away.
            let mut sink = [0u8; 1];
            let _ = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut sink)).await;
        });
        let url = format!("http://{addr}/v1");

        let mut h = harness(url).await;
        h.send("will be interrupted");
        tokio::time::timeout(Duration::from_secs(5), stream_started_rx)
            .await
            .unwrap()
            .unwrap();
        h.session_cancel.cancel();

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "interrupted"
        })
        .await;
        let mut all = envelopes;
        all.extend(drain_ready(&mut h.live));
        let kinds = assert_monotonic(&all);
        assert!(
            kinds.contains(&"turn.end")
                && kinds.contains(&"agent.state")
                && kinds.contains(&"session.end"),
            "ordered terminal events after cancel: {kinds:?}"
        );
        let end = all
            .iter()
            .find(|e| kind_of(e) == "session.end")
            .expect("session.end sequenced by teardown");
        assert_eq!(end.seq, all.iter().map(|e| e.seq).max().unwrap());
        assert_eq!(end.payload["reason"], "cancel");

        // Clone the sender BEFORE take_actor consumes the harness, so the
        // late send below really targets the torn-down session.
        let late_tx = h.inbox_tx.clone();
        let actor = h.take_actor().await;
        assert_eq!(actor.state, AgentState::Stopped);

        // PERMANENT teardown: a later try_send fails Closed — NOT "still
        // accepts".
        let err = late_tx
            .try_send(text_command(Ulid::new(), "too late"))
            .unwrap_err();
        assert!(
            matches!(err, mpsc::error::TrySendError::Closed(_)),
            "inbox must be torn down permanently: {err:?}"
        );
    }

    // ---- (g) direct-vs-runtime golden: byte-identical provider request bodies ----

    #[tokio::test]
    async fn direct_vs_runtime_request_bodies_are_byte_identical() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;

        // Direct drive (the Phase-1 path), two requests captured.
        let server_direct = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut actor = make_actor(server_direct.url.clone(), CancellationToken::new());
        actor.push_user("run echo hi");
        let mut noop = |_e: AgentEvent<'_>| {};
        let direct = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(direct, RunOutcome::Done(_)));

        // Runtime drive: same scripted responses, prompt via the bus.
        let server_rt = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut h = harness(server_rt.url.clone()).await;
        h.send("run echo hi");
        let _ = drain_until(&mut h.live, |e| kind_of(e) == "task.done").await;
        h.take_actor().await;

        let direct_bodies: Vec<String> = server_direct
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| split_body(r))
            .collect();
        let rt_bodies: Vec<String> = server_rt
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| split_body(r))
            .collect();
        assert_eq!(direct_bodies.len(), 2);
        assert_eq!(rt_bodies.len(), 2);
        assert_eq!(direct_bodies, rt_bodies, "wire bodies byte-identical");
    }

    fn split_body(request: &str) -> String {
        request
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string()
    }
}
