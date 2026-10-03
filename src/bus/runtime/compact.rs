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
use crate::agent::compaction::{self, Compaction, PrepareInput};
use crate::bus::event::{ContextCompacted, EventKind};
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::provider::openai::ChatMessage;

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

/// The runtime's answer to a compaction request.
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
    pub reply: oneshot::Sender<CompactReply>,
}

/// The two ends of the control channel and the saver.
#[derive(Debug)]
pub(super) struct Controls {
    pub(super) tx: mpsc::UnboundedSender<CompactRequest>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<CompactRequest>>>,
    saver: Mutex<Option<SaverCell>>,
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
        }
    }
}

impl Controls {
    pub(super) fn take_receiver(&self) -> Option<mpsc::UnboundedReceiver<CompactRequest>> {
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

    fn saver(&self) -> Option<CheckpointSaver> {
        match self.saver.lock() {
            Ok(guard) => guard.as_ref().map(|c| Arc::clone(&c.0)),
            Err(poisoned) => poisoned.into_inner().as_ref().map(|c| Arc::clone(&c.0)),
        }
    }
}

/// Folds the focus texts of requests that waited for one turn into one.
pub(super) fn merge_focus(waiting: &[Option<String>]) -> Option<String> {
    waiting.iter().rev().find_map(|f| f.clone())
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
    if actor.pending_ask_id.is_some() {
        return Err(
            "the agent is waiting for an answer: reply to its question first, then compact"
                .to_string(),
        );
    }
    let budget = status
        .context_snapshot()
        .map(|snapshot| snapshot.input_budget)
        .ok_or_else(|| "this session has no context budget to compact against".to_string())?;
    let saver = status
        .controls
        .saver()
        .ok_or_else(|| "this session cannot save a compaction".to_string())?;
    // Between turns the agent and the variant may have changed since the
    // last turn started: the summary runs as the session is now.
    if actor.role.is_some() {
        actor.role = Some(status.selection.current());
    }
    let variant = status.active_variant().map(|(_, variant)| variant);

    let role = actor.role_message();
    let tools = actor.registry.schemas();
    let prepared = compaction::prepare(
        PrepareInput {
            provider: &actor.provider,
            api_key: actor.api_key.clone(),
            model: &actor.model,
            variant: variant.as_ref(),
            max_tokens: actor.max_tokens,
            transcript: &actor.transcript,
            current: actor.compaction.as_ref(),
            components: actor.components(role.as_deref(), &tools),
            destination_budget: budget,
            source_budget: budget,
            focus,
        },
        cancel,
    )
    .await
    .map_err(|e| e.to_string())?;

    let installed = prepared.compaction;
    let done = Compacted {
        generation: installed.checkpoint.generation,
        through_message: installed.checkpoint.through_message,
        pruned: installed.pruned_message_indices.len(),
        estimate_before: prepared.estimated_before,
        estimate_after: prepared.estimated_after,
    };
    actor
        .commit_compaction(installed, |transcript, compaction| {
            saver(transcript, compaction)
        })
        .await
        .map_err(|e| format!("the compaction could not be saved: {e}"))?;
    publish_compacted(actor, status, sequencer, &done);
    Ok(done)
}

fn publish_compacted(
    actor: &AgentActor,
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    done: &Compacted,
) {
    let notice = serde_json::to_value(ContextCompacted {
        generation: done.generation,
        through_message: done.through_message as u64,
        pruned: done.pruned as u64,
        estimate_before: done.estimate_before,
        estimate_after: done.estimate_after,
    })
    .unwrap_or(serde_json::Value::Null);
    let _ = sequencer.publish(Candidate::new(EventKind::ContextCompacted, notice));
    if let Some(snapshot) = status.update_context(actor.context_breakdown()) {
        if let Ok(payload) = serde_json::to_value(&snapshot) {
            let _ = sequencer.publish(Candidate::new(EventKind::ContextUpdated, payload));
        }
    }
}

/// Tells the stream that a compaction that waited for a turn could not run.
pub(super) fn report_failure(sequencer: &Sequencer, why: String) {
    use crate::bus::event::{CommandResult, EventKind};
    let payload = serde_json::to_value(CommandResult {
        operation_id: ulid::Ulid::new(),
        name: "compact".to_string(),
        ok: false,
        data: None,
        error: Some(why),
    })
    .unwrap_or(serde_json::Value::Null);
    let _ = sequencer.publish(crate::bus::sequencer::Candidate::new(
        EventKind::CommandResult,
        payload,
    ));
}
