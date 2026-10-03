//! The log lines a session writes when its local instructions and its skills
//! are loaded (at creation and at resume): one per source or file.

use super::service::SessionId;
use crate::agent::definitions::Agents;
use crate::agent::instructions::Instructions;
use crate::agent::skills::Skills;

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

pub(super) fn log_skills(id: SessionId, resumed: bool, skills: &Skills) {
    let when = if resumed { "resume" } else { "create" };
    tracing::info!(
        session_id = %id,
        on = when,
        skills = skills.len(),
        diagnostics = skills.diagnostics().len(),
        "session skills loaded"
    );
    for d in skills.diagnostics() {
        if d.is_problem() {
            tracing::warn!(session_id = %id, path = %d.path.display(), "skills: {}", d.kind);
        } else {
            tracing::info!(session_id = %id, path = %d.path.display(), "skills: {}", d.kind);
        }
    }
}

pub(super) fn log_agents(id: SessionId, resumed: bool, agents: &Agents) {
    let when = if resumed { "resume" } else { "create" };
    tracing::info!(
        session_id = %id,
        on = when,
        agents = agents.all().count(),
        diagnostics = agents.diagnostics().len(),
        "session agents loaded"
    );
    for d in agents.diagnostics() {
        tracing::warn!(session_id = %id, path = %d.path.display(), "agents: {}", d.message);
    }
}
