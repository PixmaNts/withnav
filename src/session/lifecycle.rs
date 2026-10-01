//! Teardown and close: the ordered fence that ends a session, the bounded
//! pool of [`Tombstone`]s it leaves behind (FIFO eviction), and `discard`.
//! The sequencer owns `session.end`; this module only orders the steps and
//! records what the fence assigned.

use std::path::PathBuf;
use std::time::Duration;

use super::service::{Lifecycle, Service, SessionHandle, SessionId, SessionSlot};
use crate::bus::event::EventEnvelope;
use crate::error::ServerError;

/// Retained closed-session tombstones before the OLDEST is evicted (FIFO).
/// Live sessions are never evicted — they are active. Only the tombstone
/// pool is bounded, so clients cannot grow the registry unboundedly through
/// create/stop churn.
pub const MAX_TOMBSTONES: usize = 128;

/// Upper bound on waiting for the runtime task to unwind during teardown:
/// the actor token is already cancelled at that point, so an interrupted
/// provider stream ends promptly; the cap only covers a wedged I/O layer so
/// `/stop` is a bounded HTTP response. On timeout the backstop fence still
/// makes `session.end` the last event.
const TEARDOWN_AWAIT_CAP: Duration = Duration::from_secs(10);

/// A closed session's retained record (its tombstone). Holds what a late
/// GET needs PLUS the frozen durable-history snapshot (for a reconnect
/// after `final_seq`): a streaming route replays `retained` through
/// `final_seq` then ends cleanly. No `inbox_tx`, no `Sequencer`, no
/// cancellation token: the routing resources are released when the live
/// slot is replaced.
#[derive(Clone, Debug)]
pub struct Tombstone {
    /// The session's last seq: the seq the sequencer's CloseSession fence
    /// actually assigned to `session.end` (read from the fence reply, or off
    /// the terminal tail when the runtime fenced itself).
    pub final_seq: u64,
    pub workspace: PathBuf,
    pub model: String,
    /// Durable envelopes retained at close time, ending at `session.end`.
    /// `None` when the ring was already past [`RING_CAP`] (eviction) — a
    /// snapshot without its evicted prefix cannot answer a fresh (no-cursor)
    /// reconnect without silently hiding events, so replay is refused.
    pub retained: Option<Vec<EventEnvelope>>,
    /// The evicted-durable watermark at close time: durable seqs
    /// `<= resume_floor` are unreplayable from this tombstone.
    pub resume_floor: Option<u64>,
}

impl Service {
    /// Ordered teardown of ONE session (`/stop`, shutdown):
    /// `Stopping` FIRST (so concurrent requests see it), then cancel the session token (the RUNTIME emits the interrupted
    /// `turn.end` + `agent.state{Idle}` from the natural outcome and drops
    /// its inbox receiver), WAIT for the runtime task's teardown to
    /// complete, THEN fire the sequencer's `CloseSession` fence — the
    /// SEQUENCER sequences `session.end{final_seq}` as the LAST event — and
    /// the live slot is replaced by a bounded [`Tombstone`]. Idempotent: an
    /// already-Stopping or already-closed session is a no-op.
    pub async fn close_session(&self, id: SessionId) -> Result<(), ServerError> {
        let Some(handle) = self.mark_stopping(id)? else {
            return Ok(());
        };
        self.fence_and_tombstone(id, handle, "stop requested").await;
        Ok(())
    }

    /// Graceful shutdown: stop every active session (each one's fence emits
    /// its `session.end`), tombstone them all, then return so axum finishes
    /// draining.
    pub async fn close_all(&self) {
        let targets: Vec<(SessionId, SessionHandle)> = {
            let mut guard = self.lock_sessions();
            guard
                .iter_mut()
                .filter_map(|(id, slot)| match slot {
                    SessionSlot::Live(handle) if handle.lifecycle == Lifecycle::Active => {
                        handle.lifecycle = Lifecycle::Stopping;
                        Some((*id, handle.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        for (id, handle) in targets {
            self.fence_and_tombstone(id, handle, "shutdown").await;
        }
    }

    /// Teardown order. The RUNTIME owns the terminal tail: the
    /// session-token cancel interrupts the actor, the runtime's natural
    /// outcome path emits `turn.end{interrupted}` then `agent.state{Idle}`,
    /// and its `teardown()` fires the CloseSession fence itself, sequencing
    /// `session.end` as the LAST event. The manager (1) takes a tail probe
    /// (a live subscription) BEFORE cancelling so the terminal tail stays
    /// observable even with no external subscriber, (2) cancels and awaits
    /// the runtime JoinHandle (bounded; on breach the task is ABORTED
    /// because a dropped JoinHandle would only DETACH it), (3) fires an
    /// idempotent backstop fence only when the runtime did not fence
    /// (already-closed is a `SequencerClosed` no-op), and (4) tombstones
    /// with the seq the fence actually assigned to `session.end`. No lock
    /// is held across any await.
    async fn fence_and_tombstone(&self, id: SessionId, handle: SessionHandle, reason: &str) {
        // Tail probe BEFORE the cancel: survives even with no external
        // subscriber and anchors the tombstone's final_seq on the real tail.
        let tail = handle.sequencer.subscribe_since(None).await.ok();
        let tail_resume_floor = tail.as_ref().and_then(|reply| reply.resume_floor);
        handle.session_cancel.cancel();
        drop(handle.inbox_tx);

        // Backstop: after the runtime has settled, the sequencer must be
        // closed. If it is STILL OPEN (a wedged-then-aborted runtime, or a
        // foreign join task that never owned a fence), the manager fences.
        // On the normal path the runtime's teardown already closed it and
        // this is skipped — a second CloseSession would be a SequencerClosed
        // no-op, never a second session.end.
        let mut fenced_seq: Option<u64> = None;
        if let Some(mut task) = handle
            .runtime_task
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
        {
            // Await by reference so the JoinHandle survives the timeout (a
            // dropped JoinHandle DETACHES the task instead of aborting it).
            if tokio::time::timeout(TEARDOWN_AWAIT_CAP, &mut task)
                .await
                .is_err()
            {
                // DEGRADED path (documented): the runtime outlived the cap.
                // Abort it, REAP it (the abort lands at the task's next
                // poll; the await proves it can poll/emit no more), then
                // fence below. The terminal tail
                // (turn.end/agent.state) is lost in this path by
                // definition; it exists only for a wedged/aborted task.
                task.abort();
                let _ = task.await;
            }
        }
        if handle.sequencer.current_seq().await.is_ok() {
            fenced_seq = handle
                .sequencer
                .close_session_with_final_seq(reason)
                .await
                .ok()
                .flatten();
        }

        // The tombstone's final_seq is the seq the fence REALLY assigned:
        // authoritatively from the backstop fence's reply, else read off the
        // terminal tail the runtime produced (its session.end is the last
        // envelope the probe delivered).
        let fenced = fenced_seq.map(|seq| (seq, None));
        let (final_seq, collected_tail) = match fenced {
            Some((seq, collected)) => (seq, collected),
            None => {
                let (seq, collected) = Self::collect_tail_through_session_end(tail).await;
                (seq, Some(collected))
            }
        };

        let retained = match collected_tail {
            // The runtime fenced the session: the tail probe carries the
            // full durable history through session.end — freeze it.
            Some(collected) => collected,
            // The MANAGER fired the backstop fence (wedged/aborted runtime):
            // the probe cannot have seen session.end from the fence, so
            // re-subscribe to the now-closed session's retained ring.
            None => {
                let fresh = handle.sequencer.subscribe_since(None).await.ok();
                Self::collect_tail_through_session_end(fresh).await.1
            }
        };
        self.retain_tombstone(
            id,
            Tombstone {
                final_seq,
                workspace: handle.workspace.clone(),
                model: handle.model.clone(),
                retained,
                resume_floor: tail_resume_floor,
            },
        );
    }

    /// The durable tail of a closing session: replay + live envelopes up to and
    /// including `session.end`, with the seq it landed on. The single collection
    /// the tombstone derives BOTH `final_seq` (runtime-fenced path) and the
    /// frozen `retained` snapshot from — the live receiver is consumed exactly
    /// once. `None` when the ring had already evicted (a partial snapshot must
    /// not masquerade as complete) or the tail never reached `session.end`.
    async fn collect_tail_through_session_end(
        tail: Option<crate::bus::SubscribeReply>,
    ) -> (u64, Option<Vec<EventEnvelope>>) {
        let Some(mut reply) = tail else {
            return (0, None);
        };
        let resume_floor = reply.resume_floor;
        let mut envelopes = match reply.replay {
            crate::bus::ResumeOutcome::Replay(events) => events,
            crate::bus::ResumeOutcome::Resync | crate::bus::ResumeOutcome::InvalidCursor => {
                return (0, None);
            }
        };
        let mut last = envelopes.last().map(|e| e.seq).unwrap_or(0);
        if let Some(mut live) = reply.live.take() {
            while let Ok(Some(env)) = tokio::time::timeout(TEARDOWN_AWAIT_CAP, live.recv()).await {
                let is_end = env.kind.as_str() == "session.end";
                last = last.max(env.seq);
                envelopes.push(env);
                if is_end {
                    break;
                }
            }
        }
        if resume_floor.is_some()
            || !envelopes
                .last()
                .is_some_and(|env| env.kind.as_str() == "session.end")
        {
            return (last, None);
        }
        (last, Some(envelopes))
    }

    /// Marks a live session `Stopping` under the registry lock.
    /// `Ok(None)` when already Stopping/closed (idempotent); 404 when
    /// unknown. The slot stays in the map until the tombstone replaces it,
    /// so a concurrent request sees `Stopping` rather than a hole.
    fn mark_stopping(&self, id: SessionId) -> Result<Option<SessionHandle>, ServerError> {
        let mut guard = self.lock_sessions();
        match guard.get_mut(&id) {
            Some(SessionSlot::Live(handle)) if handle.lifecycle == Lifecycle::Active => {
                handle.lifecycle = Lifecycle::Stopping;
                Ok(Some(handle.clone()))
            }
            Some(_) => Ok(None),
            None => Err(ServerError::SessionNotFound(id.to_string())),
        }
    }

    /// Retains a tombstone, bounded by [`MAX_TOMBSTONES`] with FIFO eviction
    /// of the oldest. The insert REPLACES the live slot — the live routing
    /// resources (`inbox_tx`/`Sequencer`/token) drop with it, since a
    /// tombstone holds none of them. Two short synchronous critical
    /// sections — never across an await.
    fn retain_tombstone(&self, id: SessionId, tombstone: Tombstone) {
        let evicted = {
            let mut guard = self.lock_tombstones();
            guard.push_back(id);
            if guard.len() > MAX_TOMBSTONES {
                guard.pop_front()
            } else {
                None
            }
        };
        {
            let mut guard = self.lock_sessions();
            guard.insert(id, SessionSlot::Tombstone(tombstone));
            if let Some(oldest) = evicted {
                guard.remove(&oldest);
            }
        }
    }

    /// Removes a session from the registry AND the tombstone queue: the
    /// DELETE route's memory half. Call after `close_session` (live) or on
    /// a disk-only id (no memory slot at all).
    pub fn discard(&self, id: SessionId) {
        self.lock_sessions().remove(&id);
        self.lock_tombstones().retain(|other| *other != id);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use ulid::Ulid;

    use super::*;
    use crate::agent::actor::AgentActor;
    use crate::bus::event::ROOT_ACTOR_ID;
    use crate::bus::sequencer::Sequencer;
    use crate::bus::SessionBus;
    use crate::session::actor::start_candidate;
    use crate::session::service::tests::test_config;
    use crate::tools::{Registry, ToolContext};

    /// A real, never-driven actor: the stand-in result of the detached
    /// runtime task the registry-level tests below construct handles with.
    fn idle_actor() -> AgentActor {
        use crate::provider::config::{ProviderConfig, ReasoningHistory};
        use std::collections::BTreeMap;
        AgentActor::new(
            Registry::new(),
            ToolContext {
                workspace: std::env::temp_dir(),
                writable_roots: vec![std::env::temp_dir()],
            },
            ProviderConfig {
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 100,
                request_timeout_ms: 100,
            },
            "test-key".into(),
            "test-model".into(),
            ReasoningHistory::Preserve,
            Some(64),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    // Given a live session closed through the manager (fence + tombstone)
    // When GET asks for it afterwards
    // Then it answers from the tombstone (stopped, final_seq == the
    //      session.end slot) and is NOT routable; an unknown id is a 404
    #[tokio::test]
    async fn closed_session_answers_from_tombstone_and_is_not_routable() {
        let state = Service::new(test_config("127.0.0.1:0", false));
        let id = Ulid::new();
        assert!(matches!(
            state.session_summary(id).await,
            Err(ServerError::SessionNotFound(_))
        ));

        let handle = SessionHandle {
            inbox_tx: mpsc::channel(1).0,
            sequencer: Sequencer::spawn(Ulid::new(), SessionBus::default(), 0),
            session_cancel: CancellationToken::new(),
            runtime_task: Arc::new(Mutex::new(Some(tokio::spawn(async { idle_actor() })))),
            actor_id: ROOT_ACTOR_ID.to_string(),
            lifecycle: Lifecycle::Active,
            actor_state: "idle".to_string(),
            workspace: PathBuf::from("/w"),
            model: "fast".to_string(),
        };
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("open sequencer accepts the start candidate");
        state
            .lock_sessions()
            .insert(id, SessionSlot::Live(handle.clone()));

        state.close_session(id).await.expect("close");

        let summary = state.session_summary(id).await.expect("tombstone answers");
        assert_eq!(summary.actor_state, "stopped");
        assert_eq!(summary.current_seq, 1, "session.end is seq 1");
        assert!(state.handle(id).is_none(), "tombstone is not routable");
        assert_eq!(state.tombstone_count(), 1);
    }

    // Given more closes than MAX_TOMBSTONES
    // When tombstones are retained
    // Then the pool is bounded at MAX_TOMBSTONES and the OLDEST ids are
    //      evicted to the structured 404 while the newest still answers
    #[tokio::test]
    async fn tombstone_pool_is_bounded_by_max_tombstones_fifo() {
        let state = Service::new(test_config("127.0.0.1:0", false));
        let ids: Vec<SessionId> = (0..MAX_TOMBSTONES + 3).map(|_| Ulid::new()).collect();
        for (i, id) in ids.iter().enumerate() {
            state.retain_tombstone(
                *id,
                Tombstone {
                    final_seq: i as u64,
                    workspace: PathBuf::from("/w"),
                    model: "m".to_string(),
                    retained: None,
                    resume_floor: None,
                },
            );
        }
        assert_eq!(state.tombstone_count(), MAX_TOMBSTONES);
        assert_eq!(state.session_count(), MAX_TOMBSTONES);
        for id in &ids[..3] {
            assert!(matches!(
                state.session_summary(*id).await,
                Err(ServerError::SessionNotFound(_))
            ));
        }
        let newest = ids.last().expect("newest id");
        let summary = state.session_summary(*newest).await.expect("retained");
        assert_eq!(summary.actor_state, "stopped");
    }

    // ---- defect-fix proofs (virtual time: deterministic, no sleeps) ----

    /// A registry handle whose runtime task is a stand-in: parks forever
    /// (never fences) — the "wedged/foreign runtime" the cap+backstop path
    /// exists for.
    fn wedged_runtime_handle() -> SessionHandle {
        SessionHandle {
            inbox_tx: mpsc::channel(1).0,
            sequencer: Sequencer::spawn(Ulid::new(), SessionBus::default(), 0),
            session_cancel: CancellationToken::new(),
            runtime_task: Arc::new(Mutex::new(Some(tokio::spawn(async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                idle_actor()
            })))),
            actor_id: ROOT_ACTOR_ID.to_string(),
            lifecycle: Lifecycle::Active,
            actor_state: "idle".to_string(),
            workspace: PathBuf::from("/w"),
            model: "fast".to_string(),
        }
    }

    // Given a runtime wedged mid-turn (it outlives the teardown await cap)
    // When close_session runs
    // Then the wedged task is ABORTED (not detached), the manager's backstop
    //      fence fires and REPLIES the final_seq, the tombstone records it,
    //      session.end is the LAST event, and nothing is emitted after it
    #[tokio::test(flavor = "current_thread")]
    async fn wedged_runtime_is_aborted_and_backstop_fenced_with_exact_final_seq() {
        // Virtual time: the 10s teardown cap elapses instantly while the
        // wedged stand-in (which awaits nothing) stays parked.
        tokio::time::pause();
        let state = Service::new(test_config("127.0.0.1:0", false));
        let id = Ulid::new();
        let handle = wedged_runtime_handle();
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("start");
        state
            .lock_sessions()
            .insert(id, SessionSlot::Live(handle.clone()));

        state.close_session(id).await.expect("close");

        let summary = state.session_summary(id).await.expect("tombstone");
        assert_eq!(summary.actor_state, "stopped");
        assert!(
            summary.current_seq >= 1,
            "session.end was sequenced by the backstop: {}",
            summary.current_seq
        );
        assert!(state.handle(id).is_none());
    }

    // Given a runtime that tears down faithfully (emits its terminal tail,
    // fires the CloseSession fence from ITS teardown, then exits) — the
    // production stop ordering
    // When close_session awaits that runtime and tombstones
    // Then the tombstone's final_seq EQUALS the seq the fence stamped on
    //      session.end (defect-2 proof) and exactly one session.end exists
    #[tokio::test]
    async fn tombstone_final_seq_equals_the_fenced_session_end_seq() {
        use crate::bus::event::EventKind;
        use crate::bus::sequencer::Candidate;

        let state = Service::new(test_config("127.0.0.1:0", false));
        let id = Ulid::new();
        let mut handle = wedged_runtime_handle();
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("start");
        // The faithful runtime teardown: late tail event, THEN the fence,
        // THEN the task exits — all after the manager's cancel.
        let seq = handle.sequencer.clone();
        handle.runtime_task = Arc::new(Mutex::new(Some(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = seq.publish(Candidate::new(
                EventKind::TurnEnd,
                serde_json::json!({ "outcome": "interrupted" }),
            ));
            let _ = seq.close_session("runtime teardown");
            idle_actor()
        }))));
        state
            .lock_sessions()
            .insert(id, SessionSlot::Live(handle.clone()));

        // The TEST's own probe observes the real session.end payload.
        let observer = handle.sequencer.subscribe_since(None).await.unwrap();
        state.close_session(id).await.expect("close");

        let observed_end_seq = collect_session_end_seq(observer).await;
        let summary = state.session_summary(id).await.expect("tombstone GET");
        assert_eq!(
            summary.current_seq, observed_end_seq,
            "tombstone final_seq == session.end.payload.final_seq"
        );
    }

    /// Drains an observer probe (replay + live) until session.end lands and
    /// returns its seq.
    async fn collect_session_end_seq(mut probe: crate::bus::SubscribeReply) -> u64 {
        let mut end_seq = 0;
        if let crate::bus::ResumeOutcome::Replay(replay) = &probe.replay {
            for env in replay {
                if env.kind.as_str() == "session.end" {
                    end_seq = env.seq;
                }
            }
        }
        if let Some(mut live) = probe.live.take() {
            while let Ok(Some(env)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), live.recv()).await
            {
                if env.kind.as_str() == "session.end" {
                    end_seq = env.seq;
                    break;
                }
            }
        }
        end_seq
    }

    // Given a stopped session
    // When GET asks for its summary and a command route asks for its handle
    // Then GET answers from the tombstone (stopped @ final_seq) while the
    //     handle is gone: a tombstone answers reads, never commands.
    #[tokio::test]
    async fn stopped_session_answers_get_but_is_not_routable() {
        let state = Service::new(test_config("127.0.0.1:0", false));
        let id = Ulid::new();
        let handle = wedged_runtime_handle();
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("start");
        state
            .lock_sessions()
            .insert(id, SessionSlot::Live(handle.clone()));

        state.close_session(id).await.expect("close");

        let summary = state.session_summary(id).await.expect("tombstone GET");
        assert_eq!(summary.actor_state, "stopped");
        assert!(state.handle(id).is_none(), "not routable for streaming");
    }
}
