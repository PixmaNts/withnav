//! Editor context and lifecycle callbacks the host injects into the
//! [`Service`](super::Service). Model selection belongs to agent configuration,
//! never to these actor setup and conversation attachment hooks.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use super::sender::SessionSender;
use super::service::SessionId;
use super::subscription::Subscription;
use crate::agent::actor::AgentActor;

/// What a host learns about a session that just became routable.
#[derive(Debug, Clone)]
pub struct StartedSession {
    pub id: SessionId,
    pub editor: bool,
    pub workspace: PathBuf,
    /// Rebuilt from storage rather than freshly created: the subscription's
    /// replay then holds the past conversation.
    pub resumed: bool,
}

/// Editor-specific behaviour of a session, supplied at `Service`
/// construction.
pub trait SessionHooks: Send + Sync + std::fmt::Debug {
    /// Editor-specific actor setup: role prompt, extra tools (through
    /// `actor.registry`) and repair of the restored `actor.transcript`.
    /// Runs after the transcript is restored and before the pending ask is.
    fn configure_actor(&self, editor: bool, actor: &mut AgentActor);

    /// A session is live and routable. `subscription` replays the stored
    /// events (just `session.start` for a fresh session), then goes live;
    /// `cancel` fires when this incarnation of the session ends. Called
    /// synchronously: spawn a task for anything that waits.
    fn started(
        &self,
        _info: StartedSession,
        _sender: SessionSender,
        _subscription: Subscription,
        _cancel: CancellationToken,
    ) {
    }
}

/// No editor behaviour: the plain agent, for `run` and tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl SessionHooks for NoHooks {
    fn configure_actor(&self, _editor: bool, _actor: &mut AgentActor) {}
}
