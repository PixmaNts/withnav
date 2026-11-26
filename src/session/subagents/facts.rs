//! Facts about a session's recent tool calls, counted by WithNav from its stored events.
//!
//! A question to the decision model that sees only the last call and how long ago it ran cannot
//! tell a long build from a loop: a comparable supervisor whose drift question saw only
//! `tool -> ok|error` read drift 0 times in 298 calls, including 40-minute stalls (reviewed
//! 2026-11-26). So the watchdogs give the model what a person would count first, computed here
//! and never guessed by the model: how many times the same call returned the same result, how
//! many failures nothing has answered, how long since anything moved.
//!
//! The fold is pure: it reads the same durable `tool.pre_call` / `tool.post_call` events the
//! ledger reads, pairs them by call id, and keeps counters plus the last [`RECENT_CALLS`] calls.
//! It never sees reasoning, only what the tools did.
//!
//! Progress is one of three observable things: a file changed (a successful `write` or `edit`),
//! a command passed (a `bash` call that exited 0), or a call failed with a different error than
//! the last time the same call failed. Reading and searching are never progress by themselves:
//! an agent can read for an hour and move nothing.

use std::collections::{HashMap, VecDeque};

use serde::Serialize;
use serde_json::Value;
use tokio::time::Instant;

use crate::bus::event::{EventEnvelope, EventKind};
use crate::session::ledger::{head, one_line, tail};
use crate::session::service::SessionId;
use crate::session::store::HistoryStore;
use crate::tools::ToolStatus;

/// Calls kept in [`Snapshot::attempts`] and in [`Counts::recent_exits`]. Eight shows a run of
/// three identical failures, the call that broke it and the ones after, in about 3 KB of state;
/// fewer would hide a change of approach, more would spend the question's input on old calls.
pub const RECENT_CALLS: usize = 8;
/// Longest call text kept per attempt, in characters.
const CALL_CHARS: usize = 160;
/// Longest error text kept per failed attempt, and compared between calls, in characters. The end
/// of an output is where its cause usually is.
const ERROR_CHARS: usize = 200;
/// What the model reads in place of counts that could not be computed. It is never zero: a
/// missing snapshot must not look like a calm one.
pub const UNJUDGED: &str = "unjudged";

/// Tools that only read: a run of them moves nothing.
const READ_ONLY_TOOLS: [&str; 3] = ["read", "glob", "grep"];

/// The counts the model is told are computed by WithNav.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// How many times in a row the last call ran with the same result (1 when it differs from
    /// the one before). The same arguments with a different error restart it: a changed error is
    /// new information.
    pub repeat_streak: u64,
    /// Failed calls since the last progress. A changed error is progress and restarts the count
    /// at that failure.
    pub failures_unanswered: u64,
    /// Calls in a row, ending at the last one, that only read or searched.
    pub read_only_streak: u64,
    /// Whole minutes since progress was last seen, on the watchdog's clock (see
    /// [`ProgressClock`]).
    pub minutes_since_progress: u64,
    /// The outcome of the last [`RECENT_CALLS`] calls, oldest first: `ok`, `exit 3`, `error` or
    /// `denied`.
    pub recent_exits: Vec<String>,
}

/// One recent call, as the model reads it. The text is tool arguments and output: untrusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attempt {
    pub call: String,
    pub exit: String,
    /// The end of a failed call's output; empty when it succeeded.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// What a fold of a session's events says about its recent calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub counts: Counts,
    /// The last [`RECENT_CALLS`] settled calls, oldest first.
    pub attempts: Vec<Attempt>,
    /// The `seq` of the event that last made progress; `None` when nothing has yet.
    pub progress_marker: Option<u64>,
}

impl Snapshot {
    /// This snapshot with `minutes_since_progress` read from `clock` at `now`.
    pub fn stamped(mut self, clock: &mut ProgressClock, now: Instant) -> Self {
        self.counts.minutes_since_progress = clock.minutes(self.progress_marker, now);
        self
    }
}

/// Minutes since progress, on the watchdog's own clock.
///
/// The events carry wall-clock stamps and the watchdog runs on a monotonic clock that tests
/// replace, so the two cannot be subtracted. Instead the clock remembers when it first saw the
/// current progress marker: a check at which the marker moved reads 0, and the count grows with
/// the clock while it stays. Progress that happened between two checks is therefore dated at the
/// later one, which can only understate the time since progress: an unfair stop is the cost this
/// avoids, a late one is the cost it accepts.
#[derive(Debug, Clone, Copy)]
pub struct ProgressClock {
    marker: Option<u64>,
    since: Instant,
}

impl ProgressClock {
    pub fn new(now: Instant) -> Self {
        Self {
            marker: None,
            since: now,
        }
    }

    fn minutes(&mut self, marker: Option<u64>, now: Instant) -> u64 {
        if marker != self.marker {
            self.marker = marker;
            self.since = now;
        }
        now.saturating_duration_since(self.since).as_secs() / 60
    }
}

/// Reads `session`'s events and folds them; `None` when the store cannot be read in time. An
/// unreadable snapshot is "unjudged", never zeros.
pub async fn read(history: &HistoryStore, session: SessionId) -> Option<Snapshot> {
    let replay = history.replay(session, None, u64::MAX);
    let events = tokio::time::timeout(super::root_watchdog::LEDGER_READ_DEADLINE, replay)
        .await
        .ok()?
        .ok()?;
    Some(fold(&events))
}

/// Folds one session's events, in `seq` order.
pub fn fold(events: &[EventEnvelope]) -> Snapshot {
    let mut folder = Folder::default();
    for event in events {
        folder.apply(event);
    }
    folder.finish()
}

#[derive(Default)]
struct Folder {
    /// Calls announced by `tool.pre_call` and not settled yet: name and arguments.
    pending: HashMap<String, (String, Value)>,
    /// The signature and result key of the last settled call.
    last: Option<(String, String)>,
    streak: u64,
    unanswered: u64,
    read_only_streak: u64,
    /// The result key of each call's last failure, to tell a changed error from a repeated one.
    failures: HashMap<String, String>,
    attempts: VecDeque<Attempt>,
    marker: Option<u64>,
}

impl Folder {
    fn apply(&mut self, event: &EventEnvelope) {
        let payload = &event.payload;
        match EventKind::from_wire(&event.kind) {
            Some(EventKind::ToolPreCall) => {
                if let (Some(id), Some(name)) =
                    (payload["tool_call_id"].as_str(), payload["name"].as_str())
                {
                    self.pending
                        .insert(id.to_owned(), (name.to_owned(), payload["args"].clone()));
                }
            }
            Some(EventKind::ToolPostCall) => {
                if let Some((name, args)) = payload["tool_call_id"]
                    .as_str()
                    .and_then(|id| self.pending.remove(id))
                {
                    self.settle(event.seq, &name, &args, payload);
                }
            }
            _ => {}
        }
    }

    fn settle(&mut self, seq: u64, name: &str, args: &Value, payload: &Value) {
        let Ok(status) = serde_json::from_value::<ToolStatus>(payload["status"].clone()) else {
            return;
        };
        // A stop or a new message cut the call short: it says nothing about the agent's work.
        if status == ToolStatus::Interrupted {
            return;
        }
        let output = one_line(payload["output"].as_str().unwrap_or_default());
        let command = (name == "bash")
            .then(|| args["command"].as_str().map(one_line))
            .flatten();
        let exit = match (status, payload["metadata"]["exit_code"].as_i64()) {
            (ToolStatus::Denied, _) => "denied".to_owned(),
            (_, Some(code)) if name == "bash" && code != 0 => format!("exit {code}"),
            (ToolStatus::Error, _) => "error".to_owned(),
            _ => "ok".to_owned(),
        };
        let failed = exit != "ok";
        let text = command
            .clone()
            .unwrap_or_else(|| one_line(&format!("{name} {args}")));
        let signature = format!("{name}:{text}");
        let result = format!("{exit}|{}", comparable(&output));
        if self.last.as_ref() == Some(&(signature.clone(), result.clone())) {
            self.streak += 1;
        } else {
            self.streak = 1;
        }
        self.last = Some((signature.clone(), result.clone()));
        if READ_ONLY_TOOLS.contains(&name) {
            self.read_only_streak += 1;
        } else {
            self.read_only_streak = 0;
        }
        if failed {
            match self.failures.insert(signature, result.clone()) {
                // The same call failed before and the error changed: new information.
                Some(before) if before != result => {
                    self.marker = Some(seq);
                    self.unanswered = 1;
                }
                _ => self.unanswered += 1,
            }
        } else if name == "bash" || name == "write" || name == "edit" {
            self.marker = Some(seq);
            self.unanswered = 0;
            self.failures.clear();
        }
        if self.attempts.len() == RECENT_CALLS {
            self.attempts.pop_front();
        }
        self.attempts.push_back(Attempt {
            call: head(&text, CALL_CHARS),
            error: if failed {
                tail(&output, ERROR_CHARS)
            } else {
                String::new()
            },
            exit,
        });
    }

    fn finish(self) -> Snapshot {
        let attempts: Vec<Attempt> = self.attempts.into();
        Snapshot {
            counts: Counts {
                repeat_streak: self.streak,
                failures_unanswered: self.unanswered,
                read_only_streak: self.read_only_streak,
                minutes_since_progress: 0,
                recent_exits: attempts.iter().map(|a| a.exit.clone()).collect(),
            },
            attempts,
            progress_marker: self.marker,
        }
    }
}

/// The end of `output` with durations blanked, so two runs of the same failing command that
/// differ only in how long they took read as the same error. Counts, paths and messages are kept:
/// "5 failed" then "3 failed" is a partial success and must read as a change.
fn comparable(output: &str) -> String {
    let chars: Vec<char> = tail(output, ERROR_CHARS).chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let starts_number =
            chars[i].is_ascii_digit() && (i == 0 || !chars[i - 1].is_alphanumeric());
        if starts_number {
            let mut end = i;
            while end < chars.len() && (chars[end].is_ascii_digit() || chars[end] == '.') {
                end += 1;
            }
            if let Some(after) = duration_unit(&chars, end) {
                out.push_str("<t>");
                i = after;
                continue;
            }
            out.extend(&chars[i..end]);
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The index after a duration unit (`ms`, `s`, `sec`, `secs`, `second`, `seconds`, one optional
/// space after the number) starting at `at`, when one stands there as a whole word.
fn duration_unit(chars: &[char], at: usize) -> Option<usize> {
    let start = if chars.get(at) == Some(&' ') {
        at + 1
    } else {
        at
    };
    ["seconds", "second", "secs", "sec", "ms", "s"]
        .iter()
        .find_map(|unit| {
            let end = start + unit.chars().count();
            let matches = chars.get(start..end)?.iter().copied().eq(unit.chars());
            (matches && chars.get(end).is_none_or(|c| !c.is_alphanumeric())).then_some(end)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(
        events: &mut Vec<EventEnvelope>,
        name: &str,
        args: Value,
        status: &str,
        output: &str,
        code: Option<i64>,
    ) {
        let id = format!("c{}", events.len());
        let session = ulid::Ulid::generate();
        let seq = events.len() as u64 + 1;
        events.push(EventEnvelope::new(
            session,
            seq,
            EventKind::ToolPreCall,
            json!({"tool_call_id": id, "name": name, "args": args}),
        ));
        events.push(EventEnvelope::new(
            session,
            seq + 1,
            EventKind::ToolPostCall,
            json!({"tool_call_id": id, "name": name, "status": status, "output": output,
                "metadata": code.map_or(json!({}), |c| json!({"exit_code": c}))}),
        ));
    }

    fn bash(events: &mut Vec<EventEnvelope>, command: &str, output: &str, code: i64) {
        let status = if code == 0 { "ok" } else { "error" };
        call(
            events,
            "bash",
            json!({"command": command}),
            status,
            output,
            Some(code),
        );
    }

    #[test]
    fn the_same_failing_call_with_the_same_error_builds_a_streak() {
        let mut events = Vec::new();
        for _ in 0..3 {
            bash(&mut events, "cargo test", "error: 2 tests failed", 101);
        }
        let snapshot = fold(&events);
        assert_eq!(snapshot.counts.repeat_streak, 3);
        assert_eq!(snapshot.counts.failures_unanswered, 3);
        assert_eq!(snapshot.progress_marker, None);
        assert_eq!(snapshot.counts.recent_exits, ["exit 101"; 3]);
        assert_eq!(snapshot.attempts[2].call, "cargo test");
        assert_eq!(snapshot.attempts[2].error, "error: 2 tests failed");
    }

    #[test]
    fn a_changed_error_is_progress_and_restarts_the_streak() {
        let mut events = Vec::new();
        bash(&mut events, "cargo test", "error: 5 tests failed", 101);
        bash(&mut events, "cargo test", "error: 5 tests failed", 101);
        bash(&mut events, "cargo test", "error: 3 tests failed", 101);
        let snapshot = fold(&events);
        assert_eq!(snapshot.counts.repeat_streak, 1);
        assert_eq!(snapshot.counts.failures_unanswered, 1);
        assert_eq!(
            snapshot.progress_marker,
            Some(6),
            "the changed error's event"
        );
    }

    #[test]
    fn durations_in_an_error_do_not_make_it_a_new_error() {
        let mut events = Vec::new();
        bash(&mut events, "make", "failed in 0.31s", 2);
        bash(&mut events, "make", "failed in 12ms", 2);
        bash(&mut events, "make", "failed in 3 seconds", 2);
        assert_eq!(fold(&events).counts.repeat_streak, 3);
        assert_eq!(
            comparable("5 passed, 2 failed in 1.5s (exit 2)"),
            "5 passed, 2 failed in <t> (exit 2)"
        );
        assert_eq!(comparable("version 1.2.3 of s3"), "version 1.2.3 of s3");
    }

    #[test]
    fn a_passing_command_or_a_file_change_answers_the_failures() {
        let mut events = Vec::new();
        bash(&mut events, "cargo test", "boom", 1);
        bash(&mut events, "cargo test", "boom", 1);
        assert_eq!(fold(&events).counts.failures_unanswered, 2);
        call(
            &mut events,
            "edit",
            json!({"path": "a.rs"}),
            "ok",
            "edited",
            None,
        );
        let snapshot = fold(&events);
        assert_eq!(snapshot.counts.failures_unanswered, 0);
        assert_eq!(snapshot.progress_marker, Some(6));
        bash(&mut events, "cargo test", "boom", 1);
        bash(&mut events, "true", "", 0);
        let snapshot = fold(&events);
        assert_eq!(snapshot.counts.failures_unanswered, 0);
        assert_eq!(snapshot.progress_marker, Some(10));
    }

    #[test]
    fn reading_is_never_progress_and_builds_a_read_only_streak() {
        let mut events = Vec::new();
        bash(&mut events, "cargo test", "boom", 1);
        for n in 0..4 {
            call(
                &mut events,
                "read",
                json!({"path": format!("f{n}.rs")}),
                "ok",
                "content",
                None,
            );
        }
        let snapshot = fold(&events);
        assert_eq!(snapshot.counts.read_only_streak, 4);
        assert_eq!(snapshot.counts.failures_unanswered, 1);
        assert_eq!(snapshot.progress_marker, None);
        bash(&mut events, "ls", "", 0);
        assert_eq!(fold(&events).counts.read_only_streak, 0);
    }

    #[test]
    fn interrupted_and_unsettled_calls_count_for_nothing() {
        let mut events = Vec::new();
        bash(&mut events, "cargo test", "boom", 1);
        call(
            &mut events,
            "bash",
            json!({"command": "sleep 99"}),
            "interrupted",
            "",
            None,
        );
        events.push(EventEnvelope::new(
            ulid::Ulid::generate(),
            99,
            EventKind::ToolPreCall,
            json!({"tool_call_id": "running", "name": "bash", "args": {"command": "make"}}),
        ));
        let snapshot = fold(&events);
        assert_eq!(snapshot.counts.repeat_streak, 1);
        assert_eq!(snapshot.attempts.len(), 1);
        assert_eq!(fold(&[]), Snapshot::default());
    }

    #[test]
    fn only_the_last_calls_are_kept_and_a_denial_is_a_failure() {
        let mut events = Vec::new();
        for n in 0..(RECENT_CALLS + 3) {
            call(
                &mut events,
                "read",
                json!({"path": format!("f{n}")}),
                "ok",
                "x",
                None,
            );
        }
        call(
            &mut events,
            "bash",
            json!({"command": "rm x"}),
            "denied",
            "no",
            None,
        );
        let snapshot = fold(&events);
        assert_eq!(snapshot.attempts.len(), RECENT_CALLS);
        assert_eq!(snapshot.counts.recent_exits.last().unwrap(), "denied");
        assert_eq!(snapshot.counts.failures_unanswered, 1);
    }

    #[test]
    fn minutes_since_progress_follow_the_clock_and_restart_on_new_progress() {
        let start = Instant::now();
        let mut clock = ProgressClock::new(start);
        let at = |secs: u64| start + std::time::Duration::from_secs(secs);
        let snapshot = |marker| Snapshot {
            progress_marker: marker,
            ..Snapshot::default()
        };
        assert_eq!(
            snapshot(None)
                .stamped(&mut clock, at(0))
                .counts
                .minutes_since_progress,
            0
        );
        assert_eq!(
            snapshot(None)
                .stamped(&mut clock, at(900))
                .counts
                .minutes_since_progress,
            15
        );
        let moved = snapshot(Some(7)).stamped(&mut clock, at(1200));
        assert_eq!(moved.counts.minutes_since_progress, 0);
        let later = snapshot(Some(7)).stamped(&mut clock, at(1500));
        assert_eq!(later.counts.minutes_since_progress, 5);
    }
}
