//! Shared stdio MCP connections, snapshotted into each session's tool registry.
pub mod config;
mod connection;
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
    stderr: Mutex<std::collections::VecDeque<u8>>,
    secrets: Mutex<Vec<String>>,
}

impl Server {
    pub(crate) fn redact(&self, text: &str) -> String {
        let mut text = text.to_string();
        for secret in self
            .secrets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|s| !s.is_empty())
        {
            text = text.replace(secret, "[redacted]");
        }
        text
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
        self.servers
            .iter()
            .flat_map(|s| {
                s.ready
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|r| r.tools.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        let mut statuses = self.invalid.clone();
        statuses.extend(self.servers.iter().map(|s| {
            let mut status = s.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let bytes: Vec<_> = s
                .stderr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .copied()
                .collect();
            status.stderr_tail = s.redact(&String::from_utf8_lossy(&bytes));
            status.skipped_tools = status
                .skipped_tools
                .iter()
                .map(|text| s.redact(text))
                .collect();
            status
        }));
        statuses
    }

    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for task in tasks {
            let _ = task.await;
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
