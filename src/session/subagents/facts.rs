//! Facts about a session's recent tool calls, counted by WithNav from its stored events.
//!
//! A question to the decision model that sees only the last call and how long ago it ran cannot
//! tell a long build from a loop: a comparable supervisor whose drift question saw only
//! `tool -> ok|error` read drift 0 times in 298 calls, including 40-minute stalls (reviewed
//! 2026-11-26). So the watchdogs give the model what a person would count first, computed here
//! and never guessed by the model: how many times the same call returned the same result, how
//! many failures nothing has answered, how long since anything moved.
//!
//! The fold reads the same durable `tool.pre_call` / `tool.post_call` events the ledger reads,
//! pairs them by call id, and keeps counters plus the last [`RECENT_CALLS`] calls. It never sees
//! reasoning, only what the tools did. A watchdog keeps one [`Facts`] per watched session and
//! feeds it only the events after the last `seq` it saw: replaying a long session in full at every
//! tick reads more rows each time the session grows, and an hour-long session is read most.
//!
//! A call with a `tool.pre_call` and no `tool.post_call` yet is running, not failed. It is
//! counted in [`Counts::in_flight`] and listed among the attempts with how long it has run, so a
//! watchdog does not mistake a long build for a call that never answered.
//!
//! Progress is one of three observable things: a file changed (a successful `write` or `edit`),
//! a command passed (a `bash` call that exited 0), or a call failed with a different error than
//! the last time the same call failed. Reading and searching are never progress by themselves:
//! an agent can read for an hour and move nothing.

use std::collections::{HashMap, VecDeque};

use serde::Serialize;
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

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
/// Most distinct failing calls remembered to tell a changed error from a repeated one. A session
/// that lives for hours keeps this state between checks; without a bound a long run of different
/// failing commands would grow it for as long as the session lives. Past the bound the memory
/// restarts, which at worst reads one repeated error as new once.
const MAX_FAILURES_KEPT: usize = 256;
/// The `exit` text of a call that has started and not answered.
pub const IN_FLIGHT: &str = "in flight";
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
    /// Calls started and not answered yet. A running call is not a failure.
    pub in_flight: u64,
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
    /// Whole seconds a call in flight has been seen running; `None` for a settled call. Counted
    /// from the check that first saw the call start, so it can only understate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
}

/// What a fold of a session's events says about its recent calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub counts: Counts,
    /// The last [`RECENT_CALLS`] settled calls, oldest first.
    pub attempts: Vec<Attempt>,
    /// The `seq` of the event that last made progress; `None` when nothing has yet.
    pub progress_marker: Option<u64>,
    /// Progress happened since the previous check (see [`ProgressClock`]). Only a stamped
    /// snapshot can say so.
    pub progressed: bool,
}

impl Snapshot {
    /// This snapshot with `minutes_since_progress` and `progressed` read from `clock` at `now`.
    pub fn stamped(mut self, clock: &mut ProgressClock, now: Instant) -> Self {
        let (minutes, moved) = clock.stamp(self.progress_marker, now);
        self.counts.minutes_since_progress = minutes;
        self.progressed = moved;
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

    /// Whole minutes since the marker was first seen, and whether it moved to a new progress
    /// since the previous call.
    fn stamp(&mut self, marker: Option<u64>, now: Instant) -> (u64, bool) {
        let moved = marker.is_some() && marker != self.marker;
        if marker != self.marker {
            self.marker = marker;
            self.since = now;
        }
        (
            now.saturating_duration_since(self.since).as_secs() / 60,
            moved,
        )
    }
}

/// The facts of one watched session, kept between checks.
///
/// Each [`Facts::read`] fetches only the events after the last `seq` it folded and adds them to
/// the folded state, so the work per check is the events since the previous one.
pub struct Facts {
    folder: Folder,
    clock: ProgressClock,
    /// The `seq` of the last event folded; `None` before the first read.
    through: Option<u64>,
}

impl Facts {
    pub fn new(now: Instant) -> Self {
        Self {
            folder: Folder::default(),
            clock: ProgressClock::new(now),
            through: None,
        }
    }

    /// The `seq` of the last event folded.
    pub fn through(&self) -> Option<u64> {
        self.through
    }

    /// Reads the events of `session` after the last one seen and folds them in; `None` when the
    /// store cannot be read in time or `cancel` fires first, and nothing is folded then. An
    /// unreadable snapshot is "unjudged", never zeros.
    pub async fn read(
        &mut self,
        history: &HistoryStore,
        session: SessionId,
        now: Instant,
        cancel: &CancellationToken,
    ) -> Option<Snapshot> {
        let replay = history.replay(session, self.through, u64::MAX);
        let events = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            read = tokio::time::timeout(super::root_watchdog::LEDGER_READ_DEADLINE, replay) => read.ok()?.ok()?,
        };
        Some(self.absorb(&events, now))
    }

    /// Folds `events` in `seq` order, skipping any at or before the last one seen, and returns
    /// the snapshot at `now`.
    pub fn absorb(&mut self, events: &[EventEnvelope], now: Instant) -> Snapshot {
        for event in events {
            if self.through.is_some_and(|seen| event.seq <= seen) {
                continue;
            }
            self.folder.apply(event, Some(now));
            self.through = Some(event.seq);
        }
        self.folder
            .snapshot(Some(now))
            .stamped(&mut self.clock, now)
    }
}

/// Folds one session's events, in `seq` order, in one go. Elapsed times of calls in flight are
/// unknown here.
pub fn fold(events: &[EventEnvelope]) -> Snapshot {
    let mut folder = Folder::default();
    for event in events {
        folder.apply(event, None);
    }
    folder.snapshot(None)
}

#[derive(Default)]
struct Folder {
    /// Calls announced by `tool.pre_call` and not settled yet.
    pending: HashMap<String, Pending>,
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

/// A call started and not answered yet.
struct Pending {
    seq: u64,
    name: String,
    args: Value,
    /// When a check first saw it running; `None` when folded without a clock.
    seen: Option<Instant>,
}

impl Folder {
    fn apply(&mut self, event: &EventEnvelope, now: Option<Instant>) {
        let payload = &event.payload;
        match EventKind::from_wire(&event.kind) {
            Some(EventKind::ToolPreCall) => {
                if let (Some(id), Some(name)) =
                    (payload["tool_call_id"].as_str(), payload["name"].as_str())
                {
                    self.pending.insert(
                        id.to_owned(),
                        Pending {
                            seq: event.seq,
                            name: name.to_owned(),
                            args: payload["args"].clone(),
                            seen: now,
                        },
                    );
                }
            }
            Some(EventKind::ToolPostCall) => {
                if let Some(call) = payload["tool_call_id"]
                    .as_str()
                    .and_then(|id| self.pending.remove(id))
                {
                    self.settle(event.seq, &call.name, &call.args, payload);
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
            if self.failures.len() >= MAX_FAILURES_KEPT && !self.failures.contains_key(&signature) {
                self.failures.clear();
            }
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
            elapsed_secs: None,
        });
    }

    /// The counts and attempts so far. Calls in flight come after the settled ones, oldest start
    /// first, within the same [`RECENT_CALLS`] bound.
    fn snapshot(&self, now: Option<Instant>) -> Snapshot {
        let mut running: Vec<&Pending> = self.pending.values().collect();
        running.sort_by_key(|call| call.seq);
        let mut attempts: Vec<Attempt> = self.attempts.iter().cloned().collect();
        attempts.extend(running.iter().map(|call| {
            let text = if call.name == "bash" {
                call.args["command"].as_str().map(one_line)
            } else {
                None
            }
            .unwrap_or_else(|| one_line(&format!("{} {}", call.name, call.args)));
            Attempt {
                call: head(&text, CALL_CHARS),
                exit: IN_FLIGHT.to_owned(),
                error: String::new(),
                elapsed_secs: call
                    .seen
                    .zip(now)
                    .map(|(seen, now)| now.saturating_duration_since(seen).as_secs()),
            }
        }));
        let excess = attempts.len().saturating_sub(RECENT_CALLS);
        attempts.drain(..excess);
        Snapshot {
            counts: Counts {
                repeat_streak: self.streak,
                failures_unanswered: self.unanswered,
                read_only_streak: self.read_only_streak,
                minutes_since_progress: 0,
                in_flight: self.pending.len() as u64,
                recent_exits: attempts.iter().map(|a| a.exit.clone()).collect(),
            },
            attempts,
            progress_marker: self.marker,
            progressed: false,
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
    fn an_interrupted_call_counts_for_nothing_and_an_unsettled_one_is_only_in_flight() {
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
        assert_eq!(snapshot.counts.failures_unanswered, 1);
        assert_eq!(snapshot.counts.in_flight, 1);
        // The failed call, then the one still running; the interrupted one is nowhere.
        let exits: Vec<&str> = snapshot.attempts.iter().map(|a| a.exit.as_str()).collect();
        assert_eq!(exits, ["exit 1", IN_FLIGHT]);
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

    fn pre(events: &mut Vec<EventEnvelope>, id: &str, command: &str) {
        let seq = events.len() as u64 + 1;
        events.push(EventEnvelope::new(
            ulid::Ulid::generate(),
            seq,
            EventKind::ToolPreCall,
            json!({"tool_call_id": id, "name": "bash", "args": {"command": command}}),
        ));
    }

    fn post(events: &mut Vec<EventEnvelope>, id: &str) {
        let seq = events.len() as u64 + 1;
        events.push(EventEnvelope::new(
            ulid::Ulid::generate(),
            seq,
            EventKind::ToolPostCall,
            json!({"tool_call_id": id, "name": "bash", "status": "ok", "output": "done",
                "metadata": {"exit_code": 0}}),
        ));
    }

    #[test]
    fn a_call_in_flight_is_counted_and_listed_with_its_elapsed_time_and_is_no_failure() {
        let mut events = Vec::new();
        bash(&mut events, "cargo test", "error: 2 tests failed", 101);
        pre(&mut events, "slow", "cargo build --release");
        let folded = fold(&events);
        assert_eq!(folded.counts.in_flight, 1);
        // No clock: the call is listed, its time is not known.
        let last = folded.attempts.last().unwrap();
        assert_eq!(
            (last.call.as_str(), last.exit.as_str()),
            ("cargo build --release", IN_FLIGHT)
        );
        assert_eq!(last.elapsed_secs, None);
        // The running call is not a failure and does not break the failure count.
        assert_eq!(folded.counts.failures_unanswered, 1);
        assert_eq!(folded.counts.recent_exits, ["exit 101", IN_FLIGHT]);

        // Seen at one check and again 90 seconds later, then answered.
        let start = Instant::now();
        let mut facts = Facts::new(start);
        let first = facts.absorb(&events, start);
        assert_eq!(first.attempts.last().unwrap().elapsed_secs, Some(0));
        let later = facts.absorb(&events, start + std::time::Duration::from_secs(90));
        assert_eq!(later.counts.in_flight, 1);
        assert_eq!(later.attempts.last().unwrap().elapsed_secs, Some(90));
        post(&mut events, "slow");
        let done = facts.absorb(&events, start + std::time::Duration::from_secs(95));
        assert_eq!(done.counts.in_flight, 0);
        assert_eq!(done.attempts.last().unwrap().exit, "ok");
        assert_eq!(done.attempts.last().unwrap().elapsed_secs, None);
    }

    #[test]
    fn events_already_folded_are_never_folded_again() {
        let mut events = Vec::new();
        for _ in 0..3 {
            bash(&mut events, "cargo test", "error: 2 tests failed", 101);
        }
        let start = Instant::now();
        let mut facts = Facts::new(start);
        facts.absorb(&events[..4], start);
        assert_eq!(facts.through(), Some(4));
        // Handed the whole list again, only the new events count.
        let seen = facts.absorb(&events, start);
        assert_eq!(facts.through(), Some(6));
        assert_eq!(seen.counts, fold(&events).counts);
        assert_eq!(seen.counts.repeat_streak, 3);
        assert_eq!(facts.absorb(&events, start).counts.repeat_streak, 3);
    }

    #[test]
    fn failing_calls_are_remembered_up_to_a_bound() {
        let mut events = Vec::new();
        for n in 0..MAX_FAILURES_KEPT + 20 {
            bash(&mut events, &format!("cmd {n}"), "error: boom", 1);
        }
        let mut folder = Folder::default();
        for event in &events {
            folder.apply(event, None);
        }
        assert!(folder.failures.len() <= MAX_FAILURES_KEPT);
        assert_eq!(
            folder.snapshot(None).counts.failures_unanswered,
            events.len() as u64 / 2
        );
    }

    async fn stored(dir: &tempfile::TempDir) -> (HistoryStore, SessionId) {
        let store = HistoryStore::open(dir.path().join("sessions.db")).unwrap();
        let id = ulid::Ulid::generate();
        store.record_session(id, "/w", "m", false).await.unwrap();
        store
            .append_event_acked(
                id,
                EventEnvelope::new(id, 0, EventKind::SessionStart, json!({})),
            )
            .await
            .unwrap();
        (store, id)
    }

    async fn failing_call(store: &HistoryStore, id: SessionId, seq: u64) {
        for (offset, kind, payload) in [
            (
                0,
                EventKind::ToolPreCall,
                json!({"tool_call_id": format!("c{seq}"), "name": "bash",
                    "args": {"command": "cargo test"}}),
            ),
            (
                1,
                EventKind::ToolPostCall,
                json!({"tool_call_id": format!("c{seq}"), "name": "bash", "status": "error",
                    "output": "error: 2 tests failed", "metadata": {"exit_code": 101}}),
            ),
        ] {
            store
                .append_event_acked(id, EventEnvelope::new(id, seq + offset, kind, payload))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_read_folds_only_what_was_stored_since_the_last_one_and_stops_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = stored(&dir).await;
        let now = Instant::now();
        let cancel = CancellationToken::new();
        let mut facts = Facts::new(now);
        failing_call(&store, id, 1).await;
        failing_call(&store, id, 3).await;
        let first = facts.read(&store, id, now, &cancel).await.unwrap();
        assert_eq!(first.counts.repeat_streak, 2);
        assert_eq!(facts.through(), Some(4));
        // Nothing new: the same counts, with nothing folded twice.
        let again = facts.read(&store, id, now, &cancel).await.unwrap();
        assert_eq!(again.counts.repeat_streak, 2);
        failing_call(&store, id, 5).await;
        let third = facts.read(&store, id, now, &cancel).await.unwrap();
        assert_eq!(third.counts.repeat_streak, 3);
        assert_eq!(facts.through(), Some(6));

        // A cancelled session is not read, and what was folded stays as it was.
        failing_call(&store, id, 7).await;
        cancel.cancel();
        assert!(facts.read(&store, id, now, &cancel).await.is_none());
        assert_eq!(facts.through(), Some(6));
        let fresh = CancellationToken::new();
        let resumed = facts.read(&store, id, now, &fresh).await.unwrap();
        assert_eq!(resumed.counts.repeat_streak, 4);
    }
}
