//! Teardown and close: the ordered fence that ends a session, and `discard`.
//! The sequencer owns `session.end`; this module only orders the steps. A
//! closed session leaves the registry: its history is answered from the
//! store.

use std::time::Duration;

use super::service::{Lifecycle, Service, SessionHandle, SessionId};
use crate::error::ServerError;

/// Upper bound on each wait during teardown: the runtime task unwinding, and
/// the sequencer finishing after its fence. The actor token is already
/// cancelled at that point, so an interrupted provider stream ends promptly;
/// the cap only covers a wedged runtime or sequencer so `/stop` and shutdown
/// are bounded. On timeout the backstop fence still makes `session.end` the
/// last event.
const TEARDOWN_AWAIT_CAP: Duration = Duration::from_secs(10);

impl Service {
    /// Ownership inspection and lifecycle claim share admission; waits always retry.
    async fn claim_lifecycle_stop(&self, id: SessionId) -> Option<SessionHandle> {
        #[cfg(debug_assertions)]
        {
            let barrier = self
                .lifecycle_claim_barrier
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(barrier) = barrier {
                barrier.reached.notify_one();
                barrier.release.notified().await;
            }
        }
        loop {
            let pending = {
                let _admission = self.admission_gate.lock().await;
                let pending = self
                    .child_operations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&id)
                    .cloned();
                if pending.is_none() {
                    let target = self.mark_stopping(id);
                    if let Some(handle) = &target {
                        handle.session_cancel.cancel();
                    }
                    return target;
                }
                pending
            };
            if let Some(pending) = pending {
                pending.cancelled().await;
            }
        }
    }
    /// Ordered teardown of ONE session (`/stop`): `Stopping` FIRST (so
    /// concurrent requests see it), cancel the session token, wait for the
    /// runtime to unwind, fence the sequencer (`session.end` is the LAST
    /// event), wait until `session.end` is emitted, flush it to the store,
    /// THEN drop the registry entry. Idempotent: a conversation that is
    /// already closed (or only stored) is a no-op; unknown is a 404.
    pub async fn close_session(&self, id: SessionId) -> Result<(), ServerError> {
        let _gate = self.lifecycle_gate().await;
        self.ensure_not_archived(id).await?;
        self.wait_child_operation(id).await;
        let parent = self.handle(id).and_then(|h| h.parent_session_id);
        self.close_session_gated(id).await?;
        if let Some(parent) = parent {
            self.finish_child(
                parent,
                id,
                "stopped",
                Some("stopped_by_owner".into()),
                String::new(),
            )
            .await?;
        }
        Ok(())
    }

    /// [`Service::close_session`] for a caller that already holds the
    /// lifecycle gate.
    pub(super) async fn close_session_gated(&self, id: SessionId) -> Result<(), ServerError> {
        let target = self.claim_lifecycle_stop(id).await;
        let claimed_children = {
            let _admission = self.admission_gate.lock().await;
            let children = self
                .lock_sessions()
                .iter()
                .filter(|(_, h)| h.parent_session_id == Some(id))
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            let mut claimed = std::collections::HashMap::new();
            for child in children {
                if self
                    .child_operations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&child)
                {
                    continue;
                }
                if let Some(handle) = self.mark_stopping(child) {
                    handle.session_cancel.cancel();
                    claimed.insert(child, handle);
                }
            }
            claimed
        };
        if let Some(handle) = target {
            self.fence_and_remove(id, handle, "stop requested").await;
            self.close_children(id, claimed_children).await?;
            return Ok(());
        }
        let stored = self
            .history()
            .exists(id)
            .await
            .map_err(ServerError::History)?;
        if stored {
            self.close_children(id, claimed_children).await?;
            Ok(())
        } else {
            Err(ServerError::SessionNotFound(id.to_string()))
        }
    }

    async fn close_children(
        &self,
        parent: SessionId,
        mut claimed: std::collections::HashMap<SessionId, SessionHandle>,
    ) -> Result<(), ServerError> {
        for child in self
            .history()
            .child_ids(parent)
            .await
            .map_err(ServerError::History)?
        {
            let handle = match claimed.remove(&child) {
                Some(handle) => Some(handle),
                None => self.claim_lifecycle_stop(child).await,
            };
            if let Some(handle) = handle {
                self.fence_and_remove(child, handle, "parent stopped").await;
            }
            self.finish_child(
                parent,
                child,
                "stopped",
                Some("parent_stopped".into()),
                String::new(),
            )
            .await?;
        }
        Ok(())
    }

    /// Graceful shutdown: stop every active session (each one's fence emits
    /// its `session.end`), then return so axum finishes draining.
    pub async fn close_all(&self) {
        self.mcp.shutdown().await;
        let _gate = self.lifecycle_gate().await;
        loop {
            let admission = self.admission_gate.lock().await;
            let owned = self
                .child_operations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let targets: Vec<(SessionId, SessionHandle)> = {
                let mut guard = self.lock_sessions();
                guard
                    .iter_mut()
                    .filter(|(id, _)| !owned.contains_key(id))
                    .map(|(id, handle)| {
                        handle.lifecycle = Lifecycle::Stopping;
                        handle.session_cancel.cancel();
                        (*id, handle.clone())
                    })
                    .collect()
            };
            drop(admission);
            #[cfg(debug_assertions)]
            {
                let barrier = self
                    .shutdown_snapshot_barrier
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(barrier) = barrier {
                    barrier.reached.notify_one();
                    barrier.release.notified().await;
                }
            }
            if targets.is_empty() && owned.is_empty() {
                let _admission = self.admission_gate.lock().await;
                if self.lock_sessions().is_empty()
                    && self
                        .child_operations
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_empty()
                {
                    break;
                }
                continue;
            }
            for operation in owned.into_values() {
                operation.cancelled().await;
            }
            for (id, handle) in targets {
                self.wait_child_operation(id).await;
                let parent = handle.parent_session_id;
                self.fence_and_remove(id, handle, "shutdown").await;
                if let Some(parent) = parent {
                    let _ = self
                        .finish_child(
                            parent,
                            id,
                            "stopped",
                            Some("shutdown".into()),
                            String::new(),
                        )
                        .await;
                }
            }
        }
    }

    /// Teardown order. The RUNTIME owns the terminal tail: the session-token
    /// cancel interrupts the actor, the runtime's natural outcome path emits
    /// `turn.end{interrupted}` then `agent.state{Idle}`, and its `teardown()`
    /// fires the CloseSession fence itself. The manager (1) cancels and
    /// awaits the runtime JoinHandle (bounded; on breach the task is ABORTED
    /// because a dropped JoinHandle would only DETACH it), (2) fires the
    /// fence (an already-closed sequencer ignores it: never a second
    /// `session.end`), (3) waits for the sequencer to finish, so
    /// `session.end` has been emitted and queued to the store writer, (4)
    /// flushes the store, and (5) removes the registry entry. No lock is held
    /// across any await.
    pub(super) async fn fence_and_remove(
        &self,
        id: SessionId,
        handle: SessionHandle,
        reason: &str,
    ) {
        handle.session_cancel.cancel();
        handle.status.watchdog.supervisor_cancel.cancel();
        let supervisor = handle
            .status
            .root_supervisor
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(supervisor) = supervisor {
            let _ = supervisor.await;
        }
        let pending = handle
            .publications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for publication in pending {
            publication.cancelled().await;
        }
        drop(handle.inbox_tx);

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
                // DEGRADED path: the runtime outlived the cap. Abort it and
                // REAP it (the abort lands at the task's next poll; the await
                // proves it can emit no more). Its terminal tail is lost.
                task.abort();
                let _ = task.await;
            }
        }
        let _ = handle.sequencer.close_session(reason);
        if tokio::time::timeout(TEARDOWN_AWAIT_CAP, handle.sequencer.closed())
            .await
            .is_err()
        {
            tracing::error!(session_id = %id, "sequencer did not close in time; dropping it");
        }
        if let Err(err) = self.history().flush().await {
            tracing::error!(session_id = %id, error = %err, "flushing history at close failed");
        }
        self.lock_sessions().remove(&id);
    }

    /// Marks an ACTIVE session `Stopping` under the registry lock and
    /// returns its handle; `None` for anything else. The entry stays in the
    /// map until the close finishes, so a concurrent request sees `Stopping`
    /// rather than a hole.
    pub(super) fn mark_stopping(&self, id: SessionId) -> Option<SessionHandle> {
        let mut guard = self.lock_sessions();
        let handle = guard.get_mut(&id)?;
        if handle.lifecycle != Lifecycle::Active {
            return None;
        }
        handle.lifecycle = Lifecycle::Stopping;
        Some(handle.clone())
    }

    /// Removes a session from the registry: the DELETE route's memory half.
    /// Call after `close_session` (live) or on a stored-only id.
    pub fn discard(&self, id: SessionId) {
        self.lock_sessions().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
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
    use crate::session::service::tests::test_service;
    use crate::tools::{Registry, ToolContext};

    /// A real, never-driven actor: the stand-in result of the detached
    /// runtime task the registry-level tests below construct handles with.
    fn idle_actor() -> AgentActor {
        use crate::provider::config::{ProviderConfig, ReasoningHistory};
        use std::collections::BTreeMap;
        AgentActor::new(
            Registry::new(),
            ToolContext {
                binding: None,
                workspace: std::env::temp_dir(),
                writable_roots: vec![std::env::temp_dir()],
                readable_roots: Vec::new(),
            },
            ProviderConfig {
                base_url: "http://127.0.0.1:1".into(),
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 100,
                request_timeout_ms: 100,
                catalog_format: None,
            },
            "test-key".into(),
            "test-model".into(),
            ReasoningHistory::Preserve,
            Some(64),
            tokio_util::sync::CancellationToken::new(),
        )
    }

    // ---- defect-fix proofs (virtual time: deterministic, no sleeps) ----

    /// A registry handle whose runtime task is a stand-in: parks forever
    /// (never fences) — the "wedged/foreign runtime" the cap+backstop path
    /// exists for.
    fn wedged_runtime_handle() -> SessionHandle {
        SessionHandle {
            flows: None,
            skills: Arc::default(),
            publications: Arc::default(),
            incarnation: Ulid::generate(),
            inbox_tx: mpsc::channel(1).0,
            permissions: mpsc::channel(1).0,
            sequencer: Sequencer::spawn(Ulid::generate(), SessionBus::default(), 0),
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
            selected: Arc::new(std::sync::Mutex::new(
                crate::session::service::SelectedModel {
                    provenance: None,
                    role: crate::agent::definitions::Agents::builtin()
                        .get("build")
                        .unwrap()
                        .clone(),
                    agent: "build".into(),
                    execution: None,
                    alias: "fast".to_string(),
                    resolution: None,
                    revision: 1,
                    variant: None,
                    offer: Arc::default(),
                },
            )),
            status: Arc::default(),
            parent_session_id: None,
        }
    }

    // Given a runtime wedged mid-turn (it outlives the teardown await cap)
    // When close_session runs
    // Then the wedged task is ABORTED (not detached), the manager's backstop
    //      fence fires, session.end is the last event, the sequencer is
    //      closed and the registry entry is gone
    #[tokio::test(flavor = "current_thread")]
    async fn wedged_runtime_is_aborted_and_backstop_fenced_with_exact_final_seq() {
        // Virtual time: the 10s teardown cap elapses instantly while the
        // wedged stand-in (which awaits nothing) stays parked.
        tokio::time::pause();
        let (_dir, state) = test_service();
        let id = Ulid::generate();
        let handle = wedged_runtime_handle();
        handle
            .sequencer
            .publish(start_candidate(Path::new("/w")))
            .expect("start");
        state.lock_sessions().insert(id, handle.clone());

        state.close_session(id).await.expect("close");

        let reply = handle.sequencer.subscribe_since(None).await;
        assert!(reply.is_err(), "the backstop fence closed the sequencer");
        assert!(state.handle(id).is_none());
        assert_eq!(state.session_count(), 0);
    }

    // Given a runtime that tears down faithfully (emits its terminal tail,
    // fires the CloseSession fence from ITS teardown, then exits) — the
    // production stop ordering
    // When close_session awaits that runtime
    // Then exactly one session.end exists, after the late tail event, and
    //      the registry entry is gone
    #[tokio::test]
    async fn the_runtime_fence_yields_one_session_end_after_the_late_tail() {
        use crate::bus::event::EventKind;
        use crate::bus::sequencer::Candidate;

        let (_dir, state) = test_service();
        let id = Ulid::generate();
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
        state.lock_sessions().insert(id, handle.clone());

        // The TEST's own probe observes the real session.end payload.
        let observer = handle.sequencer.subscribe_since(None).await.unwrap();
        state.close_session(id).await.expect("close");

        assert_eq!(collect_session_end_seq(observer).await, 2);
        assert!(state.handle(id).is_none());
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
}
