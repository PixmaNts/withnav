//! Switching the model of a live conversation inside the runtime, the way
//! `/compact` runs: between turns, at once when idle and after the running
//! turn otherwise. The runtime owns the actor, so nothing else touches the
//! conversation while the target is resolved, measured and installed.
//!
//! The session service supplies a [`ModelSwitch`] (resolve an alias, commit
//! the choice to the store), so the bus never imports the service or the
//! store. Nothing is installed unless the store acknowledged the choice.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use super::{RuntimeStatus, SelectedVariant};
use crate::agent::actor::AgentActor;
use crate::agent::context::{ContextLimits, ContextTracker};
use crate::bus::event::{
    CommandResult, EventEnvelope, EventKind, ModelChangeReason, ModelChanged, ModelIdentity,
};
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::provider::catalog::ModelResolution;
use crate::provider::config::{ProviderConfig, ReasoningHistory, Variants};
use std::sync::Arc as StdArc;

/// Why a switch refused while the agent waits for a human.
pub const PENDING_MODEL_ERROR: &str =
    "the agent is waiting for an answer or a decision: reply to its question or \
     decide on its permission request first, then switch model";

/// A boxed future, for the callbacks the service supplies.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What it takes to move the conversation to another alias.
#[derive(Debug, Clone)]
pub struct ModelTarget {
    pub warning: Option<String>,
    pub provenance: Option<crate::session::store::SelectionProvenance>,
    pub source: String,
    pub variant_source: String,
    pub agent: Option<Arc<crate::agent::definitions::AgentDefinition>>,
    pub override_value: Option<(Option<String>, crate::session::VariantChoice)>,
    pub alias: String,
    pub provider: ProviderConfig,
    pub api_key: String,
    /// The model name sent on the wire.
    pub model: String,
    pub reasoning_history: ReasoningHistory,
    pub max_tokens: Option<u64>,
    /// The target's own default variant.
    pub variant: SelectedVariant,
    /// The saved resolution the conversation keeps from now on.
    pub resolution: ModelResolution,
    /// The limits of the target, without the rules and skills sources.
    pub limits: ContextLimits,
    pub previous: ModelIdentity,
    pub new: ModelIdentity,
    /// Whether the target is the same provider, model, API and endpoint as
    /// the one in use.
    pub same_identity: bool,
    /// What the target offers as variants, and whether that is known.
    pub offer: Variants,
    pub offer_known: bool,
}

/// The store write of a switch: the target, the reasoning floor and the
/// sequenced `model.changed`.
pub type CommitFn = dyn Fn(
        ModelTarget,
        usize,
        Option<StagedCheckpoint>,
        Vec<EventEnvelope>,
        Install,
        CancellationToken,
    ) -> BoxFuture<Result<(), String>>
    + Send
    + Sync;

/// What the runtime installs for the new selection (the variant it applies and
/// the context gauge). The service runs it, once the store write is durable,
/// in the same step in which it installs its own view of the selection, so
/// readers never see one without the other. It must not block.
pub type Install = Box<dyn FnOnce() + Send>;

#[derive(Debug, Clone)]
pub struct StagedCheckpoint {
    pub transcript: Vec<crate::provider::openai::ChatMessage>,
    pub checkpoint: crate::agent::compaction::Compaction,
}

/// The service's side of a switch.
pub struct ModelSwitch {
    /// The alias the conversation talks to now.
    pub current_alias: Box<dyn Fn() -> String + Send + Sync>,
    /// Resolves an alias like a new session does: its window, its offer, its
    /// default variant. The refusal text says why it cannot be used.
    pub resolve:
        Box<dyn Fn(SelectionRequest) -> BoxFuture<Result<ModelTarget, String>> + Send + Sync>,
    /// The revision the next selection will carry.
    pub next_revision: Box<dyn Fn() -> u64 + Send + Sync>,
    /// Stores the choice, the reasoning floor and the sequenced `model.changed`
    /// envelope in ONE transaction and returns once it is durable. The
    /// service's own view of the model changes with it.
    pub commit: Box<CommitFn>,
}

/// The shared form of [`ModelSwitch`].
pub type ModelSwitcher = Arc<ModelSwitch>;

#[cfg(debug_assertions)]
type SwitchObserver = Box<dyn FnOnce(&AgentActor) + Send>;
#[cfg(debug_assertions)]
static SWITCH_OBSERVERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<Ulid, SwitchObserver>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Debug-only, one-shot observation of the actual actor after a successful
/// switch, before its completion is returned (not the service projection).
#[cfg(debug_assertions)]
pub fn observe_selection_install(
    operation: Ulid,
    observer: impl FnOnce(&AgentActor) + Send + 'static,
) {
    SWITCH_OBSERVERS
        .lock()
        .unwrap()
        .insert(operation, Box::new(observer));
}

pub(super) struct SwitcherCell(pub(super) ModelSwitcher);

impl std::fmt::Debug for SwitcherCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ModelSwitch")
    }
}

/// What a finished switch (or a no-op) did; `data` is the `data` of the
/// command result before `applies` is added.
#[derive(Debug, Clone, PartialEq)]
pub struct Switched {
    data: Value,
}

impl Switched {
    pub fn wire(&self, applies: &str) -> Value {
        let mut data = self.data.clone();
        data["applies"] = json!(applies);
        data
    }
}

/// The runtime's answer to a switch request. The runtime has already
/// published the request's `command.result` when it answers.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelReply {
    Done(Switched),
    /// A turn is running: it runs when that turn ends.
    Deferred,
    Refused(String),
}

/// One request on the control channel.
#[derive(Debug)]
pub struct ModelRequest {
    pub alias: String,
    pub selection: SelectionRequest,
    pub operation_id: Ulid,
    pub reply: oneshot::Sender<ModelReply>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectionRequest {
    pub alias: String,
    pub target_agent: Option<String>,
    pub requesting_agent: String,
    pub command: String,
    pub variant: Option<crate::session::VariantChoice>,
}

fn identity_json(identity: &ModelIdentity) -> Value {
    json!({
        "alias": identity.alias,
        "provider": identity.provider,
        "model": identity.model,
    })
}

/// Moves `actor` to the model `alias`. Nothing about the selection changes
/// unless the target fits and the store acknowledged the choice.
///
/// A conversation that is larger than the target's input budget is compacted
/// first, by the model it is on now (the target may be too small to read it),
/// aiming at the target's budget and writing the checkpoint under the target's
/// terms.
///
/// The checkpoint and destination selection commit together. Store failure or
/// cancellation before commit leaves the actor and original checkpoint untouched.
pub(super) async fn run(
    actor: &mut AgentActor,
    status: &StdArc<RuntimeStatus>,
    sequencer: &Sequencer,
    cancel: &CancellationToken,
    request: &SelectionRequest,
    operation_id: Ulid,
) -> Result<Switched, String> {
    let alias = &request.alias;
    if cancel.is_cancelled() {
        return Err(super::compact::CANCELLED.to_string());
    }
    if actor.has_pending() || status.progress_parked() {
        return Err(PENDING_MODEL_ERROR.to_string());
    }
    let switcher = status
        .controls
        .switcher()
        .ok_or_else(|| "this session cannot switch model".to_string())?;
    let current = (switcher.current_alias)();
    if current == *alias && request.command == "model" && !alias.contains('/') {
        return Ok(Switched {
            data: json!({ "alias": alias, "changed": false }),
        });
    }
    // From here until the choice is installed or given up, no agent, variant
    // or compaction request lands.
    let _switching = status.selection.begin_switch().await;
    if !request.requesting_agent.is_empty()
        && status.selection.current().id.as_str() != request.requesting_agent
    {
        return Err("the requesting agent changed before the selection could apply".into());
    }
    let mut target = (switcher.resolve)(request.clone()).await?;
    target.previous.alias = current;
    // Reasoning is only replayed to the identity that produced it: against
    // another one (or a model that omits it) the whole history so far is
    // projected without it. The check measures exactly that projection.
    let from_message = actor.transcript.len();
    let floor = if !target.same_identity || target.reasoning_history == ReasoningHistory::Omit {
        from_message
    } else {
        actor.reasoning_floor
    };
    let budget = target.limits.budget.input_budget;
    let old_role = actor.role.clone();
    let old_compaction = actor.compaction.clone();
    // Measure destination policy without installing it on the live actor.
    let destination_role = target.agent.clone().or_else(|| old_role.clone());
    let breakdown = destination_breakdown(
        actor,
        destination_role.as_ref(),
        floor,
        old_compaction.as_ref(),
    );
    let mut compacted = None;
    let mut staged = None;
    if breakdown.total() > budget {
        let summarized = compact_for_target(
            actor,
            status,
            cancel,
            &target.alias,
            floor,
            budget,
            destination_role.as_ref(),
        )
        .await?;
        if destination_breakdown(
            actor,
            destination_role.as_ref(),
            floor,
            Some(&summarized.compaction),
        )
        .total()
            > budget
        {
            return Err(format!(
                "the conversation still does not fit {alias}'s window after a compaction: \
                 choose a model with a larger context window, or start a new conversation"
            ));
        }
        compacted = Some(summarized.done);
        staged = Some(StagedCheckpoint {
            transcript: actor.transcript.clone(),
            checkpoint: summarized.compaction,
        });
    }
    // A session that is stopping commits nothing: the stop wins.
    if cancel.is_cancelled() {
        return Err(super::compact::CANCELLED.to_string());
    }
    let window = target.resolution.bounds.context_window;
    let mut changed = serde_json::to_value(ModelChanged {
        operation_id: Some(operation_id),
        revision: (switcher.next_revision)(),
        reason: ModelChangeReason::Switch,
        previous: Some(target.previous.clone()),
        new: target.new.clone(),
        variant: target.variant.as_ref().map(|(name, _)| name.clone()),
        window,
        input_budget: budget,
        from_message: from_message as u64,
    })
    .unwrap_or(Value::Null);
    if let Some(agent) = &target.agent {
        changed["agent"] = json!(agent.id);
        changed["previous_agent"] = json!(old_role.as_ref().map(|role| role.id.as_str()));
        changed["source"] = json!(target.source);
        changed["variant_source"] = json!(target.variant_source);
    }
    if let Some(warning) = &target.warning {
        changed["warning"] = json!(warning);
    }
    // The new gauge is built before the write, against the destination
    // projection, and installed with the selection.
    let limits = target
        .limits
        .clone()
        .with_sources(&actor.rules, &actor.skills);
    let tracker = ContextTracker::new(
        limits,
        destination_breakdown(
            actor,
            destination_role.as_ref(),
            floor,
            staged
                .as_ref()
                .map(|s| &s.checkpoint)
                .or(old_compaction.as_ref()),
        ),
    );
    let snapshot = tracker.snapshot().clone();
    let install: Install = {
        let (status, variant, role) =
            (status.clone(), target.variant.clone(), target.agent.clone());
        Box::new(move || {
            status.replace_variant(variant);
            status.replace_context(tracker);
            if let Some(role) = role {
                status.selection.install_selected(role);
            }
        })
    };
    // The selection (row, resolution, variant, floor) and its event are
    // written in one transaction; the event reaches the stream only once it
    // is durable, and nothing below runs when it is not.
    let commit = {
        let (switcher, target, checkpoint, token) = (
            switcher.clone(),
            target.clone(),
            staged.clone(),
            cancel.clone(),
        );
        Box::new(move |envelopes: Vec<EventEnvelope>| {
            (switcher.commit)(target, floor, checkpoint, envelopes, install, token)
        })
    };
    let mut candidates = Vec::new();
    if let Some(done) = &compacted {
        candidates.push(Candidate::new(EventKind::ContextCompacted, json!({"reason":"switch", "generation":done.generation, "through_message":done.through_message, "pruned":done.pruned, "estimate_before":done.estimate_before, "estimate_after":done.estimate_after})));
    }
    candidates.push(Candidate::new(EventKind::ModelChanged, changed));
    if request.command == "agent" {
        candidates.push(Candidate::new(EventKind::SessionAgentChanged, json!({"agent": destination_role.as_ref().map(|r| r.id.as_str()), "previous": old_role.as_ref().map(|r| r.id.as_str())})));
    }
    sequencer
        .commit_batch_acked(candidates, commit)
        .await
        .map_err(|e| format!("the model switch could not be saved: {e}"))?;
    status.controls.before_switch_swap().await;

    actor.reasoning_floor = floor;
    actor.role = destination_role;
    if let Some(staged) = staged {
        actor.compaction = Some(staged.checkpoint);
    }
    actor.provider = target.provider.clone();
    actor.api_key = target.api_key.clone();
    actor.model = target.model.clone();
    actor.reasoning_history = target.reasoning_history;
    actor.max_tokens = target.max_tokens;
    actor.variant = target.variant.clone().map(|(_, variant)| variant);
    #[cfg(debug_assertions)]
    {
        let observer = SWITCH_OBSERVERS.lock().unwrap().remove(&operation_id);
        if let Some(observer) = observer {
            observer(actor);
        }
    }
    let variant_name = target.variant.as_ref().map(|(name, _)| name.clone());

    if let Ok(payload) = serde_json::to_value(&snapshot) {
        let _ = sequencer.publish(Candidate::new(EventKind::ContextUpdated, payload));
    }
    Ok(Switched {
        data: json!({
            "warning": target.warning,
            "alias": target.new.alias,
            "changed": true,
            "previous": identity_json(&target.previous),
            "new": identity_json(&target.new),
            "variant": variant_name,
            "window": window,
            "input_budget": budget,
            "from_message": from_message,
            "compacted": compacted.map(|done| json!({
                "generation": done.generation,
                "estimate_before": done.estimate_before,
                "estimate_after": done.estimate_after,
            })),
        }),
    })
}

/// Compacts the conversation with the model it is on now, so that it fits
/// `budget`, the input budget of the model it is about to move to. The
/// estimate is the destination's: its reasoning policy applies (`floor`).
async fn compact_for_target(
    actor: &AgentActor,
    status: &StdArc<RuntimeStatus>,
    cancel: &CancellationToken,
    alias: &str,
    floor: usize,
    budget: u64,
    role: Option<&Arc<crate::agent::definitions::AgentDefinition>>,
) -> Result<super::compact::Summarized, String> {
    use super::compact::{summarize_with_components, Failure, Goal};
    use crate::agent::compaction::{Aim, CompactionError};

    let source_budget = status
        .context_snapshot()
        .map(|snapshot| snapshot.input_budget)
        .ok_or_else(|| "this session has no context budget".to_string())?;
    // The transcript as the destination reads it.
    let seen_by_target = actor.planning_transcript(floor);
    let goal = Goal {
        focus: None,
        aim: Aim::Fit,
        destination_budget: budget,
        source_budget,
    };
    let refused = |why: String| {
        format!(
            "the conversation does not fit {alias}'s window and cannot be compacted to fit it: \
             {why}"
        )
    };
    let message = destination_message(actor, role);
    let tools = actor.effective_tool_schemas_for_role(role.map(Arc::as_ref));
    let summarized = summarize_with_components(
        actor,
        status,
        cancel,
        &seen_by_target,
        goal,
        crate::agent::compaction::Components {
            system_prompt: message.as_deref(),
            rules: actor.rules.message(),
            skills: actor.effective_skills_catalog_for_role(role.map(Arc::as_ref)),
            tools: &tools,
        },
    )
    .await
    .map_err(|e| match e {
        Failure::Compaction(CompactionError::Cancelled) => super::compact::CANCELLED.to_string(),
        Failure::Compaction(
            e @ (CompactionError::Capacity { .. }
            | CompactionError::NothingToCompact
            | CompactionError::SummaryInputTooLarge { .. }),
        ) => refused(e.to_string()),
        other => format!("the compaction before the switch failed: {other}"),
    })?;
    Ok(summarized)
}

fn destination_breakdown(
    actor: &AgentActor,
    role: Option<&Arc<crate::agent::definitions::AgentDefinition>>,
    floor: usize,
    compaction: Option<&crate::agent::compaction::Compaction>,
) -> crate::agent::context::Breakdown {
    let message = destination_message(actor, role);
    let transcript = actor.planning_transcript(floor);
    let history = crate::agent::compaction::effective_history(&transcript, compaction);
    crate::agent::context::estimate(
        message.as_deref(),
        actor.rules.message(),
        actor.effective_skills_catalog_for_role(role.map(Arc::as_ref)),
        &history,
        &actor.effective_tool_schemas_for_role(role.map(Arc::as_ref)),
    )
}

fn destination_message(
    actor: &AgentActor,
    role: Option<&Arc<crate::agent::definitions::AgentDefinition>>,
) -> Option<String> {
    let message = crate::agent::actor::compose_role_message(
        role.map(|r| r.prompt.as_str()),
        actor.system_prompt.as_deref(),
    );
    let roots: Vec<_> = actor
        .scratch_roots
        .iter()
        .filter(|root| root.valid())
        .map(|root| root.path.display().to_string())
        .collect();
    if roots.is_empty() {
        message
    } else {
        Some(format!(
            "{}\nTemporary scratch roots (private first, recommended): {}",
            message.as_deref().unwrap_or(""),
            roots.join(", ")
        ))
    }
}

fn result(
    operation_id: Ulid,
    outcome: &Result<Switched, String>,
    applies: &str,
    command: &str,
) -> Candidate {
    let result = match outcome {
        Ok(done) => CommandResult {
            operation_id,
            name: command.to_string(),
            ok: true,
            data: Some(CommandResult::bound_data(done.wire(applies))),
            error: None,
        },
        Err(why) => CommandResult {
            operation_id,
            name: command.to_string(),
            ok: false,
            data: None,
            error: Some(why.clone()),
        },
    };
    Candidate::new(
        EventKind::CommandResult,
        serde_json::to_value(result).unwrap_or(Value::Null),
    )
}

/// Publishes the `command.result` of a request that ran between turns, then
/// answers the caller. A result that cannot be published (the session closed)
/// leaves the request unanswered, which the caller reports as stopping.
pub(super) async fn answer(
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    request: ModelRequest,
    outcome: Result<Switched, String>,
) {
    status.controls.before_result().await;
    if sequencer
        .publish_acked(result(
            request.operation_id,
            &outcome,
            "now",
            &request.selection.command,
        ))
        .await
        .is_err()
    {
        return;
    }
    let _ = request.reply.send(match outcome {
        Ok(done) => ModelReply::Done(done),
        Err(why) => ModelReply::Refused(why),
    });
}

/// Accepts a request that arrived while a turn runs: the acceptance is
/// published, acknowledged, before the caller is told.
pub(super) async fn accept_deferred(sequencer: &Sequencer, request: ModelRequest) -> bool {
    let payload = serde_json::to_value(CommandResult {
        operation_id: request.operation_id,
        name: request.selection.command.clone(),
        ok: true,
        data: Some(CommandResult::bound_data(
            json!({ "accepted": true, "applies": "after_turn" }),
        )),
        error: None,
    })
    .unwrap_or(Value::Null);
    if sequencer
        .publish_acked(Candidate::new(EventKind::CommandResult, payload))
        .await
        .is_err()
    {
        return false;
    }
    let _ = request.reply.send(ModelReply::Deferred);
    true
}

/// Tells the stream how a request that waited for the turn ended.
pub(super) fn report(sequencer: &Sequencer, operation_id: Ulid, outcome: Result<Switched, String>) {
    let _ = sequencer.publish(result(operation_id, &outcome, "after_turn", "model"));
}
