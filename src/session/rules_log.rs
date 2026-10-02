//! The one log line per source a session writes when its local instructions
//! are loaded (at creation and at resume).

use super::service::SessionId;
use crate::agent::instructions::Instructions;

pub(super) fn log(id: SessionId, resumed: bool, rules: &Instructions) {
    let when = if resumed { "resume" } else { "create" };
    tracing::info!(
        session_id = %id,
        on = when,
        sources = rules.sources().len(),
        loaded_bytes = rules.loaded_bytes(),
        "session instructions loaded"
    );
    for source in rules.sources() {
        if source.is_problem() {
            tracing::warn!(session_id = %id, path = %source.path.display(), "instructions: {}", source.status);
        } else {
            tracing::info!(session_id = %id, path = %source.path.display(), "instructions: {}", source.status);
        }
    }
}
