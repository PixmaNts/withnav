//! The controls accepted while a turn runs, kept in the order they came and
//! run in that order when the turn ends: `/model other` then `/compact`
//! compacts with the new model.

use ulid::Ulid;

use super::{compact, model};
use crate::agent::actor::AgentActor;
use crate::bus::sequencer::Sequencer;
use tokio_util::sync::CancellationToken;

/// One accepted control and the operation its result is published under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Deferred {
    Compact {
        operation_id: Ulid,
        focus: Option<String>,
    },
    Model {
        operation_id: Ulid,
        alias: String,
    },
}

/// The deferred controls of one turn, oldest first.
pub(super) type Queue = Vec<Deferred>;

/// Tells the stream that every control that waited did not happen, in the
/// order they were accepted, each under its own operation id.
pub(super) fn report_cancelled(sequencer: &Sequencer, queue: &Queue, why: &str) {
    for control in queue {
        match control {
            Deferred::Compact {
                operation_id,
                focus,
            } => compact::report_failure(sequencer, &vec![(*operation_id, focus.clone())], why),
            Deferred::Model { operation_id, .. } => {
                model::report(sequencer, *operation_id, Err(why.to_string()))
            }
        }
    }
}

/// Runs the queue in order. Adjacent compactions are one summary (their focus
/// texts folded into one); a compaction on the other side of a switch is not
/// merged with it, since the summary is written by whichever model is in
/// place.
pub(super) async fn run(
    queue: Queue,
    actor: &mut AgentActor,
    status: &std::sync::Arc<super::RuntimeStatus>,
    sequencer: &Sequencer,
    cancel: &CancellationToken,
) {
    let mut rest = queue.into_iter().peekable();
    while let Some(control) = rest.next() {
        match control {
            Deferred::Compact {
                operation_id,
                focus,
            } => {
                let mut group: compact::Waiting = vec![(operation_id, focus)];
                while let Some(Deferred::Compact { .. }) = rest.peek() {
                    if let Some(Deferred::Compact {
                        operation_id,
                        focus,
                    }) = rest.next()
                    {
                        group.push((operation_id, focus));
                    }
                }
                let focus = compact::merge_focus(&group);
                match compact::run(actor, status, sequencer, cancel, focus.as_deref()).await {
                    Ok(done) => compact::report_done(sequencer, &group, &done),
                    Err(why) => compact::report_failure(sequencer, &group, &why),
                }
            }
            Deferred::Model {
                operation_id,
                alias,
            } => {
                let outcome =
                    model::run(actor, status, sequencer, cancel, &alias, operation_id).await;
                model::report(sequencer, operation_id, outcome);
            }
        }
    }
}
