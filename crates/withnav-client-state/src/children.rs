//! Agents started by a conversation and how far each one got.
use withnav_client_api::event::EventEnvelope;

/// An agent working for a tab's conversation, drawn nested under it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChildAgent {
    pub session_id: String,
    pub name: String,
    pub goal: String,
    pub status: ChildStatus,
    pub delivery: Option<String>,
    pub reason: Option<String>,
    /// A sequenced parent event takes precedence over a session-list snapshot.
    pub event_seq: Option<u64>,
    pub child_seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChildStatus {
    #[default]
    Unknown,
    Running,
    Permission,
    OutOfSteps,
    Doubt,
    Done,
    Failed,
    Stopped,
}

impl ChildStatus {
    pub fn terminal_event(event: &EventEnvelope) -> Option<Self> {
        match event.kind.as_str() {
            "session.end" => Some(Self::Stopped),
            _ => None,
        }
    }

    pub fn project(status: &mut Self, seq: &mut Option<u64>, event: &EventEnvelope) {
        if status.terminal() || seq.is_some_and(|held| event.seq <= held) {
            return;
        }
        *seq = Some(event.seq);
        if let Some(terminal) = Self::terminal_event(event) {
            *status = terminal;
            return;
        }
        *status = match event.kind.as_str() {
            "turn.start" => Self::Running,
            "agent.yield" if event.payload["kind"] == "budget" => Self::OutOfSteps,
            "permission.requested" => Self::Permission,
            "permission.resolved" => Self::Running,
            _ => *status,
        };
    }
    pub fn from_wire(status: &str) -> Self {
        match status {
            "running" | "active" => Self::Running,
            "waiting_for_permission" | "permission" => Self::Permission,
            "out_of_steps" | "step_budget_exhausted" | "parked" => Self::OutOfSteps,
            "doubt" => Self::Doubt,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "—",
            Self::Running => "running",
            Self::Permission => "waiting for permission",
            Self::OutOfSteps => "out of steps",
            Self::Doubt => "in doubt",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Stopped)
    }
}
