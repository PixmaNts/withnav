//! Carries the MCP servers' status to the conversations: one task watches the service's
//! [`McpManager`](crate::mcp::McpManager) and publishes each new snapshot as a live-only
//! `mcp.status` event on every live session's stream.
//!
//! The status is global to the service, the bus is per session: a client following one
//! conversation hears every change while it is connected, and reads `GET /v1/mcp` when it
//! connects (live-only events are not replayed). Changes close together are sent once, as the
//! latest snapshot: this is a gauge, not a log of transitions.
//!
//! The same wake-up asks every root session to refresh its MCP tools; the session's runtime
//! does it between two turns (see `bus::runtime::mcp_tools`). A child keeps the tools it was
//! created with.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use tokio_util::sync::CancellationToken;

use super::service::{SessionHandle, SessionId};
use crate::bus::event::EventKind;
use crate::bus::runtime::mcp_tools::McpRefresh;
use crate::bus::sequencer::Candidate;
use crate::mcp::McpManager;

/// The running bridge; [`McpBridge::shutdown`] stops it and waits for it.
#[derive(Debug)]
pub(crate) struct McpBridge {
    cancel: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl McpBridge {
    /// Starts the task. It holds the session registry weakly, so it never keeps a dropped
    /// service alive; it ends on [`McpBridge::shutdown`] or once the registry is gone.
    pub(crate) fn start(
        mcp: &Arc<McpManager>,
        sessions: &Arc<Mutex<HashMap<SessionId, SessionHandle>>>,
    ) -> Self {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            Arc::downgrade(mcp),
            mcp.watch(),
            Arc::downgrade(sessions),
            cancel.clone(),
        ));
        Self {
            cancel,
            task: Mutex::new(Some(task)),
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel.cancel();
        let task = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for McpBridge {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn run(
    mcp: Weak<McpManager>,
    mut changed: tokio::sync::watch::Receiver<u64>,
    sessions: Weak<Mutex<HashMap<SessionId, SessionHandle>>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            result = changed.changed() => if result.is_err() { return },
        }
        let (Some(mcp), Some(sessions)) = (mcp.upgrade(), sessions.upgrade()) else {
            return;
        };
        // One snapshot per change, whatever the number of sessions.
        let payload = match serde_json::to_value(mcp.status_snapshot()) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(%error, "MCP status not serializable");
                continue;
            }
        };
        // The sequencers are cloned under the lock and published to after it is released.
        let targets: Vec<_> = sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|handle| {
                (
                    handle.sequencer.clone(),
                    handle
                        .parent_session_id
                        .is_none()
                        .then(|| (handle.status.clone(), handle.mcp_tools.clone())),
                )
            })
            .collect();
        drop(sessions);
        for (sequencer, root) in targets {
            // A session closing meanwhile refuses it; that is not an error for a gauge.
            let _ = sequencer.publish(Candidate::new(EventKind::McpStatus, payload.clone()));
            if let Some((status, installed)) = root {
                status.refresh_mcp(McpRefresh {
                    manager: mcp.clone(),
                    installed,
                });
            }
        }
    }
}
