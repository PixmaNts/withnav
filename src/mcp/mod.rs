//! Shared stdio and Streamable HTTP MCP connections, snapshotted into each session's tool registry.
pub mod config;
mod connection;
mod prompt;
mod redaction;
mod resource;
mod tool;
mod ui_error;
pub use prompt::{
    BLOCK_CLOSE as PROMPT_BLOCK_CLOSE, BLOCK_OPEN as PROMPT_BLOCK_OPEN, Fetched, PromptCommand,
    PromptRun, captain_prose, carries_prompt,
};
pub use tool::McpTool;
pub use ui_error::{McpUiError, UI_ERROR_DETAIL_CHARS};

use crate::tools::Tool;
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

/// Bounds of one server's diagnostics on the wire: clients show them, `nav doctor` keeps the
/// full text.
const WIRE_STDERR_BYTES: usize = 4 * 1024;
const WIRE_STDERR_LINES: usize = 40;
const WIRE_ERROR_CHARS: usize = 500;
const WIRE_SKIPPED: usize = 20;

/// The one MIME type of an MCP App interface resource: a single HTML document.
const MCP_APP_HTML_MIME: &str = "text/html;profile=mcp-app";
/// Ceiling on the raw resource text, counted in characters, before redaction. Text over it
/// is refused (not truncated): redaction matches whole secrets, and the scan it runs is
/// bounded to this many characters rather than to the whole response.
const MAX_UI_RESOURCE_TEXT_INPUT_CHARS: usize = 4 * 1024 * 1024;
/// Delivered text, counted in characters (the read path's existing unit): 512 Ki characters.
/// This is the post-redaction bound, unchanged from before.
const MAX_UI_RESOURCE_TEXT_CHARS: usize = 512 * 1024;
/// Ceiling on an encoded blob, in base64 characters, checked before decoding so a server
/// cannot make the agent allocate an unbounded base64 buffer.
const MAX_UI_RESOURCE_BLOB_BASE64_CHARS: usize = 4 * 1024 * 1024;
/// Decoded blob, in bytes. Past this the blob is refused, not truncated.
const MAX_UI_RESOURCE_BLOB_BYTES: usize = 512 * 1024;

/// One server as clients see it: the redacted, bounded form of a [`ServerStatus`].
pub use crate::bus::event::McpServerView as ServerView;
/// Every server of the service at one moment, as clients see it.
pub use crate::bus::event::McpStatus as McpSnapshot;

fn server_view(status: &ServerStatus, transport: &str) -> ServerView {
    let (stderr_tail, stderr_truncated) = bounded_tail(&status.stderr_tail);
    let ready = status.state == "ready";
    ServerView {
        server_id: status.server_id.clone(),
        transport: transport.to_owned(),
        state: status.state.to_owned(),
        tool_count: if ready { status.tool_count } else { 0 },
        skipped_tools: status
            .skipped_tools
            .iter()
            .take(WIRE_SKIPPED)
            .map(|s| bounded_chars(s, WIRE_ERROR_CHARS))
            .collect(),
        skipped_more: status.skipped_tools.len().saturating_sub(WIRE_SKIPPED),
        error: status
            .error
            .as_deref()
            .filter(|e| !e.is_empty())
            .map(|e| bounded_chars(e, WIRE_ERROR_CHARS)),
        stderr_tail,
        stderr_truncated,
    }
}

/// The largest app document the server will serve, in bytes.
pub const MAX_APP_HTML_BYTES: usize = 512 * 1024;

fn bounded_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The last lines of `tail` that fit in the wire bounds, cut on a character boundary.
fn bounded_tail(tail: &str) -> (String, bool) {
    let lines: Vec<&str> = tail.lines().collect();
    let mut kept = lines[lines.len().saturating_sub(WIRE_STDERR_LINES)..].join("\n");
    let mut truncated = kept.len() < tail.trim_end_matches('\n').len();
    if kept.len() > WIRE_STDERR_BYTES {
        let mut start = kept.len() - WIRE_STDERR_BYTES;
        while !kept.is_char_boundary(start) {
            start += 1;
        }
        kept = kept[start..].to_owned();
        truncated = true;
    }
    (kept, truncated)
}

/// Decodes standard base64, padded or not: an MCP server may omit padding. The input is
/// validated exactly as it arrives and returned unchanged, so a blob a browser accepts is
/// also accepted here, and one it rejects (leading or trailing whitespace) is rejected too.
fn decode_base64(data: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data))
        .map_err(|_| ())
}

/// One entry of an MCP App interface resource, validated and bounded.
///
/// An MCP App's interface resource is a single HTML document, so exactly one
/// `text/html;profile=mcp-app` item is expected: none, several, or one carrying neither
/// `text` nor `blob` is an error naming the server and the URI, never a silent success.
/// Errors are returned rather than papering over a server that answered wrongly.
fn mcp_app_resource(
    contents: &[serde_json::Value],
    server_id: &str,
    uri: &str,
    secrets: &[String],
) -> Result<serde_json::Value, String> {
    let named: Vec<&serde_json::Value> = contents
        .iter()
        .filter(|item| item["mimeType"].as_str() == Some(MCP_APP_HTML_MIME))
        .collect();
    let item = match named.as_slice() {
        [one] => *one,
        [] => {
            return Err(format!(
                "MCP App resource {uri} on server {server_id} has no {MCP_APP_HTML_MIME} item"
            ));
        }
        several => {
            return Err(format!(
                "MCP App resource {uri} on server {server_id} has {} {MCP_APP_HTML_MIME} items, expected one",
                several.len()
            ));
        }
    };
    // The raw text is refused over the input ceiling, never truncated: redaction matches
    // whole secret strings, so cutting text could leave a secret's prefix unredacted and
    // silently served. Under the ceiling it is redacted, then bounded for delivery.
    let text = match item["text"].as_str() {
        Some(t) if t.chars().count() > MAX_UI_RESOURCE_TEXT_INPUT_CHARS => {
            return Err(format!(
                "MCP App resource {uri} on server {server_id} has text of {} characters, over the {MAX_UI_RESOURCE_TEXT_INPUT_CHARS}-character ceiling",
                t.chars().count()
            ));
        }
        Some(t) => {
            let redacted = redaction::redact(t, secrets);
            Some(bounded_chars(&redacted, MAX_UI_RESOURCE_TEXT_CHARS))
        }
        None => None,
    };
    let blob = match item["blob"].as_str() {
        Some(b) => {
            let validated = validated_blob(b, server_id, uri)?;
            // A blob is not text that can be partly redacted: one holding a configured
            // secret (a header token is often base64-shaped) is withheld whole.
            if redaction::redact(&validated, secrets) == validated {
                Some(validated)
            } else {
                None
            }
        }
        None => None,
    };
    if text.is_none() && blob.is_none() {
        return Err(format!(
            "MCP App resource {uri} on server {server_id} carries neither text nor blob"
        ));
    }
    Ok(serde_json::json!({
        "uri": uri,
        "mime_type": MCP_APP_HTML_MIME,
        "text": text,
        "blob": blob,
        "csp": item["_meta"]["ui"]["csp"],
    }))
}

/// Validates one blob as base64 and bounds its decoded size. The encoded string is
/// checked before decoding; the decoded bytes are checked after.
fn validated_blob(blob: &str, server_id: &str, uri: &str) -> Result<String, String> {
    if blob.len() > MAX_UI_RESOURCE_BLOB_BASE64_CHARS {
        return Err(format!(
            "MCP App resource {uri} on server {server_id} has a blob of {} base64 characters, too large",
            blob.len()
        ));
    }
    let decoded = decode_base64(blob).map_err(|_| {
        format!("MCP App resource {uri} on server {server_id} has a blob that is not valid base64")
    })?;
    if decoded.len() > MAX_UI_RESOURCE_BLOB_BYTES {
        return Err(format!(
            "MCP App resource {uri} on server {server_id} has a blob that decodes to {} bytes, too large",
            decoded.len()
        ));
    }
    Ok(blob.to_owned())
}

pub(crate) struct Ready {
    peer: rmcp::Peer<rmcp::RoleClient>,
    disconnected: CancellationToken,
    tools: Vec<McpTool>,
    prompts: Vec<prompt::Prompt>,
}

pub(crate) struct Server {
    id: String,
    config: config::ServerConfig,
    ready: Mutex<Option<Ready>>,
    status: Mutex<ServerStatus>,
    stderr: Mutex<redaction::StderrTail>,
    secrets: Mutex<Vec<String>>,
    /// Shared by every server of a manager: bumped after each change a client would see.
    changed: tokio::sync::watch::Sender<u64>,
}

impl Server {
    /// Tells the manager's watchers that this server's status changed.
    pub(crate) fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
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
    incarnation: ulid::Ulid,
    changed: tokio::sync::watch::Sender<u64>,
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
        let (changed, _) = tokio::sync::watch::channel(0u64);
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
                changed: changed.clone(),
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
            incarnation: ulid::Ulid::generate(),
            changed,
        })
    }

    /// Wakes when any server's status changes. Changes close together are seen once:
    /// read [`McpManager::status_snapshot`] after each wake-up, never count them.
    pub fn watch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// What clients see of every server now, redacted and bounded, with the revision a
    /// client compares to keep the newest. Built from one read of each server's status and
    /// ready inventory, so a failed server never shows the tools it had.
    pub fn status_snapshot(&self) -> McpSnapshot {
        // Read the revision first: a change made while building is then announced again,
        // and the next snapshot (with a higher revision) carries it.
        let revision = *self.changed.borrow();
        let transports = |id: &str| {
            self.servers
                .iter()
                .find(|s| s.id == id)
                .map_or("unknown", |s| s.config.transport.as_str())
                .to_owned()
        };
        let servers = self
            .statuses()
            .iter()
            .map(|status| server_view(status, &transports(&status.server_id)))
            .collect();
        McpSnapshot {
            incarnation: self.incarnation,
            revision,
            servers,
        }
    }

    /// Every tool of every ready server, before cross-server collision resolution: what a
    /// session reconciles its own registry against between turns.
    pub fn inventory(&self) -> Vec<McpTool> {
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
        // Collisions are logged by `snapshot`, once per session registry: a status read is
        // repeated by every client and must not repeat the warning.
        let (tools, collisions) = resolve_collisions(inventory);
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
            status.error = status.error.as_deref().map(|text| s.redact(text));
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

    /// The prompts of every ready server, as `<server>:<prompt>` commands.
    pub fn prompt_commands(&self) -> Vec<PromptCommand> {
        self.servers
            .iter()
            .flat_map(|s| {
                s.ready
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|r| r.prompts.iter().map(|p| p.command(s)).collect())
                    .unwrap_or_else(Vec::new)
            })
            .collect()
    }

    /// The prompt a `<server>:<prompt>` command names, when that server is ready and offers it.
    pub fn prompt(&self, command: &str) -> Option<PromptRun> {
        let (server_id, name) = command.split_once(':')?;
        let server = self.servers.iter().find(|s| s.id == server_id)?;
        let prompt = server
            .ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .prompts
            .iter()
            .find(|p| p.name() == name)?
            .clone();
        Some(PromptRun::new(server.clone(), prompt))
    }

    /// The server an app request names: unknown ids are 404-class, configured servers whose
    /// configuration did not parse are unavailable rather than unknown.
    fn ui_server(&self, server_id: &str) -> Result<&Arc<Server>, McpUiError> {
        if let Some(server) = self.servers.iter().find(|s| s.id == server_id) {
            return Ok(server);
        }
        if self.invalid.iter().any(|s| s.server_id == server_id) {
            return Err(McpUiError::Unavailable);
        }
        Err(McpUiError::UnknownServer)
    }

    pub async fn read_ui_resource(
        &self,
        server_id: &str,
        uri: &str,
    ) -> Result<serde_json::Value, McpUiError> {
        const METHOD: &str = "resources/read";
        if !uri.starts_with("ui://") || uri.len() > 512 {
            return Err(McpUiError::InvalidRequest(
                "only ui:// resources are permitted",
            ));
        }
        let server = self.ui_server(server_id)?;
        let ready = server
            .ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| (r.peer.clone(), r.disconnected.clone()));
        let Some((peer, disconnected)) = ready else {
            return Err(McpUiError::Unavailable);
        };
        let params = rmcp::model::ReadResourceRequestParams::new(uri.to_string());
        let request = rmcp::model::ClientRequest::ReadResourceRequest(
            rmcp::model::ReadResourceRequest::new(params),
        );
        let timeout = std::time::Duration::from_millis(server.config.call_timeout_ms);
        let secrets = server.secrets();
        let value = tokio::select! {
            biased;
            _ = disconnected.cancelled() => return Err(McpUiError::Unavailable),
            res = tokio::time::timeout(timeout, peer.send_request(request)) => match res {
                Ok(Ok(response)) => serde_json::to_value(response)
                    .map_err(|_| McpUiError::InvalidResponse { method: METHOD })?,
                Ok(Err(error)) => return Err(ui_error::classify(&server.id, METHOD, error, &secrets)),
                Err(_) => return Err(ui_error::timed_out(&server.id, METHOD, timeout)),
            }
        };
        let contents = value["contents"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match mcp_app_resource(contents, server_id, uri, &secrets) {
            Ok(resource) => Ok(resource),
            Err(err) => {
                let diagnostic = bounded_chars(&redaction::redact(&err, &secrets), 1024);
                tracing::warn!(server = server_id, error = %diagnostic, "invalid MCP App resource");
                Err(McpUiError::InvalidResponse { method: METHOD })
            }
        }
    }

    /// The HTML of a `ui://` app resource, ready to be served as a document: text as the
    /// server sent it, or a blob decoded to UTF-8, both redacted and bounded to
    /// [`MAX_APP_HTML_BYTES`]; plus the CSP the resource declares in `_meta.ui.csp` (`null`
    /// when it declares none). A blob that is not valid base64 or UTF-8, or is too large, is
    /// refused rather than served.
    pub async fn read_app_resource(
        &self,
        server_id: &str,
        uri: &str,
    ) -> Result<(String, serde_json::Value), String> {
        use base64::Engine as _;
        let body = self
            .read_ui_resource(server_id, uri)
            .await
            .map_err(|e| e.to_string())?;
        let html = if let Some(text) = body["text"].as_str() {
            text.to_owned()
        } else if let Some(blob) = body["blob"].as_str() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(blob)
                .map_err(|_| "MCP app resource is not valid base64".to_string())?;
            let text = String::from_utf8(bytes)
                .map_err(|_| "MCP app resource is not valid UTF-8".to_string())?;
            let secrets = self
                .servers
                .iter()
                .find(|s| s.id == server_id)
                .map(|s| s.secrets())
                .unwrap_or_default();
            redaction::redact(&text, &secrets)
        } else {
            return Err("MCP resource returned no HTML content".into());
        };
        if html.len() > MAX_APP_HTML_BYTES {
            return Err("MCP app resource is too large".into());
        }
        Ok((html, body["csp"].clone()))
    }

    /// Calls a tool for an MCP App. Arguments come from app code in a client, so they are
    /// bounded before anything is sent; the response is bounded before it is copied into a JSON
    /// value and again after redaction, which can lengthen it.
    pub async fn call_ui_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, McpUiError> {
        const METHOD: &str = "tools/call";
        check_ui_arguments(&arguments)?;
        let server = self.ui_server(server_id)?;
        let ready = server
            .ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| {
                // Only the server's own tools: the resource tools WithNav adds are not
                // `tools/call` names on the server.
                let tool = r
                    .tools
                    .iter()
                    .find(|t| {
                        t.kind == tool::Kind::Call
                            && (t.original == tool_name || t.wire == tool_name)
                    })
                    .map(|t| t.original.clone());
                (r.peer.clone(), r.disconnected.clone(), tool)
            });
        let Some((peer, disconnected, tool)) = ready else {
            return Err(McpUiError::Unavailable);
        };
        let Some(original) = tool else {
            return Err(McpUiError::UnknownTool);
        };
        let params = rmcp::model::CallToolRequestParams::new(original).with_arguments(arguments);
        let request =
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(params));
        let timeout = std::time::Duration::from_millis(server.config.call_timeout_ms);
        let secrets = server.secrets();
        let response = tokio::select! {
            biased;
            _ = disconnected.cancelled() => return Err(McpUiError::Unavailable),
            res = tokio::time::timeout(timeout, peer.send_request(request)) => match res {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(ui_error::classify(&server.id, METHOD, error, &secrets)),
                Err(_) => return Err(ui_error::timed_out(&server.id, METHOD, timeout)),
            }
        };
        ui_tool_response(METHOD, &response, &secrets)
    }

    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for task in tasks {
            let _ = task.await;
        }
    }
}

/// Largest serialised arguments an MCP App may send to one tool call. Below the HTTP body limit
/// of the route (2 MiB), so this check, not the transport, is what refuses an oversized call.
const MAX_UI_ARGUMENT_BYTES: usize = 1024 * 1024;

/// Deepest nesting of objects and arrays in MCP App tool arguments, the arguments object
/// included.
const MAX_UI_ARGUMENT_DEPTH: usize = 64;

/// Largest MCP App tool response returned to a client, serialised, before and after redaction.
const MAX_UI_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

fn check_ui_arguments(
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), McpUiError> {
    fn depth_within(value: &serde_json::Value, levels: usize) -> bool {
        match value {
            serde_json::Value::Array(items) => {
                levels > 0 && items.iter().all(|v| depth_within(v, levels - 1))
            }
            serde_json::Value::Object(entries) => {
                levels > 0 && entries.values().all(|v| depth_within(v, levels - 1))
            }
            _ => true,
        }
    }
    // Depth first: it bounds the recursion of the size check that follows. The messages name
    // the limits literally (a static message); the unit test pins them to the constants.
    if !arguments
        .values()
        .all(|v| depth_within(v, MAX_UI_ARGUMENT_DEPTH - 1))
    {
        return Err(McpUiError::InvalidRequest(
            "MCP tool arguments are nested deeper than 64 levels",
        ));
    }
    match tool::json_within(arguments, MAX_UI_ARGUMENT_BYTES, false) {
        Ok(_) => Ok(()),
        Err(tool::JsonLimit::TooLarge) => Err(McpUiError::InvalidRequest(
            "MCP tool arguments are larger than 1024 KiB",
        )),
        Err(tool::JsonLimit::Invalid) => {
            Err(McpUiError::InvalidRequest("invalid MCP tool arguments"))
        }
    }
}

/// The redacted JSON of an MCP App tool response, refused when it is larger than
/// [`MAX_UI_RESPONSE_BYTES`].
fn ui_tool_response(
    method: &'static str,
    response: &impl serde::Serialize,
    secrets: &[String],
) -> Result<serde_json::Value, McpUiError> {
    let refused = |error| match error {
        tool::JsonLimit::TooLarge => McpUiError::ResponseTooLarge {
            limit_mib: MAX_UI_RESPONSE_BYTES / (1024 * 1024),
        },
        tool::JsonLimit::Invalid => McpUiError::InvalidResponse { method },
    };
    let (_, bytes) = tool::json_within(response, MAX_UI_RESPONSE_BYTES, true).map_err(refused)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| McpUiError::InvalidResponse { method })?;
    drop(bytes);
    let redacted = redaction::redact_value(&value, secrets);
    tool::json_within(&redacted, MAX_UI_RESPONSE_BYTES, false).map_err(refused)?;
    Ok(redacted)
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

/// The MCP tools one session's registry holds now. The session handle keeps it (a child
/// is created from what its parent holds at that moment) and the runtime refreshes it
/// between two turns.
#[derive(Clone, Default)]
pub struct InstalledTools(Arc<Mutex<Installed>>);

#[derive(Default)]
struct Installed {
    tools: Arc<Vec<McpTool>>,
    /// The clashes already reported to this session, so one that persists is announced
    /// once, not at every status change.
    reported: std::collections::HashSet<(String, String)>,
}

/// What a refresh changed in one session's MCP tools, by wire name.
#[derive(Default)]
pub struct ToolChange {
    /// What the registry holds now.
    pub tools: Arc<Vec<McpTool>>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Same name, other description, schema or app metadata.
    pub updated: Vec<String>,
    /// New clashes: a tool whose name another server already holds is not installed.
    pub warnings: Vec<String>,
}

impl ToolChange {
    /// Whether the Captain is told: a changed description alone is installed silently.
    pub fn worth_a_note(&self) -> bool {
        !(self.added.is_empty() && self.removed.is_empty() && self.warnings.is_empty())
    }
}

type Fingerprint = (serde_json::Value, Option<String>, Option<String>);

fn fingerprint(tool: &McpTool) -> Fingerprint {
    (
        tool.schema(),
        tool.ui_resource_uri.clone(),
        tool.ui_url.clone(),
    )
}

impl std::fmt::Debug for InstalledTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstalledTools")
    }
}

impl InstalledTools {
    /// The tools a new root session starts with: what is ready now.
    pub fn from_inventory(inventory: Vec<McpTool>) -> Self {
        let installed = Self::default();
        installed.refresh(inventory);
        installed
    }

    /// A copy frozen at this moment and never refreshed: what a child keeps.
    pub fn frozen(tools: Arc<Vec<McpTool>>) -> Self {
        Self(Arc::new(Mutex::new(Installed {
            tools,
            reported: Default::default(),
        })))
    }

    pub fn get(&self) -> Arc<Vec<McpTool>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tools
            .clone()
    }

    /// Reconciles the held tools with `inventory`. A tool already held keeps its wire name
    /// against another server's tool that claims it later; among the others the server id
    /// and tool name decide, as for a new session. `None` when nothing changed.
    pub fn refresh(&self, mut inventory: Vec<McpTool>) -> Option<ToolChange> {
        let mut held = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let key = |t: &McpTool| (t.server_id().to_owned(), t.original.clone());
        let before: std::collections::HashMap<_, _> = held
            .tools
            .iter()
            .map(|t| (key(t), fingerprint(t)))
            .collect();
        inventory.sort_by(|a, b| {
            (!before.contains_key(&key(a)), a.server_id(), &a.original).cmp(&(
                !before.contains_key(&key(b)),
                b.server_id(),
                &b.original,
            ))
        });
        let mut owners = std::collections::HashMap::<String, String>::new();
        let mut tools = vec![];
        let mut clashes = std::collections::HashMap::new();
        for tool in inventory {
            match owners.get(&tool.wire) {
                None => {
                    owners.insert(tool.wire.clone(), tool.server_id().to_owned());
                    tools.push(tool);
                }
                Some(owner) => {
                    clashes.insert(
                        key(&tool),
                        format!(
                            "{} from {} is not installed: {owner} already holds the name",
                            tool.wire,
                            tool.server_id()
                        ),
                    );
                }
            }
        }
        tools.sort_by(|a, b| (a.server_id(), &a.original).cmp(&(b.server_id(), &b.original)));
        let after: std::collections::HashMap<_, _> = tools
            .iter()
            .map(|t| (key(t), (t.wire.clone(), fingerprint(t))))
            .collect();
        let wire_before: std::collections::HashMap<_, _> = held
            .tools
            .iter()
            .map(|t| (key(t), t.wire.clone()))
            .collect();
        let mut added: Vec<String> = after
            .iter()
            .filter(|(k, _)| !before.contains_key(*k))
            .map(|(_, (wire, _))| wire.clone())
            .collect();
        let mut removed: Vec<String> = wire_before
            .iter()
            .filter(|(k, _)| !after.contains_key(*k))
            .map(|(_, wire)| wire.clone())
            .collect();
        let mut updated: Vec<String> = after
            .iter()
            .filter(|(k, (_, print))| before.get(*k).is_some_and(|old| old != print))
            .map(|(_, (wire, _))| wire.clone())
            .collect();
        let mut warnings: Vec<String> = clashes
            .iter()
            .filter(|(k, _)| !held.reported.contains(*k))
            .map(|(_, text)| text.clone())
            .collect();
        for list in [&mut added, &mut removed, &mut updated, &mut warnings] {
            list.sort();
        }
        held.reported = clashes.into_keys().collect();
        if added.is_empty() && removed.is_empty() && updated.is_empty() && warnings.is_empty() {
            return None;
        }
        for warning in &warnings {
            tracing::warn!(%warning, "MCP tool skipped");
        }
        held.tools = Arc::new(tools);
        Some(ToolChange {
            tools: held.tools.clone(),
            added,
            removed,
            updated,
            warnings,
        })
    }
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

    #[test]
    fn the_wire_view_is_bounded_on_character_boundaries_and_counts_tools_only_when_ready() {
        // 60 lines of 3-byte characters: kept to the last 40 lines and 4 KB, cut between characters.
        let tail: String = (0..60)
            .map(|n| format!("{n:02} {}\n", "é".repeat(80)))
            .collect();
        let (kept, truncated) = bounded_tail(&tail);
        assert!(truncated);
        assert!(kept.len() <= WIRE_STDERR_BYTES);
        assert!(kept.ends_with(&"é".repeat(80)));
        assert!(!kept.contains("19 "), "older lines are dropped");
        assert_eq!(bounded_tail("short\n"), ("short".to_owned(), false));

        let error = "x".repeat(WIRE_ERROR_CHARS + 10);
        let status = ServerStatus {
            server_id: "s".into(),
            state: "failed",
            tool_count: 4,
            skipped_tools: (0..WIRE_SKIPPED + 3).map(|n| format!("t{n}")).collect(),
            error: Some(error),
            stderr_tail: String::new(),
        };
        let view = server_view(&status, "stdio");
        assert_eq!(view.tool_count, 0, "a failed server offers no tool");
        assert_eq!(view.error.unwrap().chars().count(), WIRE_ERROR_CHARS);
        assert_eq!(view.skipped_tools.len(), WIRE_SKIPPED);
        assert_eq!(view.skipped_more, 3);
        let ready = ServerStatus {
            state: "ready",
            ..status
        };
        assert_eq!(server_view(&ready, "stdio").tool_count, 4);
    }

    #[test]
    fn mcp_app_tool_arguments_are_bounded_in_depth_and_size() {
        let nested = |levels: usize, array: bool| {
            // `levels` containers in all, the arguments object included.
            let mut value = json!(1);
            for _ in 1..levels {
                value = if array {
                    json!([value])
                } else {
                    json!({ "k": value })
                };
            }
            serde_json::from_value::<serde_json::Map<_, _>>(json!({ "a": value })).unwrap()
        };
        for array in [true, false] {
            assert!(check_ui_arguments(&nested(MAX_UI_ARGUMENT_DEPTH, array)).is_ok());
            assert!(
                check_ui_arguments(&nested(MAX_UI_ARGUMENT_DEPTH + 1, array))
                    .unwrap_err()
                    .to_string()
                    .contains(&format!(
                        "nested deeper than {MAX_UI_ARGUMENT_DEPTH} levels"
                    ))
            );
        }
        // `{"a":"…"}` is 8 bytes around the string.
        let sized = |len: usize| {
            serde_json::from_value::<serde_json::Map<_, _>>(json!({ "a": "x".repeat(len) }))
                .unwrap()
        };
        assert!(check_ui_arguments(&sized(MAX_UI_ARGUMENT_BYTES - 8)).is_ok());
        assert_eq!(
            check_ui_arguments(&sized(MAX_UI_ARGUMENT_BYTES - 7))
                .unwrap_err()
                .to_string(),
            format!(
                "MCP tool arguments are larger than {} KiB",
                MAX_UI_ARGUMENT_BYTES / 1024
            )
        );
    }

    #[test]
    fn an_mcp_app_tool_response_is_bounded_before_and_after_redaction() {
        // `"…"` is 2 bytes around the string.
        let at_limit = json!("x".repeat(MAX_UI_RESPONSE_BYTES - 2));
        assert_eq!(
            ui_tool_response("tools/call", &at_limit, &[]).unwrap(),
            at_limit
        );
        let over = json!("x".repeat(MAX_UI_RESPONSE_BYTES - 1));
        assert_eq!(
            ui_tool_response("tools/call", &over, &[])
                .unwrap_err()
                .to_string(),
            "MCP tool response is larger than 8 MiB"
        );
        // Within the limit as received, past it once each one-character secret is replaced.
        let grows =
            json!({"content": [{"type": "text", "text": "s".repeat(MAX_UI_RESPONSE_BYTES / 2)}]});
        assert_eq!(
            ui_tool_response("tools/call", &grows, &["s".to_string()])
                .unwrap_err()
                .to_string(),
            "MCP tool response is larger than 8 MiB"
        );
        let redacted =
            ui_tool_response("tools/call", &json!({"token": "s"}), &["s".to_string()]).unwrap();
        assert_eq!(redacted, json!({"token": "[redacted]"}));
    }

    fn bare_server(id: &str) -> Arc<Server> {
        let config: config::ServerConfig =
            serde_json::from_value(json!({"command": ["true"], "enabled": false})).unwrap();
        let (changed, _) = tokio::sync::watch::channel(0u64);
        Arc::new(Server {
            id: id.into(),
            config,
            ready: Mutex::new(None),
            status: Mutex::new(ServerStatus {
                server_id: id.into(),
                state: "ready",
                tool_count: 0,
                skipped_tools: vec![],
                error: None,
                stderr_tail: String::new(),
            }),
            stderr: Mutex::new(Default::default()),
            secrets: Mutex::new(vec![]),
            changed,
        })
    }

    fn tool(server: &Arc<Server>, name: &str, description: &str) -> McpTool {
        let schema = json!({"type": "object"}).as_object().unwrap().clone();
        McpTool::new(
            server.clone(),
            rmcp::model::Tool::new(name.to_owned(), description.to_owned(), schema),
        )
        .unwrap()
    }

    #[test]
    fn a_refresh_reports_added_removed_and_updated_tools() {
        let a = bare_server("a");
        let installed =
            InstalledTools::from_inventory(vec![tool(&a, "one", "first"), tool(&a, "two", "x")]);
        assert_eq!(installed.get().len(), 2);
        assert!(
            installed
                .refresh(vec![tool(&a, "one", "first"), tool(&a, "two", "x")])
                .is_none(),
            "an unchanged inventory changes nothing"
        );
        let change = installed
            .refresh(vec![tool(&a, "one", "reworded"), tool(&a, "three", "y")])
            .unwrap();
        assert_eq!(change.added, ["mcp__a__three"]);
        assert_eq!(change.removed, ["mcp__a__two"]);
        assert_eq!(change.updated, ["mcp__a__one"]);
        assert!(change.worth_a_note());
        assert_eq!(installed.get().len(), 2);
        // A reworded description alone is installed without a note.
        let change = installed
            .refresh(vec![tool(&a, "one", "again"), tool(&a, "three", "y")])
            .unwrap();
        assert!(!change.worth_a_note());
    }

    #[test]
    fn a_tool_that_claims_a_held_name_later_is_not_installed_and_is_warned_once() {
        // Both names reduce to `mcp__a__b__c`; only ids written before ids rejected `__`
        // can produce it.
        let (first, second) = (bare_server("a"), bare_server("a__b"));
        let held = tool(&first, "b__c", "held");
        let rival = tool(&second, "c", "rival");
        assert_eq!(held.wire, rival.wire);
        // The rival sorts before the held tool, yet the tool already held keeps the name.
        let installed = InstalledTools::from_inventory(vec![held.clone()]);
        let change = installed
            .refresh(vec![rival.clone(), held.clone()])
            .unwrap();
        assert!(change.added.is_empty() && change.removed.is_empty());
        assert_eq!(change.warnings.len(), 1, "{:?}", change.warnings);
        assert!(change.warnings[0].contains("already holds the name"));
        assert!(change.worth_a_note());
        assert_eq!(installed.get()[0].server_id(), "a");
        // The same clash is not announced again.
        assert!(installed.refresh(vec![rival.clone(), held]).is_none());
        // Once the holder goes, the rival takes the name and the Captain hears of it.
        let change = installed.refresh(vec![rival]).unwrap();
        assert_eq!(change.added, ["mcp__a__b__c"]);
        assert_eq!(change.removed, ["mcp__a__b__c"]);
    }

    #[test]
    fn a_frozen_copy_keeps_the_tools_it_was_made_with() {
        let a = bare_server("a");
        let parent = InstalledTools::from_inventory(vec![tool(&a, "one", "x")]);
        let child = InstalledTools::frozen(parent.get());
        parent.refresh(vec![tool(&a, "two", "y")]).unwrap();
        assert_eq!(child.get()[0].wire, "mcp__a__one");
        assert_eq!(parent.get()[0].wire, "mcp__a__two");
    }

    #[tokio::test]
    async fn every_state_change_wakes_the_watchers_and_raises_the_revision() {
        let mut settings = crate::settings::Settings::default();
        settings.set_dotted(
            "mcp",
            json!({"off": {"command": ["true"], "enabled": false}}),
        );
        let manager = McpManager::start(&settings);
        let mut watch = manager.watch();
        let first = manager.status_snapshot();
        assert_eq!(first.servers[0].state, "disabled");
        manager.servers[0].notify();
        watch.changed().await.unwrap();
        let second = manager.status_snapshot();
        assert_eq!(second.incarnation, first.incarnation);
        assert!(second.revision > first.revision);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn legacy_cross_server_collision_is_sorted_reported_and_dispatches_first() {
        // Bypass id parsing to cover inventories created by older configurations; current
        // declarations reject __ in ids, but the manager still owns collision resolution.
        let cancel = CancellationToken::new();
        let (changed, _) = tokio::sync::watch::channel(0u64);
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
                changed: changed.clone(),
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
            incarnation: ulid::Ulid::generate(),
            changed,
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
            secrets: None,
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
        assert!(
            config::parse(Some(&json!({"a__b":{"command":["x"]}})))[0]
                .1
                .is_err()
        );
    }

    fn html_item(text: &str) -> serde_json::Value {
        json!({"uri": "ui://s/app.html", "mimeType": MCP_APP_HTML_MIME, "text": text})
    }

    #[test]
    fn mcp_app_resource_takes_the_one_html_item_and_redacts_and_bounds_text() {
        let value = mcp_app_resource(
            &[html_item("<title>App</title> SUPERSECRETVALUE")],
            "s",
            "ui://s/app.html",
            &["SUPERSECRETVALUE".to_owned()],
        )
        .unwrap();
        assert_eq!(value["mime_type"], MCP_APP_HTML_MIME);
        assert_eq!(value["text"], "<title>App</title> [redacted]");
        assert!(value["blob"].is_null());
        // Delivered text is bounded even when the raw item is far larger.
        let long = mcp_app_resource(
            &[html_item(&"x".repeat(MAX_UI_RESOURCE_TEXT_CHARS + 10))],
            "s",
            "ui://s/app.html",
            &[],
        )
        .unwrap();
        assert_eq!(
            long["text"].as_str().unwrap().chars().count(),
            MAX_UI_RESOURCE_TEXT_CHARS
        );
    }

    #[test]
    fn mcp_app_resource_selects_the_html_item_from_among_others() {
        // The item is chosen by MIME type, not by position: the HTML document is neither
        // first nor last, so a `contents.first()` implementation would take the wrong one.
        let value = mcp_app_resource(
            &[
                json!({"mimeType": "text/plain", "text": "not it"}),
                html_item("<title>App</title>"),
                json!({"mimeType": "application/json", "text": "{}"}),
            ],
            "s",
            "ui://s/app.html",
            &[],
        )
        .unwrap();
        assert_eq!(value["text"], "<title>App</title>");
    }

    #[test]
    fn mcp_app_resource_refuses_zero_several_or_empty_items() {
        let err = |contents: Vec<serde_json::Value>| {
            mcp_app_resource(&contents, "srv", "ui://srv/a.html", &[]).unwrap_err()
        };
        let named = |err: &str| err.contains("srv") && err.contains("ui://srv/a.html");
        let none = err(vec![]);
        assert!(named(&none) && none.contains("no text/html"));
        // A different MIME is not the app item.
        assert!(err(vec![json!({"mimeType":"text/plain","text":"x"})]).contains("no text/html"));
        // A missing MIME no longer defaults to the app type.
        assert!(err(vec![json!({"text":"x"})]).contains("no text/html"));
        // Several app items are ambiguous.
        let two = err(vec![html_item("a"), html_item("b")]);
        assert!(named(&two) && two.contains('2') && two.contains("expected one"));
        // The one app item must carry text or blob.
        let empty = err(vec![json!({"mimeType": MCP_APP_HTML_MIME})]);
        assert!(named(&empty) && empty.contains("neither text nor blob"));
    }

    #[test]
    fn mcp_app_resource_validates_blobs_as_base64_and_bounds_decoded_size() {
        use base64::Engine as _;
        let ok = |blob: &str| {
            mcp_app_resource(
                &[json!({"mimeType": MCP_APP_HTML_MIME, "blob": blob})],
                "s",
                "ui://s/a.html",
                &[],
            )
            .unwrap()["blob"]
                .as_str()
                .map(ToOwned::to_owned)
        };
        // Padded and unpadded both decode.
        assert_eq!(ok("aGVsbG8="), Some("aGVsbG8=".to_owned()));
        assert_eq!(ok("aGVsbG8"), Some("aGVsbG8".to_owned()));
        // Exactly the decoded byte bound is kept; one byte more is refused.
        let at_limit =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_UI_RESOURCE_BLOB_BYTES]);
        assert_eq!(ok(&at_limit).as_deref(), Some(at_limit.as_str()));
        let over_limit =
            base64::engine::general_purpose::STANDARD
                .encode(vec![0u8; MAX_UI_RESOURCE_BLOB_BYTES + 1]);
        let err = |blob: String| {
            mcp_app_resource(
                &[json!({"mimeType": MCP_APP_HTML_MIME, "blob": blob})],
                "srv",
                "ui://srv/a.html",
                &[],
            )
            .unwrap_err()
        };
        let bad = err("not base64!!".to_owned());
        assert!(
            bad.contains("not valid base64")
                && bad.contains("srv")
                && bad.contains("ui://srv/a.html")
        );
        // Whitespace a lenient decoder would forgive but a browser's atob rejects: refused.
        assert!(err("\u{a0}aGVsbG8=\u{a0}".to_owned()).contains("not valid base64"));
        // Over the encoded ceiling: never decoded.
        assert!(err("A".repeat(MAX_UI_RESOURCE_BLOB_BASE64_CHARS + 4)).contains("too large"));
        // Valid base64 whose decoded size is over the byte bound.
        assert!(err(over_limit).contains("decodes to"));
    }

    #[test]
    fn mcp_app_resource_refuses_raw_text_over_the_input_ceiling() {
        // Over the ceiling the text is refused, not truncated: truncation could cut a secret
        // and leave its prefix unredacted in what the client receives.
        let raw = "x".repeat(MAX_UI_RESOURCE_TEXT_INPUT_CHARS + 1);
        let err = mcp_app_resource(
            &[html_item(&raw)],
            "srv",
            "ui://srv/a.html",
            &["SUPERSECRETVALUE".to_owned()],
        )
        .unwrap_err();
        assert!(err.contains("ceiling") && err.contains("srv") && err.contains("ui://srv/a.html"));
    }
}
