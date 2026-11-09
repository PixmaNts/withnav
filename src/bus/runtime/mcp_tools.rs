//! Refreshing a session's MCP tools between two turns. The registry is what the model is
//! offered and what dispatch runs, so it only changes while nothing runs: a turn, a held
//! `ask` and a held permission request all keep the tools they started with.

use std::sync::Arc;

use serde_json::json;
use ulid::Ulid;

use crate::agent::actor::AgentActor;
use crate::bus::event::EventKind;
use crate::bus::sequencer::{Candidate, Sequencer};
use crate::mcp::{InstalledTools, McpManager, ToolChange};
use crate::tools::Tool;

/// Names listed in one note; the rest are counted.
const NOTE_NAMES: usize = 20;

/// Asks the runtime to reconcile a session's tools with the servers' current inventory.
/// The inventory is read when the refresh runs, not when it is asked, so a request that
/// waited for a turn never installs an older list.
pub struct McpRefresh {
    pub manager: Arc<McpManager>,
    pub installed: InstalledTools,
}

impl std::fmt::Debug for McpRefresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpRefresh")
    }
}

/// Installs the servers' current tools into the actor's registry and tells the Captain what
/// was added or removed. The role's own tool policy still filters on top of the registry.
pub(super) fn apply(actor: &mut AgentActor, sequencer: &Sequencer, request: McpRefresh) {
    let Some(change) = request.installed.refresh(request.manager.inventory()) else {
        return;
    };
    let ToolChange {
        tools,
        added,
        removed,
        updated,
        warnings,
    } = &change;
    // A removed or updated tool leaves the registry first, so a name a server dropped can no
    // longer be dispatched.
    let leaving: std::collections::HashSet<&str> =
        removed.iter().chain(updated).map(String::as_str).collect();
    actor.registry.retain(|name| !leaving.contains(name));
    for tool in tools.iter() {
        if added.iter().chain(updated).any(|wire| wire == tool.name()) {
            actor.registry.register(Box::new(tool.clone()));
        }
    }
    if change.worth_a_note() {
        let _ = sequencer.publish(Candidate::new(
            EventKind::CommandResult,
            json!({
                "operation_id": Ulid::generate(),
                "name": "mcp_tools",
                "ok": true,
                "data": crate::bus::event::CommandResult::bound_data(note(added, removed, warnings)),
            }),
        ));
    }
}

fn note(added: &[String], removed: &[String], warnings: &[String]) -> serde_json::Value {
    let mut data = serde_json::Map::new();
    for (key, names) in [
        ("added", added),
        ("removed", removed),
        ("warnings", warnings),
    ] {
        if names.is_empty() {
            continue;
        }
        data.insert(key.into(), json!(&names[..names.len().min(NOTE_NAMES)]));
        if names.len() > NOTE_NAMES {
            data.insert(format!("{key}_more"), json!(names.len() - NOTE_NAMES));
        }
    }
    serde_json::Value::Object(data)
}
