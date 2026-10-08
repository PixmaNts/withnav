//! Manual compaction inside the runtime: the one place that asks for a
//! summary (`compaction::prepare`) and installs it (`commit_compaction`).
//!
//! A request arrives on the control channel of [`RuntimeStatus`]. Between
//! turns the runtime runs it at once and answers with the result; while a
//! turn runs it answers [`CompactReply::Deferred`] and runs it when the turn
//! ends. The runtime owns the actor, so nothing else can touch the transcript
//! while the summary is being written.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::RuntimeStatus;
use crate::agent::actor::AgentActor;
use crate::agent::compaction::CompactionError;
use crate::agent::compaction::{self, Aim, Compaction, PrepareInput};
use crate::bus::event::{CommandResult, CompactionReason, ContextCompacted, EventKind};
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::provider::openai::ChatMessage;

/// Why a compaction that a stop cut short was not done.
pub(super) const CANCELLED: &str = "the session is stopping: nothing was compacted";

/// Longest focus text `/compact` accepts, in characters.
pub const FOCUS_MAX_CHARS: usize = 1000;

/// Saves the transcript and the compaction in one transaction and returns
/// once they are durable. Supplied by the session service, so the bus never
/// imports the store.
pub type CheckpointSaver = Arc<
    dyn Fn(Vec<ChatMessage>, Compaction) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

/// What a finished compaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    pub generation: u64,
    pub through_message: usize,
    /// Tool outputs the prompt now shows as markers.
    pub pruned: usize,
    pub estimate_before: u64,
    pub estimate_after: u64,
}

impl Compacted {
    /// The `data` of the command result: `applies` says when it took effect.
    pub fn wire(&self, applies: &str) -> serde_json::Value {
        serde_json::json!({
            "generation": self.generation,
            "through_message": self.through_message,
            "pruned": self.pruned,
            "estimate_before": self.estimate_before,
            "estimate_after": self.estimate_after,
            "applies": applies,
        })
    }
}

/// The `data` of the result that accepts a request for after the running turn.
pub fn deferred_wire() -> serde_json::Value {
    serde_json::json!({ "accepted": true, "applies": "after_turn" })
}

/// The runtime's answer to a compaction request. The runtime has already
/// published the request's `command.result` under its operation id when it
/// answers, so the answer only tells the caller what to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactReply {
    /// Done now.
    Done(Compacted),
    /// A turn is running: it runs when that turn ends.
    Deferred,
    /// It cannot run; the text says why.
    Refused(String),
}

/// One request on the control channel.
#[derive(Debug)]
pub struct CompactRequest {
    pub focus: Option<String>,
    /// The `command.result` operation this request belongs to: a later
    /// failure or cancellation is published under the same id.
    pub operation_id: ulid::Ulid,
    pub reply: oneshot::Sender<CompactReply>,
}

/// What the session service asks of the runtime between turns.
#[derive(Debug)]
pub enum Control {
    Bypass(super::bypass::BypassRequest),
    Delivery,
    Continue,
    Steer,
    Interrupt(ulid::Ulid),
    DeliverySteer,
    DeliveryInterrupt(ulid::Ulid),
    Compact(CompactRequest),
    Model(super::model::ModelRequest),
}

/// The two ends of the control channel, the saver and the model switcher.
#[derive(Debug)]
pub(super) struct Controls {
    pub(super) tx: mpsc::UnboundedSender<Control>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Control>>>,
    saver: Mutex<Option<SaverCell>>,
    switcher: Mutex<Option<super::model::SwitcherCell>>,
    #[cfg(test)]
    barrier: Mutex<Option<Arc<ResultBarrier>>>,
    #[cfg(test)]
    switch_barrier: Mutex<Option<Arc<ResultBarrier>>>,
}

/// Parks the runtime after a compaction ran and before its result is
/// published.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct ResultBarrier {
    pub(crate) reached: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

struct SaverCell(CheckpointSaver);

impl std::fmt::Debug for SaverCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CheckpointSaver")
    }
}

impl Default for Controls {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            saver: Mutex::new(None),
            switcher: Mutex::new(None),
            #[cfg(test)]
            barrier: Mutex::new(None),
            #[cfg(test)]
            switch_barrier: Mutex::new(None),
        }
    }
}

impl Controls {
    pub(super) fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<Control>> {
        match self.rx.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }

    pub(super) fn set_saver(&self, saver: CheckpointSaver) {
        *match self.saver.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        } = Some(SaverCell(saver));
    }

    pub(super) fn set_switcher(&self, switcher: super::model::ModelSwitcher) {
        *match self.switcher.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        } = Some(super::model::SwitcherCell(switcher));
    }

    pub(super) fn switcher(&self) -> Option<super::model::ModelSwitcher> {
        match self.switcher.lock() {
            Ok(guard) => guard.as_ref().map(|c| Arc::clone(&c.0)),
            Err(poisoned) => poisoned.into_inner().as_ref().map(|c| Arc::clone(&c.0)),
        }
    }

    #[cfg(test)]
    pub(super) fn arm_barrier(&self) -> Arc<ResultBarrier> {
        let barrier = Arc::new(ResultBarrier::default());
        *self.barrier.lock().expect("barrier") = Some(barrier.clone());
        barrier
    }

    #[cfg(test)]
    pub(super) fn arm_switch_barrier(&self) -> Arc<ResultBarrier> {
        let barrier = Arc::new(ResultBarrier::default());
        *self.switch_barrier.lock().expect("barrier") = Some(barrier.clone());
        barrier
    }

    /// Test seam: parks a model switch after its selection is installed and
    /// before the actor is moved to the new model.
    #[cfg(test)]
    pub(super) async fn before_switch_swap(&self) {
        let barrier = self
            .switch_barrier
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(barrier) = barrier {
            barrier.reached.notify_one();
            barrier.release.notified().await;
        }
    }

    #[cfg(not(test))]
    pub(super) async fn before_switch_swap(&self) {}

    #[cfg(test)]
    pub(super) async fn before_result(&self) {
        let barrier = self.barrier.lock().ok().and_then(|mut guard| guard.take());
        if let Some(barrier) = barrier {
            barrier.reached.notify_one();
            barrier.release.notified().await;
        }
    }

    #[cfg(not(test))]
    pub(super) async fn before_result(&self) {}

    fn saver(&self) -> Option<CheckpointSaver> {
        match self.saver.lock() {
            Ok(guard) => guard.as_ref().map(|c| Arc::clone(&c.0)),
            Err(poisoned) => poisoned.into_inner().as_ref().map(|c| Arc::clone(&c.0)),
        }
    }
}

/// Requests accepted while a turn ran, each with its operation id.
pub(super) type Waiting = Vec<(ulid::Ulid, Option<String>)>;

/// Folds the focus texts of requests that waited for one turn into one.
pub(super) fn merge_focus(waiting: &Waiting) -> Option<String> {
    waiting.iter().rev().find_map(|(_, focus)| focus.clone())
}

/// Why a compaction did not happen.
#[derive(Debug)]
pub(super) enum Failure {
    /// The plan or the summary was refused: nothing to cut, the recent turns
    /// do not fit, the summary was unusable, the provider failed, or a stop
    /// cancelled it.
    Compaction(CompactionError),
    /// Anything else, already worded.
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compaction(e) => write!(f, "{e}"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

/// A summary written and validated, not yet installed.
pub(super) struct Summarized {
    pub(super) compaction: Compaction,
    pub(super) done: Compacted,
}

/// What a summary is for.
#[derive(Debug, Clone, Copy)]
pub(super) struct Goal<'a> {
    pub(super) focus: Option<&'a str>,
    pub(super) aim: Aim,
    /// The input budget the prompt must fit afterwards.
    pub(super) destination_budget: u64,
    /// The input budget of the model that writes the summary.
    pub(super) source_budget: u64,
}

/// Asks for the summary of the conversation of `actor`, written by the
/// session's own model and variant, for a prompt that must fit
/// `destination_budget` afterwards. `transcript` is the history as the
/// destination would receive it (same indices as the actor's). Installs
/// nothing and saves nothing.
pub(super) async fn summarize(
    actor: &AgentActor,
    status: &RuntimeStatus,
    cancel: &CancellationToken,
    transcript: &[ChatMessage],
    goal: Goal<'_>,
) -> Result<Summarized, Failure> {
    let role = actor.role_message();
    let tools = actor.effective_tool_schemas();
    summarize_with_components(
        actor,
        status,
        cancel,
        transcript,
        goal,
        actor.components(role.as_deref(), &tools),
    )
    .await
}

/// The model that writes a summary: the session's own for `/compact`, and for
/// a switch first the old one, then the destination when the old one fails.
pub(super) struct Writer<'a> {
    pub(super) provider: &'a crate::provider::config::ProviderConfig,
    pub(super) api_key: String,
    pub(super) model: &'a str,
    pub(super) variant: Option<&'a crate::provider::config::Variant>,
    pub(super) max_tokens: Option<u64>,
}

pub(super) async fn summarize_with_components(
    actor: &AgentActor,
    status: &RuntimeStatus,
    cancel: &CancellationToken,
    transcript: &[ChatMessage],
    goal: Goal<'_>,
    components: compaction::Components<'_>,
) -> Result<Summarized, Failure> {
    let variant = status.active_variant().map(|(_, variant)| variant);
    let writer = Writer {
        provider: &actor.provider,
        api_key: actor.api_key.clone(),
        model: &actor.model,
        variant: variant.as_ref(),
        max_tokens: actor.max_tokens,
    };
    summarize_written_by(actor, writer, cancel, transcript, goal, components).await
}

/// [`summarize_with_components`] with the summary written by `writer`.
pub(super) async fn summarize_written_by(
    actor: &AgentActor,
    writer: Writer<'_>,
    cancel: &CancellationToken,
    transcript: &[ChatMessage],
    goal: Goal<'_>,
    components: compaction::Components<'_>,
) -> Result<Summarized, Failure> {
    let prepared = compaction::prepare(
        PrepareInput {
            provider: writer.provider,
            api_key: writer.api_key,
            model: writer.model,
            variant: writer.variant,
            max_tokens: writer.max_tokens,
            transcript,
            current: actor.compaction.as_ref(),
            components,
            destination_budget: goal.destination_budget,
            aim: goal.aim,
            source_budget: goal.source_budget,
            focus: goal.focus,
        },
        cancel,
    )
    .await
    .map_err(Failure::Compaction)?;
    let installed = prepared.compaction;
    let done = Compacted {
        generation: installed.checkpoint.generation,
        through_message: installed.checkpoint.through_message,
        pruned: installed.pruned_message_indices.len(),
        estimate_before: prepared.estimated_before,
        estimate_after: prepared.estimated_after,
    };
    Ok(Summarized {
        compaction: installed,
        done,
    })
}

/// Summarizes the conversation of `actor` and installs the result. Nothing
/// is installed unless the checkpoint was saved. Once it is, the stream gets
/// a durable `context.compacted` and a live `context.updated` with the
/// recomputed breakdown.
pub(super) async fn run(
    actor: &mut AgentActor,
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    cancel: &CancellationToken,
    focus: Option<&str>,
) -> Result<Compacted, String> {
    if actor.has_pending() {
        return Err(
            "the agent is waiting for an answer or a decision: reply to its question or \
             decide on its permission request first, then compact"
                .to_string(),
        );
    }
    let budget = status
        .context_snapshot()
        .map(|snapshot| snapshot.input_budget)
        .ok_or_else(|| "this session has no context budget to compact against".to_string())?;
    // From here until the checkpoint is committed or given up, the agent and
    // the variant cannot change: the summary, the budget check and the gauge
    // all measure the same role.
    let _preparing = status.selection.begin_compaction().await;
    if actor.role.is_some() {
        actor.role = Some(status.selection.current());
    }
    let transcript = actor.planning_transcript(actor.reasoning_floor);
    let summarized = summarize(
        actor,
        status,
        cancel,
        &transcript,
        Goal {
            focus,
            aim: Aim::Shrink,
            destination_budget: budget,
            source_budget: budget,
        },
    )
    .await
    .map_err(failure_text)?;
    install(
        actor,
        status,
        sequencer,
        cancel,
        summarized,
        CompactionReason::Manual,
    )
    .await
    .map_err(failure_text)
}

/// A failure as the text a command reports; a stop is the usual stop text.
fn failure_text(failure: Failure) -> String {
    match failure {
        Failure::Compaction(CompactionError::Cancelled) => CANCELLED.to_string(),
        other => other.to_string(),
    }
}

/// Saves the checkpoint (acknowledged), installs it and announces it. A
/// session that is stopping commits nothing: the stop wins.
pub(super) async fn install(
    actor: &mut AgentActor,
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    cancel: &CancellationToken,
    summarized: Summarized,
    reason: CompactionReason,
) -> Result<Compacted, Failure> {
    let saver = status
        .controls
        .saver()
        .ok_or_else(|| Failure::Other("this session cannot save a compaction".to_string()))?;
    if cancel.is_cancelled() {
        return Err(Failure::Compaction(CompactionError::Cancelled));
    }
    let Summarized { compaction, done } = summarized;
    actor
        .commit_compaction(compaction, |transcript, compaction| {
            saver(transcript, compaction)
        })
        .await
        .map_err(|e| Failure::Other(format!("the compaction could not be saved: {e}")))?;
    publish_compacted(actor, status, sequencer, &done, reason);
    Ok(done)
}

/// What an automatic compaction before a request could not do, worded for the
/// turn's error.
pub(super) const CAPACITY_HINT: &str = "the conversation outgrew the model's context window: \
    /compact cannot help either; use a model with a larger context window, or start a new \
    conversation";

/// Compacts the conversation of `actor` because the next request would not
/// fit its input budget: the summary is written by the session's own model
/// and variant, aiming below 70% of the budget (the whole budget when the
/// recent turns that must stay verbatim need more). The caller carries the
/// same turn on. `Err` is the text of the turn's error, or `None` when a
/// stop cancelled it.
pub(super) async fn run_automatic(
    actor: &mut AgentActor,
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    cancel: &CancellationToken,
) -> Result<Compacted, Option<String>> {
    // Never while a human is being waited for: such a turn ends by asking, and
    // the cut would not be a safe one.
    if actor.has_pending() {
        return Err(Some(
            "automatic compaction does not run while the agent waits for an answer or a decision"
                .to_string(),
        ));
    }
    let budget = status
        .context_snapshot()
        .map(|snapshot| snapshot.input_budget)
        .ok_or_else(|| Some("this session has no context budget".to_string()))?;
    // The turn's error is cut short on the wire: the advice goes first.
    let capacity = |why: String| Some(format!("{CAPACITY_HINT} ({why})"));
    let _preparing = status.selection.begin_compaction().await;
    // The prompt as it is really sent: the reasoning of earlier models left out.
    let transcript = actor.planning_transcript(actor.reasoning_floor);
    // Cheapest first: keep the recent turns verbatim and aim below 70% of the
    // budget, then take the whole budget; only then give up the recent turns
    // and keep just the latest exchange, again below 70%, then the whole
    // budget. A plan that cannot fit costs no provider call.
    let attempts = [
        (Aim::Fit, budget / 10 * 7),
        (Aim::Fit, budget),
        (Aim::FitMidTurn, budget / 10 * 7),
        (Aim::FitMidTurn, budget),
    ];
    let mut last = None;
    let mut summarized = None;
    for (aim, destination_budget) in attempts {
        let goal = Goal {
            focus: None,
            aim,
            destination_budget,
            source_budget: budget,
        };
        match summarize(actor, status, cancel, &transcript, goal).await {
            Ok(done) => {
                summarized = Some(done);
                break;
            }
            Err(Failure::Compaction(CompactionError::Cancelled)) => return Err(None),
            Err(Failure::Compaction(
                e @ (CompactionError::Capacity { .. }
                | CompactionError::NothingToCompact
                | CompactionError::SummaryInputTooLarge { .. }),
            )) => last = Some(e),
            Err(e) => return Err(Some(format!("automatic compaction failed: {e}"))),
        }
    }
    let Some(summarized) = summarized else {
        return Err(capacity(match last {
            Some(CompactionError::NothingToCompact) | None => {
                "nothing older than the latest exchange is left to summarize".to_string()
            }
            Some(other) => other.to_string(),
        }));
    };
    install(
        actor,
        status,
        sequencer,
        cancel,
        summarized,
        CompactionReason::Automatic,
    )
    .await
    .map_err(|e| match e {
        Failure::Compaction(CompactionError::Cancelled) => None,
        other => Some(format!("automatic compaction failed: {other}")),
    })
}

fn publish_compacted(
    actor: &AgentActor,
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    done: &Compacted,
    reason: CompactionReason,
) {
    let notice = serde_json::to_value(ContextCompacted {
        generation: done.generation,
        through_message: done.through_message as u64,
        pruned: done.pruned as u64,
        estimate_before: done.estimate_before,
        estimate_after: done.estimate_after,
        reason: Some(reason),
    })
    .unwrap_or(serde_json::Value::Null);
    let _ = sequencer.publish(Candidate::new(EventKind::ContextCompacted, notice));
    if let Some(snapshot) = status.update_context(actor.context_breakdown()) {
        if let Ok(payload) = serde_json::to_value(&snapshot) {
            let _ = sequencer.publish(Candidate::new(EventKind::ContextUpdated, payload));
        }
    }
}

/// Publishes the `command.result` of a request that ran between turns and
/// then answers the caller. The result is on the stream, acknowledged, before
/// the reply: a stop that follows cannot fence the stream between the work
/// and its result. When the result cannot be published (the session closed)
/// the request is dropped unanswered, which the caller reports as stopping.
pub(super) async fn answer(
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    request: CompactRequest,
    outcome: Result<Compacted, String>,
) {
    let result = match &outcome {
        Ok(done) => CommandResult {
            operation_id: request.operation_id,
            name: "compact".to_string(),
            ok: true,
            data: Some(CommandResult::bound_data(done.wire("now"))),
            error: None,
        },
        Err(why) => CommandResult {
            operation_id: request.operation_id,
            name: "compact".to_string(),
            ok: false,
            data: None,
            error: Some(why.clone()),
        },
    };
    status.controls.before_result().await;
    let payload = serde_json::to_value(result).unwrap_or(serde_json::Value::Null);
    if sequencer
        .publish_acked(Candidate::new(EventKind::CommandResult, payload))
        .await
        .is_err()
    {
        return;
    }
    let _ = request.reply.send(match outcome {
        Ok(done) => CompactReply::Done(done),
        Err(why) => CompactReply::Refused(why),
    });
}

/// Accepts a request that arrived while a turn runs: its acceptance is
/// published, acknowledged, before the caller is told, so a later refusal or
/// cancellation of the same operation can never come first. Returns whether
/// the request was accepted.
pub(super) async fn accept_deferred(sequencer: &Sequencer, request: CompactRequest) -> bool {
    let payload = serde_json::to_value(CommandResult {
        operation_id: request.operation_id,
        name: "compact".to_string(),
        ok: true,
        data: Some(CommandResult::bound_data(deferred_wire())),
        error: None,
    })
    .unwrap_or(serde_json::Value::Null);
    if sequencer
        .publish_acked(Candidate::new(EventKind::CommandResult, payload))
        .await
        .is_err()
    {
        return false;
    }
    let _ = request.reply.send(CompactReply::Deferred);
    true
}

/// Tells the stream that the requests that waited for the turn ran, under
/// the operation id each command answered with.
pub(super) fn report_done(sequencer: &Sequencer, waiting: &Waiting, done: &Compacted) {
    for (operation_id, _) in waiting {
        let payload = serde_json::to_value(CommandResult {
            operation_id: *operation_id,
            name: "compact".to_string(),
            ok: true,
            data: Some(CommandResult::bound_data(done.wire("after_turn"))),
            error: None,
        })
        .unwrap_or(serde_json::Value::Null);
        let _ = sequencer.publish(Candidate::new(EventKind::CommandResult, payload));
    }
}

/// Tells the stream that each request that waited for the turn did not
/// happen, under the operation id its command answered with.
pub(super) fn report_failure(sequencer: &Sequencer, waiting: &Waiting, why: &str) {
    for (operation_id, _) in waiting {
        let payload = serde_json::to_value(CommandResult {
            operation_id: *operation_id,
            name: "compact".to_string(),
            ok: false,
            data: None,
            error: Some(why.to_string()),
        })
        .unwrap_or(serde_json::Value::Null);
        let _ = sequencer.publish(Candidate::new(EventKind::CommandResult, payload));
    }
}
