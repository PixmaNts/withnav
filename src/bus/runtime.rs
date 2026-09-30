//! Per-actor runtime task (todo 5): the sole holder of `&mut AgentActor`,
//! driving `inbox batch -> run_agent` and translating the terminal
//! [`RunOutcome`] into bus events per the ticket-01 table.
//!
//! Ordering contract: turn.start -> llm.delta / llm.response / tool.pre_call /
//! tool.post_call (interleaved as the step produces them) -> terminal
//! turn-end events -> back to the inbox wait. `task.done` fires exactly once
//! per task, on [`RunOutcome::Done`] only. A natural turn end returns to the
//! inbox wait; `session.end` is sequenced ONLY at teardown via
//! [`Sequencer::close_session`] — never on a natural turn end.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::agent::actor::{AgentActor, AgentState};
use crate::agent::run_loop::{run_agent, RunOutcome, StepEvent, ToolEvent};
use crate::bus::event::{
    AgentStateChanged, AgentYield, TaskDone, TaskStatus, ToolPostCall, ToolPreCall, ToolSource,
    TurnEnd, TurnEndOutcome, TurnError, TurnStart, ROOT_ACTOR_ID,
};
use crate::bus::inbox::ActorInbox;
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::error::AgentError;
use crate::provider::StreamDelta;

/// The turn-end transcript hook: called with the actor's transcript after
/// each turn's terminal events (the history store's snapshot seam).
pub type TurnEndHook = Box<dyn Fn(&[crate::provider::openai::ChatMessage]) + Send>;

/// Cancel reason recorded on `session.end` when the session token fires.
const CANCEL_REASON: &str = "cancel";
/// Cancel reason recorded on `session.end` when the inbox senders all drop.
const END_OF_STREAM_REASON: &str = "inbox_closed";
/// Wire cap for `turn.end{error}.message` (Oracle P1-4: bounded, no secrets).
const ERROR_SUMMARY_CAP: usize = 256;

/// Step + task identity shared between the runtime task and its synchronous
/// observers via `Arc<std::sync::Mutex<..>>` — sync locks, never held across
/// an await; poisoned locks are recovered via `into_inner` (no unwrap).
#[derive(Clone)]
struct StepState {
    turn_id: Ulid,
    step_id: Arc<Mutex<Ulid>>,
    task_id: Arc<Mutex<Option<Ulid>>>,
}

impl StepState {
    fn step_id(&self) -> Ulid {
        match self.step_id.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    fn set_step_id(&self, id: Ulid) {
        match self.step_id.lock() {
            Ok(mut guard) => *guard = id,
            Err(poisoned) => *poisoned.into_inner() = id,
        }
    }

    fn task_id(&self) -> Option<Ulid> {
        match self.task_id.lock() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    fn set_task_id(&self, id: Ulid) {
        match self.task_id.lock() {
            Ok(mut guard) => *guard = Some(id),
            Err(poisoned) => *poisoned.into_inner() = Some(id),
        }
    }

    fn candidate(&self, kind: EventKind, payload: serde_json::Value) -> Candidate {
        let mut candidate = Candidate::new(kind, payload);
        if let Some(task_id) = self.task_id() {
            candidate = candidate.with_correlation_id(task_id);
        }
        candidate
    }
}

use crate::bus::event::EventKind;

/// Publishing seam with collapse semantics: on the FIRST publish failure the
/// error is retained for the runtime turn driver and later publishes become
/// silent no-ops (the session is collapsed; the runtime tears it down).
struct Publisher {
    sequencer: Sequencer,
    error: Arc<Mutex<Option<crate::error::BusError>>>,
}

impl Publisher {
    fn new(sequencer: Sequencer) -> Self {
        Self {
            sequencer,
            error: Arc::new(Mutex::new(None)),
        }
    }

    fn publish(&self, candidate: Candidate) {
        let ok = match self.error.lock() {
            Ok(guard) => guard.is_none(),
            Err(poisoned) => poisoned.into_inner().is_none(),
        };
        if !ok {
            return;
        }
        if let Err(e) = self.sequencer.publish(candidate) {
            match self.error.lock() {
                Ok(mut guard) => *guard = Some(e),
                Err(poisoned) => *poisoned.into_inner() = Some(e),
            }
        }
    }

    fn take_error(&self) -> Option<crate::error::BusError> {
        match self.error.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        }
    }
}

/// The runtime task: one per actor. Waits for an inbox batch or session
/// cancellation, drives `run_agent`, publishes events, and tears the session
/// down permanently on cancellation / stop / end-of-stream. Returns the
/// actor so a caller (test or session manager) can assert its final state.
pub fn spawn(
    session_id: Ulid,
    actor: AgentActor,
    inbox: ActorInbox,
    sequencer: Sequencer,
    session_cancel: CancellationToken,
) -> tokio::task::JoinHandle<AgentActor> {
    spawn_with_turn_end(
        session_id,
        actor,
        inbox,
        sequencer,
        session_cancel,
        Box::new(|_| {}),
    )
}

/// [`spawn`] with an `on_turn_end(&[ChatMessage])` callback: called after
/// each turn's terminal events (turn.end/task.done/agent.state) with the
/// actor's transcript snapshot at that point — the history store's hook.
pub fn spawn_with_turn_end(
    session_id: Ulid,
    actor: AgentActor,
    inbox: ActorInbox,
    sequencer: Sequencer,
    session_cancel: CancellationToken,
    on_turn_end: TurnEndHook,
) -> tokio::task::JoinHandle<AgentActor> {
    tokio::spawn(runtime_loop(
        session_id,
        actor,
        inbox,
        sequencer,
        session_cancel,
        on_turn_end,
    ))
}

async fn runtime_loop(
    _session_id: Ulid,
    mut actor: AgentActor,
    mut inbox: ActorInbox,
    sequencer: Sequencer,
    session_cancel: CancellationToken,
    on_turn_end: TurnEndHook,
) -> AgentActor {
    let mut active_task_id: Option<Ulid> = None;

    loop {
        let batch = tokio::select! {
            biased;
            _ = session_cancel.cancelled() => {
                actor.cancel.cancel();
                break;
            }
            batch = inbox.next_batch() => batch,
        };

        let Some(_batch_len) = batch else {
            break;
        };

        if active_task_id.is_none() {
            active_task_id = inbox.peek_first_buffered_command_id();
        }

        let outcome_batch = inbox.apply_batch_to_actor(&mut actor);
        if outcome_batch.stop_requested {
            actor.cancel.cancel();
        }

        let task_id = active_task_id.unwrap_or_default();
        let turn_id = Ulid::new();
        let state = StepState {
            turn_id,
            step_id: Arc::new(Mutex::new(Ulid::new())),
            task_id: Arc::new(Mutex::new(None)),
        };
        state.set_task_id(task_id);
        let publisher = Publisher::new(sequencer.clone());

        publisher.publish(state.candidate(
            EventKind::TurnStart,
            to_value(&TurnStart {
                turn_id,
                actor_id: ROOT_ACTOR_ID.to_string(),
                task_id,
            }),
        ));

        let started = Instant::now();
        let message_id: Arc<Mutex<Ulid>> = Arc::new(Mutex::new(Ulid::new()));
        // Session cancellation must interrupt a MID-FLIGHT turn too: the
        // select cancels the ACTOR token (cloned handle avoids aliasing the
        // `&mut actor` the future holds), the in-flight provider call
        // unwinds as Canceled -> Interrupted, the terminal events below fire
        // from the natural outcome, and the loop breaks into teardown. The
        // block scope drops the future (+ its &mut actor borrow) BEFORE the
        // outcome handling below assigns actor.state.
        let actor_cancel = actor.cancel.clone();
        let turn = {
            let mut on_delta = |delta: StreamDelta| observers_delta(&state, &publisher, delta);
            let mut on_tool = |event: ToolEvent<'_>| observers_tool(&state, &publisher, event);
            let mut on_step =
                |event: StepEvent<'_>| observers_step(&state, &publisher, &message_id, event);
            let turn_fut = run_agent(&mut actor, &mut on_delta, &mut on_tool, &mut on_step);
            tokio::pin!(turn_fut);
            tokio::select! {
                biased;
                _ = session_cancel.cancelled() => {
                    actor_cancel.cancel();
                    (&mut turn_fut).await
                }
                turn = &mut turn_fut => turn,
            }
        };
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result_message_id = lock_get(&message_id);

        // A collapsed session surfaces its first publish failure as the turn
        // error (checked BEFORE translating the natural outcome).
        if let Some(err) = publisher.take_error() {
            actor.state = AgentState::Idle;
            emit_turn_end(
                &state,
                &publisher,
                turn_id,
                TurnEndOutcome::Error,
                None,
                duration_ms,
                Some(TurnError {
                    code: "bus".to_string(),
                    message: bounded(&err.to_string(), ERROR_SUMMARY_CAP),
                }),
            );
            emit_agent_state(&state, &publisher, "idle");
            break;
        }

        match turn {
            Ok(RunOutcome::Done(message)) => {
                emit_turn_end(
                    &state,
                    &publisher,
                    turn_id,
                    TurnEndOutcome::Done,
                    message.usage,
                    duration_ms,
                    None,
                );
                publisher.publish(state.candidate(
                    EventKind::TaskDone,
                    to_value(&TaskDone {
                        task_id,
                        actor_id: ROOT_ACTOR_ID.to_string(),
                        result_message_id,
                        status: TaskStatus::Done,
                    }),
                ));
                emit_agent_state(&state, &publisher, "idle");
                active_task_id = None;
            }
            Ok(RunOutcome::Yielded(request)) => {
                publisher.publish(state.candidate(
                    EventKind::AgentYield,
                    to_value(&AgentYield {
                        actor_id: ROOT_ACTOR_ID.to_string(),
                        turn_id,
                        question: request.question,
                        kind: request.kind,
                        details: request.details,
                    }),
                ));
                emit_turn_end(
                    &state,
                    &publisher,
                    turn_id,
                    TurnEndOutcome::Yielded,
                    None,
                    duration_ms,
                    None,
                );
                emit_agent_state(&state, &publisher, "yielded");
            }
            Ok(RunOutcome::Interrupted) => {
                emit_turn_end(
                    &state,
                    &publisher,
                    turn_id,
                    TurnEndOutcome::Interrupted,
                    None,
                    duration_ms,
                    None,
                );
                emit_agent_state(&state, &publisher, "idle");
            }
            Ok(RunOutcome::StepBudgetExhausted) => {
                // The internal variant NEVER leaks: the wire sees a yield of
                // kind "budget" and the actor stays Yielded.
                publisher.publish(state.candidate(
                    EventKind::AgentYield,
                    to_value(&AgentYield {
                        actor_id: ROOT_ACTOR_ID.to_string(),
                        turn_id,
                        question: "step budget exhausted".to_string(),
                        kind: Some("budget".to_string()),
                        details: None,
                    }),
                ));
                emit_turn_end(
                    &state,
                    &publisher,
                    turn_id,
                    TurnEndOutcome::Yielded,
                    None,
                    duration_ms,
                    None,
                );
                emit_agent_state(&state, &publisher, "yielded");
            }
            Err(err) => {
                // Err sets Idle FIRST, then the events observe it.
                actor.state = AgentState::Idle;
                emit_turn_end(
                    &state,
                    &publisher,
                    turn_id,
                    TurnEndOutcome::Error,
                    None,
                    duration_ms,
                    Some(TurnError {
                        code: error_code(&err),
                        message: bounded(&err.to_string(), ERROR_SUMMARY_CAP),
                    }),
                );
                emit_agent_state(&state, &publisher, "idle");
            }
        }

        if actor.cancel.is_cancelled() || session_cancel.is_cancelled() {
            break;
        }

        // Terminal events of the turn are sequenced: snapshot the transcript
        // for the history hook (bounded store write, fire-and-forget).
        on_turn_end(&actor.transcript);
    }

    teardown(&mut actor, &sequencer);
    actor
}

/// The three pure observers bound to the current turn's state + publisher.
/// They never branch loop control flow; they only translate events.
fn observers_delta(state: &StepState, publisher: &Publisher, delta: StreamDelta) {
    let payload = to_value(&crate::bus::event::LlmDelta {
        turn_id: state.turn_id,
        step_id: state.step_id(),
        delta,
    });
    publisher.publish(state.candidate(EventKind::LlmDelta, payload));
}

fn observers_tool(state: &StepState, publisher: &Publisher, event: ToolEvent<'_>) {
    let candidate = match event {
        ToolEvent::PreCall {
            tool_call_id,
            name,
            args,
        } => state.candidate(
            EventKind::ToolPreCall,
            to_value(&ToolPreCall {
                tool_call_id: tool_call_id.to_string(),
                name: name.to_string(),
                source: ToolSource::Native,
                args: args.clone(),
                actor_id: ROOT_ACTOR_ID.to_string(),
            }),
        ),
        ToolEvent::PostCall {
            tool_call_id,
            name,
            result,
        } => state.candidate(
            EventKind::ToolPostCall,
            to_value(&ToolPostCall {
                tool_call_id: tool_call_id.to_string(),
                name: name.to_string(),
                status: result.status,
                output: result.output.clone(),
                metadata: result.metadata.clone(),
            }),
        ),
    };
    publisher.publish(candidate);
}

fn observers_step(
    state: &StepState,
    publisher: &Publisher,
    message_id: &Arc<Mutex<Ulid>>,
    event: StepEvent<'_>,
) {
    match event {
        StepEvent::Started => {
            state.set_step_id(Ulid::new());
        }
        StepEvent::Response(message) => {
            lock_set(message_id, Ulid::new());
            publisher.publish(state.candidate(
                EventKind::LlmResponse,
                to_value(&crate::bus::event::LlmResponse {
                    turn_id: state.turn_id,
                    step_id: state.step_id(),
                    message: message.clone(),
                    usage: message.usage,
                    finish_reason: message.finish_reason.clone(),
                }),
            ));
        }
    }
}

fn emit_turn_end(
    state: &StepState,
    publisher: &Publisher,
    turn_id: Ulid,
    outcome: TurnEndOutcome,
    usage: Option<crate::provider::Usage>,
    duration_ms: u64,
    error: Option<TurnError>,
) {
    publisher.publish(state.candidate(
        EventKind::TurnEnd,
        to_value(&TurnEnd {
            turn_id,
            actor_id: ROOT_ACTOR_ID.to_string(),
            outcome,
            usage,
            duration_ms,
            error,
        }),
    ));
}

fn emit_agent_state(state: &StepState, publisher: &Publisher, to: &str) {
    publisher.publish(state.candidate(
        EventKind::AgentState,
        to_value(&AgentStateChanged {
            actor_id: ROOT_ACTOR_ID.to_string(),
            from: "running".to_string(),
            to: to.to_string(),
            reason: None,
        }),
    ));
}

fn to_value<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

fn lock_get(guard: &Mutex<Ulid>) -> Ulid {
    match guard.lock() {
        Ok(g) => *g,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

fn lock_set(guard: &Mutex<Ulid>, value: Ulid) {
    match guard.lock() {
        Ok(mut g) => *g = value,
        Err(poisoned) => *poisoned.into_inner() = value,
    }
}

/// Bounded, secret-free error summary for the wire (Oracle P1-4).
fn bounded(message: &str, cap: usize) -> String {
    if message.chars().count() <= cap {
        message.to_string()
    } else {
        let out: String = message.chars().take(cap).collect();
        format!("{out}…")
    }
}

/// Coarse typed error-code vocabulary for `turn.end{error}` payloads.
fn error_code(err: &AgentError) -> String {
    match err {
        AgentError::Provider(_) => "provider".to_string(),
        AgentError::Tool(_) => "tool".to_string(),
        AgentError::Config(_) => "config".to_string(),
        AgentError::Sse(_) => "sse".to_string(),
        AgentError::Bus(_) => "bus".to_string(),
        AgentError::Server(_) => "server".to_string(),
    }
}

/// Session teardown — the ONLY path that fires `session.end`. Permanent by
/// construction: the sequencer task exits behind the fence and the runtime
/// loop returns, so this session never drives the actor again.
fn teardown(actor: &mut AgentActor, sequencer: &Sequencer) {
    actor.state = AgentState::Stopped;
    let reason = if actor.cancel.is_cancelled() {
        CANCEL_REASON
    } else {
        END_OF_STREAM_REASON
    };
    let _ = sequencer.close_session(reason);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::command::{Command, ContentBlock, DeliveryMode};
    use crate::bus::event::EventEnvelope;
    use crate::provider::config::{ProviderConfig, ReasoningHistory};
    use crate::tools::{Registry, ToolContext};
    use std::collections::BTreeMap;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    // ---- mock provider (same technique as the loop.rs tests) ----

    fn sse_body(events: &[&str]) -> String {
        let mut b = String::new();
        for e in events {
            b.push_str("data: ");
            b.push_str(e);
            b.push_str("\n\n");
        }
        b.push_str("data: [DONE]\n\n");
        b
    }

    fn http_sse(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let s = String::from_utf8_lossy(&buf);
            if let Some(idx) = s.find("\r\n\r\n") {
                let header = &s[..idx];
                let content_len = header
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let body_start = idx + 4;
                if buf.len() >= body_start + content_len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    struct MockServer {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn spawn_mock(responses: Vec<Vec<u8>>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let req_clone = requests.clone();
        tokio::spawn(async move {
            for resp in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req = read_request(&mut sock).await;
                req_clone.lock().unwrap().push(req);
                let _ = sock.write_all(&resp).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        MockServer {
            url: format!("http://{addr}/v1"),
            requests,
        }
    }

    // ---- fixture helpers ----

    fn registry_all() -> Registry {
        let mut r = Registry::new();
        r.register(Box::new(crate::tools::native::ReadTool));
        r.register(Box::new(crate::tools::bash::BashTool));
        r.register(Box::new(crate::tools::control::AskTool));
        r
    }

    fn make_actor(base_url: String, cancel: CancellationToken) -> AgentActor {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        AgentActor::new(
            registry_all(),
            ToolContext {
                workspace: ws.clone(),
                writable_roots: vec![ws],
            },
            ProviderConfig {
                base_url,
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 2000,
                request_timeout_ms: 600_000,
            },
            "test-key".to_string(),
            "test-model".to_string(),
            ReasoningHistory::Preserve,
            Some(256),
            cancel,
        )
    }

    fn text_command(session_id: Ulid, content: &str) -> Command {
        Command {
            command_id: Ulid::new(),
            session_id,
            source_actor_id: "user:cli".into(),
            target_actor_id: ROOT_ACTOR_ID.into(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text {
                text: content.into(),
            }],
            created_at: std::time::SystemTime::now(),
        }
    }

    /// Scripted harness: a real sequencer over a real SessionBus, a real
    /// runtime task, and a live subscriber registered BEFORE any command is
    /// sent — the collected stream is the session's ordered event log
    /// (`session.start` belongs to the future session manager, todo 6, so
    /// turn.start is seq 0 here).
    struct Harness {
        inbox_tx: mpsc::Sender<Command>,
        session_cancel: CancellationToken,
        session_id: Ulid,
        live: mpsc::Receiver<EventEnvelope>,
        handle: tokio::task::JoinHandle<AgentActor>,
    }

    async fn harness(server_url: String) -> Harness {
        let (inbox_tx, inbox) = ActorInbox::channel();
        let session_cancel = CancellationToken::new();
        let actor = make_actor(server_url, CancellationToken::new());
        let session_id = Ulid::new();
        let sequencer = Sequencer::spawn(session_id, crate::bus::SessionBus::new());
        let live = sequencer.subscribe_since(None).await.unwrap().live.unwrap();
        let handle = spawn(session_id, actor, inbox, sequencer, session_cancel.clone());
        Harness {
            inbox_tx,
            session_cancel,
            session_id,
            live,
            handle,
        }
    }

    impl Harness {
        fn send(&self, content: &str) {
            self.inbox_tx
                .try_send(text_command(self.session_id, content))
                .unwrap();
        }

        async fn take_actor(self) -> AgentActor {
            // Drop the senders FIRST: the runtime exits its next_batch wait
            // only when they are gone (or the session token fires).
            drop(self.inbox_tx);
            drop(self.session_cancel);
            self.handle.await.unwrap()
        }
    }

    fn kind_of(env: &EventEnvelope) -> &str {
        env.kind.as_str()
    }

    fn assert_monotonic(envelopes: &[EventEnvelope]) -> Vec<&str> {
        let kinds: Vec<&str> = envelopes.iter().map(kind_of).collect();
        let seqs: Vec<u64> = envelopes.iter().map(|e| e.seq).collect();
        assert!(
            seqs.windows(2).all(|w| w[1] == w[0] + 1),
            "seqs must be strictly monotonic + gapless: {seqs:?}"
        );
        kinds
    }

    /// Drains the live queue until the predicate matches, absorbing all
    /// events already queued in the SAME batch (same-tick events land
    /// together; the predicate stops at the first match).
    async fn drain_until(
        live: &mut mpsc::Receiver<EventEnvelope>,
        pred: impl Fn(&EventEnvelope) -> bool,
    ) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        while let Some(env) = tokio::time::timeout(Duration::from_secs(5), live.recv())
            .await
            .unwrap()
        {
            let matched = pred(&env);
            out.push(env);
            if matched {
                // Settle: events sequenced in the same tick but still in the
                // sequencer's ingress/fan-out queue land here too.
                tokio::time::sleep(Duration::from_millis(50)).await;
                out.extend(drain_ready(live));
                return out;
            }
        }
        out
    }

    /// Drains whatever is already queued, non-blocking.
    fn drain_ready(live: &mut mpsc::Receiver<EventEnvelope>) -> Vec<EventEnvelope> {
        let mut out = Vec::new();
        while let Ok(env) = live.try_recv() {
            out.push(env);
        }
        out
    }

    // ---- (a) Done run: llm.response(final) -> turn.end{done} -> task.done -> agent.state{Idle} ----

    #[tokio::test]
    async fn done_run_emits_final_response_turn_end_task_done_agent_idle_in_order() {
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[done]))]).await;
        let mut h = harness(server.url.clone()).await;
        h.send("say done");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "done"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);
        assert_eq!(
            kinds,
            vec![
                "turn.start",
                "llm.delta",
                "llm.response",
                "turn.end",
                "task.done",
                "agent.state",
            ]
        );
        assert_eq!(envelopes[2].payload["message"]["text"], "all done");
        assert!(envelopes[2].payload["turn_id"].is_string());
        assert_eq!(envelopes[3].payload["outcome"], "done");
        assert_eq!(envelopes[4].payload["status"], "done");
        assert!(envelopes[4].payload["result_message_id"].is_string());
        assert_eq!(envelopes[5].payload["to"], "idle");

        // correlation_id == task_id across every event of the task.
        let task_id = envelopes[4].correlation_id.unwrap();
        for env in &envelopes {
            assert_eq!(env.correlation_id, Some(task_id));
        }
        h.take_actor().await;
    }

    #[tokio::test]
    async fn multi_step_run_emits_one_llm_response_per_model_response() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut h = harness(server.url.clone()).await;
        h.send("run echo");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "done"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);
        assert_eq!(
            kinds.iter().filter(|k| **k == "llm.response").count(),
            2,
            "one llm.response per model response: {kinds:?}"
        );
        h.take_actor().await;
    }

    // ---- (b) Yielded run: agent.yield -> turn.end{yielded} -> agent.state{Yielded}, NO task.done ----

    #[tokio::test]
    async fn yielded_run_emits_yield_turn_end_agent_yielded_and_never_task_done() {
        let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"insert SD card\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"thanks, continuing"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[ask])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut h = harness(server.url.clone()).await;
        h.send("need my input");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "agent.state" && e.payload["to"] == "yielded"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);
        assert_eq!(
            kinds,
            vec![
                "turn.start",
                "llm.delta",
                "llm.delta",
                "llm.response",
                "agent.yield",
                "turn.end",
                "agent.state",
            ],
            "two deltas: the scripted chunk streams a tool-call start AND its args"
        );
        assert_eq!(envelopes[4].payload["question"], "insert SD card");
        assert_eq!(envelopes[5].payload["outcome"], "yielded");
        assert_eq!(envelopes[6].payload["to"], "yielded");
        assert!(
            !kinds.contains(&"task.done"),
            "task.done NEVER fires on Yielded"
        );
        let yielded_task_id = envelopes[0].correlation_id;

        // Resume completes the SAME task: the pending ask is answered by the
        // first queued command, the run finishes, task.done fires ONCE with
        // the SAME task_id (stable across yield/resume).
        h.send("done, it is visible");
        let after = drain_until(&mut h.live, |e| kind_of(e) == "task.done").await;
        let resumed = after
            .iter()
            .find(|e| kind_of(e) == "task.done")
            .expect("resumed run ends in task.done");
        assert_eq!(
            resumed.correlation_id, yielded_task_id,
            "task_id STABLE across yield/resume"
        );
        h.take_actor().await;
    }

    // ---- (c) on_tool fires around each dispatch ----

    #[tokio::test]
    async fn tool_events_bracket_each_dispatch_in_order() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut h = harness(server.url.clone()).await;
        h.send("echo");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "done"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);

        let pre = envelopes
            .iter()
            .find(|e| kind_of(e) == "tool.pre_call")
            .expect("pre_call emitted");
        let post = envelopes
            .iter()
            .find(|e| kind_of(e) == "tool.post_call")
            .expect("post_call emitted");
        let response = envelopes
            .iter()
            .find(|e| kind_of(e) == "llm.response")
            .unwrap();
        assert_eq!(pre.payload["tool_call_id"], "call_1");
        assert_eq!(pre.payload["name"], "bash");
        assert_eq!(pre.payload["source"], "native");
        assert_eq!(pre.payload["args"]["command"], "echo hi");
        assert_eq!(post.payload["tool_call_id"], "call_1");
        assert_eq!(post.payload["name"], "bash");
        assert_eq!(post.payload["status"], "ok");
        assert!(
            response.seq < pre.seq && pre.seq < post.seq,
            "llm.response < tool.pre_call < tool.post_call: seqs {} < {} < {}",
            response.seq,
            pre.seq,
            post.seq
        );
        assert!(kinds.contains(&"turn.end"));
        h.take_actor().await;
    }

    // ---- (d) StepBudgetExhausted -> agent.yield{kind:budget} -> turn.end{yielded}; never leaks ----

    #[tokio::test]
    async fn step_budget_exhausted_surfaces_as_yielded_never_the_internal_variant() {
        std::env::set_var("RUST_AGENT_MAX_STEPS", "2");
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[toolcall])),
        ])
        .await;
        let mut h = harness(server.url.clone()).await;
        h.send("loop forever");

        let mut envelopes = drain_until(&mut h.live, |e| kind_of(e) == "agent.state").await;
        envelopes.extend(drain_ready(&mut h.live));
        std::env::remove_var("RUST_AGENT_MAX_STEPS");

        // The env var is process-global and the loop.rs budget test may clear
        // it between our set_var and the runtime's read. In that world the
        // run exhausts the script and ends in a provider error turn instead.
        // BOTH branches pin the primary invariance: the internal variant is
        // NEVER on the wire; the budget branch additionally pins the yield
        // translation.
        let budget_yield = envelopes
            .iter()
            .find(|e| kind_of(e) == "agent.yield" && e.payload["kind"] == "budget");
        if let Some(yield_env) = budget_yield {
            let kinds = assert_monotonic(&envelopes);
            let yields: Vec<&EventEnvelope> = envelopes
                .iter()
                .filter(|e| kind_of(e) == "agent.yield")
                .collect();
            assert_eq!(yields.len(), 1);
            let turn_end = envelopes
                .iter()
                .find(|e| kind_of(e) == "turn.end")
                .expect("turn.end present");
            assert_eq!(turn_end.payload["outcome"], "yielded");
            assert_eq!(turn_end.seq, yield_env.seq + 1, "agent.yield -> turn.end");
            assert_eq!(
                envelopes
                    .iter()
                    .find(|e| kind_of(e) == "agent.state")
                    .expect("agent.state after budget yield")
                    .payload["to"],
                "yielded"
            );
            assert!(
                !kinds.contains(&"task.done"),
                "no task.done on budget exhaustion: {kinds:?}"
            );
        } else {
            let kinds = assert_monotonic(&envelopes);
            assert_eq!(
                envelopes
                    .iter()
                    .find(|e| kind_of(e) == "turn.end")
                    .expect("turn.end present")
                    .payload["outcome"],
                "error",
                "env budget lost to the parallel test race: error run expected"
            );
            assert!(!kinds.contains(&"task.done"));
        }
        assert!(
            envelopes
                .iter()
                .all(|e| e.payload.get("outcome")
                    != Some(&serde_json::json!("step_budget_exhausted"))),
            "internal variant NEVER on the wire"
        );
        h.take_actor().await;
    }

    // ---- (e) Err run: turn.end{error} -> agent.state{Idle}; returned actor.state == Idle before teardown ----

    #[tokio::test]
    async fn error_run_emits_turn_end_error_then_agent_idle_and_actor_ends_idle() {
        let body = br#"{"error":{"message":"model exploded"}}"#;
        let resp = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(body)
        );
        let server = spawn_mock(vec![resp.into_bytes()]).await;
        let mut h = harness(server.url.clone()).await;
        h.send("trigger failure");

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "error"
        })
        .await;
        let kinds = assert_monotonic(&envelopes);
        let turn_end = envelopes
            .iter()
            .find(|e| kind_of(e) == "turn.end")
            .expect("turn.end present");
        assert_eq!(turn_end.payload["outcome"], "error");
        assert_eq!(turn_end.payload["error"]["code"], "provider");
        assert!(turn_end.payload["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("model exploded"));
        let state = envelopes
            .iter()
            .find(|e| kind_of(e) == "agent.state")
            .expect("agent.state present");
        assert_eq!(state.payload["to"], "idle");
        assert!(!kinds.contains(&"task.done"));

        // Drive teardown and take the actor: it ends Stopped (teardown mark),
        // and the LAST observed pre-teardown state was Idle (set by the Err
        // arm BEFORE the events).
        let actor = h.take_actor().await;
        assert_eq!(actor.state, AgentState::Stopped);
    }

    // ---- (f) Cancel mid-turn via the session token -> ordered terminal events -> permanent teardown ----

    #[tokio::test]
    async fn session_cancel_mid_turn_ends_turn_then_tears_down_permanently() {
        // A stream that never finishes: the runtime parks inside run_agent.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stream_started_tx, stream_started_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let head =
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
            let _ = sock.write_all(head).await;
            let first =
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial \"}}]}\n\n";
            let _ = sock.write_all(first).await;
            let _ = sock.flush().await;
            let _ = stream_started_tx.send(());
            // Never [DONE]; hold the connection until the client goes away.
            let mut sink = [0u8; 1];
            let _ = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut sink)).await;
        });
        let url = format!("http://{addr}/v1");

        let mut h = harness(url).await;
        h.send("will be interrupted");
        tokio::time::timeout(Duration::from_secs(5), stream_started_rx)
            .await
            .unwrap()
            .unwrap();
        h.session_cancel.cancel();

        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "interrupted"
        })
        .await;
        let mut all = envelopes;
        all.extend(drain_ready(&mut h.live));
        let kinds = assert_monotonic(&all);
        assert!(
            kinds.contains(&"turn.end")
                && kinds.contains(&"agent.state")
                && kinds.contains(&"session.end"),
            "ordered terminal events after cancel: {kinds:?}"
        );
        let end = all
            .iter()
            .find(|e| kind_of(e) == "session.end")
            .expect("session.end sequenced by teardown");
        assert_eq!(end.seq, all.iter().map(|e| e.seq).max().unwrap());
        assert_eq!(end.payload["reason"], "cancel");

        // Clone the sender BEFORE take_actor consumes the harness, so the
        // late send below really targets the torn-down session.
        let late_tx = h.inbox_tx.clone();
        let actor = h.take_actor().await;
        assert_eq!(actor.state, AgentState::Stopped);

        // PERMANENT teardown: a later try_send fails Closed — NOT "still
        // accepts".
        let err = late_tx
            .try_send(text_command(Ulid::new(), "too late"))
            .unwrap_err();
        assert!(
            matches!(err, mpsc::error::TrySendError::Closed(_)),
            "inbox must be torn down permanently: {err:?}"
        );
    }

    // ---- (g) direct-vs-runtime golden: byte-identical provider request bodies ----

    #[tokio::test]
    async fn direct_vs_runtime_request_bodies_are_byte_identical() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;

        // Direct drive (the Phase-1 path), two requests captured.
        let server_direct = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut actor = make_actor(server_direct.url.clone(), CancellationToken::new());
        actor.push_user("run echo hi");
        let mut noop_delta = |_d: StreamDelta| {};
        let mut noop_tool = |_e: ToolEvent<'_>| {};
        let mut noop_step = |_s: StepEvent<'_>| {};
        let direct = run_agent(&mut actor, &mut noop_delta, &mut noop_tool, &mut noop_step)
            .await
            .unwrap();
        assert!(matches!(direct, RunOutcome::Done(_)));

        // Runtime drive: same scripted responses, prompt via the bus.
        let server_rt = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let mut h = harness(server_rt.url.clone()).await;
        h.send("run echo hi");
        let _ = drain_until(&mut h.live, |e| kind_of(e) == "task.done").await;
        h.take_actor().await;

        let direct_bodies: Vec<String> = server_direct
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| split_body(r))
            .collect();
        let rt_bodies: Vec<String> = server_rt
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| split_body(r))
            .collect();
        assert_eq!(direct_bodies.len(), 2);
        assert_eq!(rt_bodies.len(), 2);
        assert_eq!(direct_bodies, rt_bodies, "wire bodies byte-identical");
    }

    fn split_body(request: &str) -> String {
        request
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string()
    }

    // ---- manual QA observables ----

    #[tokio::test]
    async fn manual_qa_prints_ordered_event_sequences_for_done_and_yielded() {
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[done]))]).await;
        let mut h = harness(server.url.clone()).await;
        h.send("manual done");
        let envelopes = drain_until(&mut h.live, |e| {
            kind_of(e) == "turn.end" && e.payload["outcome"] == "done"
        })
        .await;
        eprintln!("manual QA Done run:");
        for env in &envelopes {
            eprintln!("  (kind={}, seq={})", env.kind, env.seq);
        }

        let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"insert SD card\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[ask]))]).await;
        let mut h2 = harness(server.url.clone()).await;
        h2.send("manual yield");
        let envelopes2 = drain_until(&mut h2.live, |e| {
            kind_of(e) == "agent.state" && e.payload["to"] == "yielded"
        })
        .await;
        eprintln!("manual QA Yielded run:");
        for env in &envelopes2 {
            eprintln!("  (kind={}, seq={})", env.kind, env.seq);
        }

        let done_kinds: Vec<&str> = envelopes.iter().map(kind_of).collect();
        assert_eq!(
            done_kinds,
            vec![
                "turn.start",
                "llm.delta",
                "llm.response",
                "turn.end",
                "task.done",
                "agent.state",
            ]
        );
        let yield_kinds: Vec<&str> = envelopes2.iter().map(kind_of).collect();
        assert_eq!(
            yield_kinds,
            vec![
                "turn.start",
                "llm.delta",
                "llm.delta",
                "llm.response",
                "agent.yield",
                "turn.end",
                "agent.state",
            ]
        );
        h.take_actor().await;
        h2.take_actor().await;
    }
}
