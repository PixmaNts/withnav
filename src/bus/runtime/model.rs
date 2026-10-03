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

/// Why a switch refused while the agent waits for a human.
pub const PENDING_MODEL_ERROR: &str =
    "the agent is waiting for an answer or a decision: reply to its question or \
     decide on its permission request first, then switch model";

/// A boxed future, for the callbacks the service supplies.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What it takes to move the conversation to another alias.
#[derive(Debug, Clone)]
pub struct ModelTarget {
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

/// The service's side of a switch.
pub struct ModelSwitch {
    /// The alias the conversation talks to now.
    pub current_alias: Box<dyn Fn() -> String + Send + Sync>,
    /// Resolves an alias like a new session does: its window, its offer, its
    /// default variant. The refusal text says why it cannot be used.
    pub resolve: Box<dyn Fn(String) -> BoxFuture<Result<ModelTarget, String>> + Send + Sync>,
    /// The revision the next selection will carry.
    pub next_revision: Box<dyn Fn() -> u64 + Send + Sync>,
    /// Stores the choice, the reasoning floor and the sequenced `model.changed`
    /// envelope in ONE transaction and returns once it is durable. The
    /// service's own view of the model changes with it.
    pub commit: Box<
        dyn Fn(ModelTarget, usize, EventEnvelope) -> BoxFuture<Result<(), String>> + Send + Sync,
    >,
}

/// The shared form of [`ModelSwitch`].
pub type ModelSwitcher = Arc<ModelSwitch>;

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
    pub operation_id: Ulid,
    pub reply: oneshot::Sender<ModelReply>,
}

/// Requests accepted while a turn ran, each with its operation id.
pub(super) type Waiting = Vec<(Ulid, String)>;

fn identity_json(identity: &ModelIdentity) -> Value {
    json!({
        "alias": identity.alias,
        "provider": identity.provider,
        "model": identity.model,
    })
}

/// Moves `actor` to the model `alias`. Nothing changes unless the target
/// fits and the store acknowledged the choice.
pub(super) async fn run(
    actor: &mut AgentActor,
    status: &RuntimeStatus,
    sequencer: &Sequencer,
    cancel: &CancellationToken,
    alias: &str,
    operation_id: Ulid,
) -> Result<Switched, String> {
    if cancel.is_cancelled() {
        return Err(super::compact::CANCELLED.to_string());
    }
    if actor.has_pending() {
        return Err(PENDING_MODEL_ERROR.to_string());
    }
    let switcher = status
        .controls
        .switcher()
        .ok_or_else(|| "this session cannot switch model".to_string())?;
    let current = (switcher.current_alias)();
    if current == alias {
        return Ok(Switched {
            data: json!({ "alias": alias, "changed": false }),
        });
    }
    // From here until the choice is installed or given up, no agent, variant
    // or compaction request lands.
    let _switching = status.selection.begin_switch().await;
    if actor.role.is_some() {
        actor.role = Some(status.selection.current());
    }
    let mut target = (switcher.resolve)(alias.to_string()).await?;
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
    let estimate = actor.context_breakdown_at(floor).total();
    let budget = target.limits.budget.input_budget;
    if estimate > budget {
        return Err(format!(
            "the conversation does not fit {alias}'s window: /compact first"
        ));
    }
    // A session that is stopping commits nothing: the stop wins.
    if cancel.is_cancelled() {
        return Err(super::compact::CANCELLED.to_string());
    }
    let window = target.resolution.bounds.context_window;
    let changed = serde_json::to_value(ModelChanged {
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
    // The selection (row, resolution, variant, floor) and its event are
    // written in one transaction; the event reaches the stream only once it
    // is durable, and nothing below runs when it is not.
    let commit = {
        let (switcher, target) = (switcher.clone(), target.clone());
        Box::new(move |envelope: EventEnvelope| (switcher.commit)(target, floor, envelope))
    };
    sequencer
        .commit_acked(Candidate::new(EventKind::ModelChanged, changed), commit)
        .await
        .map_err(|e| format!("the model switch could not be saved: {e}"))?;

    actor.reasoning_floor = floor;
    actor.provider = target.provider.clone();
    actor.api_key = target.api_key.clone();
    actor.model = target.model.clone();
    actor.reasoning_history = target.reasoning_history;
    actor.max_tokens = target.max_tokens;
    actor.variant = target.variant.clone().map(|(_, variant)| variant);
    status.replace_variant(target.variant.clone());
    let variant_name = target.variant.as_ref().map(|(name, _)| name.clone());
    let limits = target
        .limits
        .clone()
        .with_sources(&actor.rules, &actor.skills);
    let snapshot = status.replace_context(ContextTracker::new(limits, actor.context_breakdown()));

    if let Ok(payload) = serde_json::to_value(&snapshot) {
        let _ = sequencer.publish(Candidate::new(EventKind::ContextUpdated, payload));
    }
    Ok(Switched {
        data: json!({
            "alias": target.new.alias,
            "changed": true,
            "previous": identity_json(&target.previous),
            "new": identity_json(&target.new),
            "variant": variant_name,
            "window": window,
            "input_budget": budget,
            "from_message": from_message,
        }),
    })
}

fn result(operation_id: Ulid, outcome: &Result<Switched, String>, applies: &str) -> Candidate {
    let result = match outcome {
        Ok(done) => CommandResult {
            operation_id,
            name: "model".to_string(),
            ok: true,
            data: Some(CommandResult::bound_data(done.wire(applies))),
            error: None,
        },
        Err(why) => CommandResult {
            operation_id,
            name: "model".to_string(),
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
        .publish_acked(result(request.operation_id, &outcome, "now"))
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
        name: "model".to_string(),
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
    let _ = sequencer.publish(result(operation_id, &outcome, "after_turn"));
}

/// Tells the stream that each request that waited for the turn did not
/// happen.
pub(super) fn report_failure(sequencer: &Sequencer, waiting: &Waiting, why: &str) {
    for (operation_id, _) in waiting {
        report(sequencer, *operation_id, Err(why.to_string()));
    }
}
