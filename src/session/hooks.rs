//! Actor-profile policy the host injects into the [`Service`](super::Service):
//! which model a profile defaults to and how its actor is set up. The
//! service itself knows no profile by name.

use crate::agent::actor::AgentActor;

/// Profile-specific behaviour of a session, supplied at `Service`
/// construction. Synchronous: it only shapes the actor before it runs.
pub trait SessionHooks: Send + Sync + std::fmt::Debug {
    /// The model alias a profile uses when the request names none.
    fn default_model(&self, profile: Option<&str>) -> Option<&str>;

    /// Profile-specific actor setup: role prompt, extra tools (through
    /// `actor.registry`) and repair of the restored `actor.transcript`.
    /// Runs after the transcript is restored and before the pending ask is.
    fn configure_actor(&self, profile: Option<&str>, actor: &mut AgentActor);
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
