//! Profile policy and lifecycle callbacks the host injects into the
//! [`Service`](super::Service): which model a profile defaults to, how its
//! actor is set up, and what to attach once a session runs. The service
//! itself knows no profile by name.

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
    pub profile: Option<String>,
    pub workspace: PathBuf,
    /// Rebuilt from storage rather than freshly created: the subscription's
    /// replay then holds the past conversation.
    pub resumed: bool,
}

/// Profile-specific behaviour of a session, supplied at `Service`
/// construction.
pub trait SessionHooks: Send + Sync + std::fmt::Debug {
    /// The model alias a profile uses when the request names none.
    fn default_model(&self, profile: Option<&str>) -> Option<&str>;

    /// Profile-specific actor setup: role prompt, extra tools (through
    /// `actor.registry`) and repair of the restored `actor.transcript`.
    /// Runs after the transcript is restored and before the pending ask is.
    fn configure_actor(&self, profile: Option<&str>, actor: &mut AgentActor);

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

/// No profile behaviour: the plain agent, for `run` and tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl SessionHooks for NoHooks {
    fn default_model(&self, _profile: Option<&str>) -> Option<&str> {
        None
    }

    fn configure_actor(&self, _profile: Option<&str>, _actor: &mut AgentActor) {}
}
