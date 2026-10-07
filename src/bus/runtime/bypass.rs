use serde::Serialize;
use tokio::sync::oneshot;

use crate::bus::event::EventKind;
use crate::bus::sequencer::{AckedCommit, Candidate, Sequencer};

#[derive(Debug, Clone, Copy)]
pub enum BypassOrigin {
    Http,
    Cli,
}

impl BypassOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "user:http",
            Self::Cli => "user:cli",
        }
    }
}

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
                        "enabled": request.enabled, "by": request.origin.as_str()
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
    let _ = request.reply.send(result);
}

impl super::RuntimeStatus {
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
            .map_err(|_| "session stopping".to_string())?;
        received.await.map_err(|_| "session stopping".to_string())?
    }
}
