use serde::Serialize;
use tokio::sync::oneshot;

use crate::bus::event::EventKind;
use crate::bus::sequencer::{AckedCommit, Candidate, Sequencer};

/// Who changed the setting; the server assigns it, a caller never names it.
#[derive(Debug, Clone, Copy)]
pub enum BypassOrigin {
    Http,
    Cli,
    /// The root session this child follows.
    Parent(ulid::Ulid),
    /// A resumed session starting over from off.
    Resume,
}

impl BypassOrigin {
    pub fn by(self) -> String {
        match self {
            Self::Http => "user:http".into(),
            Self::Cli => "user:cli".into(),
            Self::Parent(id) => format!("parent:{id}"),
            Self::Resume => "system:resume".into(),
        }
    }
}

/// The error of a setter that reached a session already going away.
pub const STOPPING: &str = "session stopping";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BypassReply {
    pub enabled: bool,
    pub changed: bool,
    pub bypass_seq: u64,
}

pub struct BypassRequest {
    pub enabled: bool,
    pub origin: BypassOrigin,
    pub commit: AckedCommit,
    pub reply: oneshot::Sender<Result<BypassReply, String>>,
}

impl std::fmt::Debug for BypassRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BypassRequest")
            .field("enabled", &self.enabled)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

pub(super) async fn apply(
    status: &super::RuntimeStatus,
    sequencer: &Sequencer,
    request: BypassRequest,
    pending: Option<(
        &crate::agent::actor::AgentActor,
        &crate::bus::inbox::ActorInbox,
    )>,
) {
    let state = *status.bypass.lock().unwrap_or_else(|e| e.into_inner());
    let result = if state.enabled == request.enabled {
        Ok(BypassReply {
            enabled: state.enabled,
            changed: false,
            bypass_seq: state.seq,
        })
    } else {
        sequencer
            .commit_acked(
                Candidate::new(
                    EventKind::SessionBypass,
                    serde_json::json!({
                        "enabled": request.enabled, "by": request.origin.by()
                    }),
                ),
                request.commit,
            )
            .await
            .map(|seq| {
                *status.bypass.lock().unwrap_or_else(|e| e.into_inner()) =
                    crate::agent::actor::BypassState {
                        enabled: request.enabled,
                        seq,
                    };
                BypassReply {
                    enabled: request.enabled,
                    changed: true,
                    bypass_seq: seq,
                }
            })
            .map_err(|error| error.to_string())
    };
    if result.is_ok() {
        if let Some((actor, inbox)) = pending {
            status.resolve_pending_bypass(actor, inbox);
        }
    }
    let _ = request.reply.send(result);
}

impl super::RuntimeStatus {
    pub(super) fn resolve_pending_bypass(
        &self,
        actor: &crate::agent::actor::AgentActor,
        inbox: &crate::bus::inbox::ActorInbox,
    ) {
        if !self
            .bypass
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enabled
            || !crate::agent::run_loop::pending_bypass_eligible(actor)
        {
            return;
        }
        let Some(request) = actor.pending_permission() else {
            return;
        };
        if !self.claim_permission(request.id) {
            return;
        }
        let (reply, _) = oneshot::channel();
        if inbox
            .permission_sender()
            .try_send(crate::bus::inbox::PermissionAnswer {
                request_id: request.id,
                decision: crate::agent::permissions::PermissionDecision::AllowOnce,
                origin: crate::agent::permissions::PermissionOrigin::Bypass,
                reply,
            })
            .is_err()
        {
            self.release_permission(request.id);
        }
    }

    pub async fn set_bypass(
        &self,
        enabled: bool,
        origin: BypassOrigin,
        commit: AckedCommit,
    ) -> Result<BypassReply, String> {
        let (reply, received) = oneshot::channel();
        self.controls
            .tx
            .send(super::Control::Bypass(BypassRequest {
                enabled,
                origin,
                commit,
                reply,
            }))
            .map_err(|_| STOPPING.to_string())?;
        received.await.map_err(|_| STOPPING.to_string())?
    }
}
