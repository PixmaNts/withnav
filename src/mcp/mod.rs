//! Shared stdio MCP connections, snapshotted into each session's tool registry.
pub mod config;
mod connection;
mod redaction;
mod tool;
pub use tool::McpTool;

use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ServerStatus {
    pub server_id: String,
    pub state: &'static str,
    pub tool_count: usize,
    pub skipped_tools: Vec<String>,
    pub error: Option<String>,
    pub stderr_tail: String,
}

pub(crate) struct Ready {
    peer: rmcp::Peer<rmcp::RoleClient>,
    disconnected: CancellationToken,
    tools: Vec<McpTool>,
}

pub(crate) struct Server {
    id: String,
    config: config::ServerConfig,
    ready: Mutex<Option<Ready>>,
    status: Mutex<ServerStatus>,
    stderr: Mutex<redaction::StderrTail>,
    secrets: Mutex<Vec<String>>,
}

impl Server {
    pub(crate) fn secrets(&self) -> Vec<String> {
        self.secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub(crate) fn redact(&self, text: &str) -> String {
        redaction::redact(
            text,
            &self.secrets.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }
}

/// One manager per service, never awaited while constructing a session.
#[derive(Debug)]
pub struct McpManager {
    servers: Vec<Arc<Server>>,
    invalid: Vec<ServerStatus>,
    cancel: CancellationToken,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl McpManager {
    pub fn start(settings: &crate::settings::Settings) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
        let mut servers = vec![];
        let mut invalid = vec![];
        let mut tasks = vec![];
        for (id, config) in settings.mcp() {
            let mut status = ServerStatus {
                server_id: id.clone(),
                state: "starting",
                tool_count: 0,
                skipped_tools: vec![],
                error: None,
                stderr_tail: String::new(),
            };
            let config = match config {
                Ok(config) => config,
                Err(error) => {
                    status.state = "failed";
                    status.error = Some(error);
                    invalid.push(status);
                    continue;
                }
            };
            if !config.enabled {
                status.state = "disabled";
            }
            let server = Arc::new(Server {
                id,
                config,
                ready: Mutex::new(None),
                status: Mutex::new(status),
                stderr: Mutex::new(Default::default()),
                secrets: Mutex::new(vec![]),
            });
            if server.config.enabled {
                let task_server = server.clone();
                let token = cancel.clone();
                let limit = semaphore.clone();
                tasks.push(tokio::spawn(async move {
                    connection::supervise(task_server, token, limit).await;
                }));
            }
            servers.push(server);
        }
        Arc::new(Self {
            servers,
            invalid,
            cancel,
            tasks: Mutex::new(tasks),
        })
    }

    pub fn snapshot(&self) -> Vec<McpTool> {
        let inventory: Vec<_> = self
            .servers
            .iter()
            .flat_map(|s| {
                s.ready
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|r| r.tools.clone())
                    .unwrap_or_default()
            })
            .collect();
        let (tools, skipped) = resolve_collisions(inventory);
        for (id, reason) in skipped {
            if let Some(server) = self.servers.iter().find(|s| s.id == id) {
                tracing::warn!(server = %id, reason = %server.redact(&reason), "MCP tool skipped");
            }
        }
        tools
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        let inventory = self
            .servers
            .iter()
            .flat_map(|s| {
                s.ready
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|r| r.tools.clone())
                    .unwrap_or_default()
            })
            .collect();
        let (tools, collisions) = resolve_collisions(inventory);
        for (id, reason) in &collisions {
            if let Some(server) = self.servers.iter().find(|s| &s.id == id) {
                tracing::warn!(server = %id, reason = %server.redact(reason), "MCP tool skipped");
            }
        }
        let mut statuses = self.invalid.clone();
        statuses.extend(self.servers.iter().map(|s| {
            let mut status = s.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
            status.stderr_tail = s.stderr.lock().unwrap_or_else(|e| e.into_inner()).text();
            status.tool_count = tools.iter().filter(|t| t.server_id() == s.id).count();
            status.skipped_tools.extend(
                collisions
                    .iter()
                    .filter(|(id, _)| id == &s.id)
                    .map(|(_, reason)| reason.clone()),
            );
            status.skipped_tools = status
                .skipped_tools
                .iter()
                .map(|text| s.redact(text))
                .collect();
            status
        }));
        statuses
    }

    /// Waits until every enabled server is ready or failed, at most the largest configured
    /// startup timeout (capped at 30 s). For one-shot callers whose single session would
    /// otherwise freeze an inventory missing a server that is about to be ready.
    pub async fn wait_until_settled(&self) {
        let budget = self
            .servers
            .iter()
            .filter(|s| s.config.enabled)
            .map(|s| s.config.startup_timeout_ms)
            .max()
            .unwrap_or(0)
            .min(30_000);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(budget);
        while self
            .servers
            .iter()
            .any(|s| s.status.lock().unwrap_or_else(|e| e.into_inner()).state == "starting")
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for task in tasks {
            let _ = task.await;
        }
    }
}

fn resolve_collisions(mut tools: Vec<McpTool>) -> (Vec<McpTool>, Vec<(String, String)>) {
    tools.sort_by(|a, b| (a.server_id(), &a.original).cmp(&(b.server_id(), &b.original)));
    let mut names = std::collections::HashSet::new();
    let mut skipped = vec![];
    tools.retain(|tool| {
        if names.insert(tool.wire.clone()) {
            true
        } else {
            skipped.push((
                tool.server_id().to_owned(),
                format!(
                    "{}: cross-server wire-name collision ({})",
                    tool.original, tool.wire
                ),
            ));
            false
        }
    });
    (tools, skipped)
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
#[path = "../../tests/common/python.rs"]
mod test_python;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn legacy_cross_server_collision_is_sorted_reported_and_dispatches_first() {
        // Bypass id parsing to cover inventories created by older configurations; current
        // declarations reject __ in ids, but the manager still owns collision resolution.
        let cancel = CancellationToken::new();
        let mut servers = vec![];
        let mut tasks = vec![];
        for (id, name, retained) in [("a__b", "c", "wrong"), ("a", "b__c", "retained")] {
            let config: config::ServerConfig = serde_json::from_value(json!({"command":[super::test_python::python(),concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/mcp_server.py")],"env":{"TOOL_NAMES":json!([name]).to_string(),"RETAINED":retained}})).unwrap();
            let server = Arc::new(Server {
                id: id.into(),
                config,
                ready: Mutex::new(None),
                status: Mutex::new(ServerStatus {
                    server_id: id.into(),
                    state: "starting",
                    tool_count: 0,
                    skipped_tools: vec![],
                    error: None,
                    stderr_tail: String::new(),
                }),
                stderr: Mutex::new(Default::default()),
                secrets: Mutex::new(vec![]),
            });
            let s = server.clone();
            let c = cancel.clone();
            tasks.push(tokio::spawn(async move {
                connection::supervise(s, c, Arc::new(tokio::sync::Semaphore::new(4))).await;
            }));
            servers.push(server);
        }
        let manager = McpManager {
            servers,
            invalid: vec![],
            cancel,
            tasks: Mutex::new(tasks),
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while manager.statuses().iter().any(|s| s.state != "ready") {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let tools = manager.snapshot();
        assert_eq!(tools.len(), 1);
        let skipped = manager
            .statuses()
            .into_iter()
            .find(|s| s.server_id == "a__b")
            .unwrap();
        assert_eq!(skipped.skipped_tools.len(), 1);
        assert_eq!(skipped.tool_count, 0);
        let mut registry = crate::tools::Registry::new();
        for tool in tools {
            registry.register(Box::new(tool));
        }
        let cx = crate::tools::ToolContext {
            binding: None,
            workspace: std::env::temp_dir(),
            writable_roots: vec![],
            readable_roots: vec![],
        };
        assert_eq!(
            registry
                .dispatch("mcp__a__b__c", json!({}), Default::default(), &cx)
                .await
                .unwrap()
                .metadata["mcp_server"],
            "a"
        );
        manager.shutdown().await;
        assert!(config::parse(Some(&json!({"a__b":{"command":["x"]}})))[0]
            .1
            .is_err());
    }
}
