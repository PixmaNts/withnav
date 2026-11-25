//! `/goal`: the Captain records what counts as done for this session.
//!
//! The command is the only writer of the session goal; no tool lets the model set or change it.
//! Each change is published as one durable `goal.updated` (or `goal.cleared`) under the tree's
//! lifecycle gate, so two `/goal` runs never read the same revision, and only once the event is
//! stored does the runtime's shared goal change: the next request then carries it (or stops
//! carrying it), and the context gauge counts it.

use serde_json::{Value, json};
use withnav_client_api::goal::{
    GoalChange, GoalCleared, GoalLog, GoalStatus, GoalUpdated, MAX_CRITERIA, MAX_CRITERION_CHARS,
    MAX_OBJECTIVE_CHARS, SessionGoal,
};

use super::ledger::one_line;
use super::run_command::{CommandFailure, Done};
use super::service::{Service, SessionHandle};
use crate::bus::event::{EventEnvelope, EventKind};
use crate::bus::sequencer::Candidate;
use crate::error::ServerError;

/// The usage line of every refused `/goal` form.
pub const GOAL_USAGE: &str =
    "usage: /goal [<objective> | criterion <text> | pause | resume | clear]";

/// The refusal of a change that needs a goal when none is set.
pub const NO_GOAL: &str = "no goal is set: set one with /goal <objective>";

/// What one `/goal` line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request<'a> {
    Show,
    Objective(&'a str),
    Criterion(&'a str),
    Pause,
    Resume,
    Clear,
}

/// Parses the arguments of `/goal` (already trimmed). The first words `criterion`, `pause`,
/// `resume` and `clear` are reserved: an objective cannot start with them.
fn parse(arguments: &str) -> Result<Request<'_>, String> {
    if arguments.is_empty() {
        return Ok(Request::Show);
    }
    let (first, rest) = match arguments.split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest.trim()),
        None => (arguments, ""),
    };
    let bare = |request: Request<'static>| {
        if rest.is_empty() {
            Ok(request)
        } else {
            Err(format!("usage: /goal {first}"))
        }
    };
    match first {
        "criterion" if rest.is_empty() => Err("usage: /goal criterion <text>".into()),
        "criterion" => Ok(Request::Criterion(rest)),
        "pause" => bare(Request::Pause),
        "resume" => bare(Request::Resume),
        "clear" => bare(Request::Clear),
        _ => Ok(Request::Objective(arguments)),
    }
}

/// Captain text as the goal keeps it: control characters stripped, on one line. Empty after
/// that is the usage error; longer than `max` characters is refused, never cut.
fn clean(text: &str, what: &str, max: usize, usage: &str) -> Result<String, String> {
    let text = one_line(text);
    if text.is_empty() {
        return Err(usage.to_owned());
    }
    let count = text.chars().count();
    if count > max {
        return Err(format!(
            "the {what} is {count} characters long; the limit is {max}"
        ));
    }
    Ok(text)
}

/// The next goal state for `request`, from `log`: `Ok(None)` for a show.
fn next(log: &GoalLog, request: Request<'_>) -> Result<Option<Change>, String> {
    let revision = log.last_revision + 1;
    let current = log.current.as_ref();
    let change = match request {
        Request::Show => return Ok(None),
        Request::Objective(text) => {
            let objective = clean(text, "objective", MAX_OBJECTIVE_CHARS, GOAL_USAGE)?;
            match current {
                Some(goal) => Change::Updated(
                    SessionGoal {
                        revision,
                        objective,
                        ..goal.clone()
                    },
                    GoalChange::Revised,
                ),
                None => Change::Updated(
                    SessionGoal {
                        revision,
                        objective,
                        criteria: Vec::new(),
                        status: GoalStatus::Active,
                    },
                    GoalChange::Set,
                ),
            }
        }
        Request::Criterion(text) => {
            let goal = current.ok_or(NO_GOAL)?;
            if goal.criteria.len() >= MAX_CRITERIA {
                return Err(format!("a goal holds at most {MAX_CRITERIA} criteria"));
            }
            let criterion = clean(
                text,
                "criterion",
                MAX_CRITERION_CHARS,
                "usage: /goal criterion <text>",
            )?;
            let mut goal = goal.clone();
            goal.revision = revision;
            goal.criteria.push(criterion);
            Change::Updated(goal, GoalChange::Criterion)
        }
        Request::Pause => {
            let goal = current.ok_or(NO_GOAL)?;
            if goal.status == GoalStatus::Paused {
                return Err("the goal is already paused".into());
            }
            // A pause changes neither the objective nor the criteria: same revision.
            let mut goal = goal.clone();
            goal.status = GoalStatus::Paused;
            Change::Updated(goal, GoalChange::Paused)
        }
        Request::Resume => {
            let goal = current.ok_or(NO_GOAL)?;
            if goal.status == GoalStatus::Active {
                return Err("the goal is not paused".into());
            }
            let mut goal = goal.clone();
            goal.status = GoalStatus::Active;
            Change::Updated(goal, GoalChange::Resumed)
        }
        Request::Clear => Change::Cleared(current.ok_or(NO_GOAL)?.revision),
    };
    Ok(Some(change))
}

enum Change {
    Updated(SessionGoal, GoalChange),
    Cleared(u64),
}

impl Service {
    /// `/goal [<objective> | criterion <text> | pause | resume | clear]`: shows the session goal,
    /// or changes it. Runs under the tree's lifecycle gate (held by `run_command`); a refused
    /// form publishes nothing but its `command.result`.
    pub(super) async fn goal_command(
        &self,
        handle: &SessionHandle,
        arguments: &str,
    ) -> Result<Done, CommandFailure> {
        let request = parse(arguments).map_err(CommandFailure::Refused)?;
        let log = handle
            .status
            .goal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(change) = next(&log, request).map_err(CommandFailure::Refused)? else {
            return Ok(Done::data(json!({ "goal": log.current })));
        };
        let (kind, payload, data) = match &change {
            Change::Updated(goal, how) => (
                EventKind::GoalUpdated,
                serde_json::to_value(GoalUpdated {
                    goal: goal.clone(),
                    change: *how,
                }),
                json!({ "goal": goal, "change": how }),
            ),
            Change::Cleared(revision) => (
                EventKind::GoalCleared,
                serde_json::to_value(GoalCleared {
                    revision: *revision,
                }),
                json!({ "goal": Value::Null, "change": "cleared", "revision": revision }),
            ),
        };
        let payload = payload.unwrap_or(Value::Null);
        let seq = handle
            .sequencer
            .publish_acked(Candidate::new(kind, payload.clone()))
            .await
            .map_err(|_| CommandFailure::Server(ServerError::SessionStopping))?;
        // The stored event, folded the way a resume and the ledger fold it, so the live goal
        // never differs from the one the log rebuilds.
        let stored = EventEnvelope::new(ulid::Ulid::nil(), seq, kind, payload);
        let mut shared = handle.status.goal.lock().unwrap_or_else(|e| e.into_inner());
        shared.apply(&stored);
        let message = shared.active().map(crate::agent::actor::goal_message);
        drop(shared);
        if let Some(snapshot) = handle.status.set_context_goal(message.as_deref())
            && let Ok(payload) = serde_json::to_value(&snapshot)
        {
            let _ = handle
                .sequencer
                .publish(Candidate::new(EventKind::ContextUpdated, payload));
        }
        Ok(Done::data(data))
    }
}

/// `/goal` as plain text: the goal with its numbered criteria, or what changed.
pub(super) fn render(data: &Value) -> String {
    let goal = serde_json::from_value::<SessionGoal>(data["goal"].clone()).ok();
    let Some(goal) = goal else {
        return match data["revision"].as_u64() {
            Some(revision) => format!("goal cleared (revision {revision})"),
            None => "no goal is set".into(),
        };
    };
    let status = match goal.status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
    };
    let mut text = format!(
        "goal (revision {}, {status}): {}",
        goal.revision, goal.objective
    );
    if goal.criteria.is_empty() {
        text.push_str("\nno success criteria");
    }
    for (n, criterion) in goal.criteria.iter().enumerate() {
        text.push_str(&format!("\n{}. {criterion}", n + 1));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_first_words_are_forms_and_take_no_text_unless_criterion() {
        assert_eq!(parse(""), Ok(Request::Show));
        assert_eq!(parse("ship it"), Ok(Request::Objective("ship it")));
        assert_eq!(
            parse("criterion  tests pass"),
            Ok(Request::Criterion("tests pass"))
        );
        assert_eq!(parse("pause"), Ok(Request::Pause));
        assert_eq!(parse("resume"), Ok(Request::Resume));
        assert_eq!(parse("clear"), Ok(Request::Clear));
        assert_eq!(
            parse("criterion"),
            Err("usage: /goal criterion <text>".into())
        );
        assert_eq!(parse("pause now"), Err("usage: /goal pause".into()));
        assert_eq!(parse("clear all"), Err("usage: /goal clear".into()));
        // Only the exact lower-case word is reserved.
        assert_eq!(
            parse("Pause the build"),
            Ok(Request::Objective("Pause the build"))
        );
    }
}
