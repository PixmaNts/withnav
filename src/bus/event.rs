//! The event envelope, the event-kind catalog and every payload live in `withnav-client-api`, the
//! wire contract shared with clients; this module re-exports them under their historical path.
//! What stays here is engine logic that needs host types.

pub use withnav_client_api::event::*;

impl From<&crate::agent::permissions::PermissionDecision> for PermissionResolution {
    fn from(decision: &crate::agent::permissions::PermissionDecision) -> Self {
        use crate::agent::permissions::PermissionDecision as Decision;
        match decision {
            Decision::AllowOnce => Self::AllowOnce,
            Decision::AllowAlways => Self::AllowAlways {
                always_saved: true,
                always_error: None,
            },
            Decision::Reject { message } => Self::Reject {
                message: message.clone(),
            },
            Decision::AllowFolder => Self::AllowFolder,
            Decision::AllowConfigSession => Self::AllowConfigSession,
            Decision::AllowSkillMachine => Self::AllowSkillMachine,
            Decision::Deny { message } => Self::Deny {
                message: message.clone(),
            },
        }
    }
}
