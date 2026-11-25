//! The session goal: what the Captain recorded as done for one conversation, with its success
//! criteria, a revision number and a status.
//!
//! The goal has no table of its own. Each change is one durable session event (`goal.updated`
//! carries the whole state, `goal.cleared` ends it), so the current goal is the fold of those
//! events ([`GoalLog`]): it survives a resume, a restart and a compaction like the rest of the
//! log, and the ledger, a resume and a client read it through the same fold.
//!
//! Only the Captain sets it, with `/goal`; no tool lets the model change it. Recording a goal
//! neither delegates work nor authorizes the agent to continue on its own.

use serde::{Deserialize, Serialize};

use crate::event::{EventEnvelope, EventKind};

/// Longest objective, in characters. A longer one is refused, never cut.
pub const MAX_OBJECTIVE_CHARS: usize = 500;
/// Most success criteria one goal holds.
pub const MAX_CRITERIA: usize = 10;
/// Longest criterion, in characters. A longer one is refused, never cut.
pub const MAX_CRITERION_CHARS: usize = 300;

/// Whether the goal is shown to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// Every request of the session carries it.
    #[default]
    Active,
    /// Kept, but no request carries it until it is resumed.
    Paused,
}

/// A session goal and its success criteria.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionGoal {
    /// Goes up by one with each change of the objective or the criteria, and keeps counting
    /// after a clear, so a revision number names one text for the whole session.
    pub revision: u64,
    pub objective: String,
    /// Numbered from 1 in the order they were added.
    #[serde(default)]
    pub criteria: Vec<String>,
    #[serde(default)]
    pub status: GoalStatus,
}

impl SessionGoal {
    /// Whether requests carry it: set and not paused.
    pub fn is_active(&self) -> bool {
        self.status == GoalStatus::Active
    }
}

/// What a `goal.updated` changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalChange {
    /// A goal was set where there was none.
    Set,
    /// The objective was replaced; the criteria were kept.
    Revised,
    /// A criterion was added.
    Criterion,
    Paused,
    Resumed,
}

/// The payload of `goal.updated`: the whole goal after the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalUpdated {
    #[serde(flatten)]
    pub goal: SessionGoal,
    pub change: GoalChange,
}

/// The payload of `goal.cleared`: the revision of the goal that ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCleared {
    pub revision: u64,
}

/// The fold of a session's goal events, in `seq` order: the goal now, and the last revision
/// seen, which the next change counts from (a clear does not reset it).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GoalLog {
    pub current: Option<SessionGoal>,
    pub last_revision: u64,
}

impl GoalLog {
    /// Folds `events` (one session's, in `seq` order); other kinds are skipped.
    pub fn fold<'a>(events: impl IntoIterator<Item = &'a EventEnvelope>) -> Self {
        let mut log = Self::default();
        for event in events {
            log.apply(event);
        }
        log
    }

    /// Applies one event: a goal event changes the state, any other kind, or a payload that does
    /// not read, leaves it as it was.
    pub fn apply(&mut self, event: &EventEnvelope) {
        match EventKind::from_wire(&event.kind) {
            Some(EventKind::GoalUpdated) => {
                if let Ok(updated) = serde_json::from_value::<GoalUpdated>(event.payload.clone()) {
                    self.last_revision = self.last_revision.max(updated.goal.revision);
                    self.current = Some(updated.goal);
                }
            }
            Some(EventKind::GoalCleared) => {
                if let Ok(cleared) = serde_json::from_value::<GoalCleared>(event.payload.clone()) {
                    self.last_revision = self.last_revision.max(cleared.revision);
                    self.current = None;
                }
            }
            _ => {}
        }
    }

    /// The goal requests carry now: set and active.
    pub fn active(&self) -> Option<&SessionGoal> {
        self.current.as_ref().filter(|g| g.is_active())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

    fn event(kind: EventKind, payload: serde_json::Value) -> EventEnvelope {
        EventEnvelope::new(Ulid::generate(), 0, kind, payload)
    }

    fn updated(revision: u64, objective: &str, status: GoalStatus) -> EventEnvelope {
        let payload = GoalUpdated {
            goal: SessionGoal {
                revision,
                objective: objective.into(),
                criteria: vec!["tests pass".into()],
                status,
            },
            change: GoalChange::Set,
        };
        event(
            EventKind::GoalUpdated,
            serde_json::to_value(payload).unwrap(),
        )
    }

    #[test]
    fn the_payload_carries_the_whole_goal_flat() {
        let wire = serde_json::to_value(GoalUpdated {
            goal: SessionGoal {
                revision: 2,
                objective: "ship".into(),
                criteria: vec!["a".into()],
                status: GoalStatus::Paused,
            },
            change: GoalChange::Paused,
        })
        .unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"revision": 2, "objective": "ship", "criteria": ["a"],
                "status": "paused", "change": "paused"})
        );
    }

    #[test]
    fn the_last_event_wins_and_a_clear_keeps_the_revision_count() {
        let events = [
            updated(1, "first", GoalStatus::Active),
            event(EventKind::TurnStart, serde_json::json!({})),
            updated(2, "second", GoalStatus::Paused),
        ];
        let log = GoalLog::fold(&events);
        assert_eq!(log.current.as_ref().unwrap().objective, "second");
        assert_eq!(log.active(), None, "a paused goal is not active");
        let mut log = log;
        log.apply(&event(
            EventKind::GoalCleared,
            serde_json::json!({"revision": 2}),
        ));
        assert_eq!(log.current, None);
        assert_eq!(log.last_revision, 2);
    }

    #[test]
    fn an_unreadable_goal_payload_changes_nothing() {
        let mut log = GoalLog::fold(&[updated(1, "kept", GoalStatus::Active)]);
        log.apply(&event(EventKind::GoalUpdated, serde_json::json!({"x": 1})));
        log.apply(&event(EventKind::GoalCleared, serde_json::json!("no")));
        assert_eq!(log.active().unwrap().objective, "kept");
    }
}
