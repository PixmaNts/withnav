//! Bounded discovery and process-group supervision for one stdio server.
use super::{tool::McpTool, Ready, Server};
use nix::{
    sys::signal::{killpg, Signal},
    unistd::Pid,
};
use rmcp::{transport::TokioChildProcess, ServiceExt};
use std::{collections::HashSet, process::Stdio, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

struct Group(Pid);
impl Drop for Group {
    fn drop(&mut self) {
        let _ = killpg(self.0, Signal::SIGKILL);
    }
}

pub(super) async fn supervise(
    server: Arc<Server>,
    cancel: CancellationToken,
    limit: Arc<tokio::sync::Semaphore>,
) {
    for attempt in 0..=5 {
        if attempt > 0 {
            tokio::select! { _ = cancel.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs((1u64 << (attempt - 1)).min(60))) => {} }
        }
        let permit = tokio::select! { _ = cancel.cancelled() => break, permit = limit.acquire() => match permit { Ok(p) => p, Err(_) => break } };
        server
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state = "starting";
        let result = run(&server, &cancel, permit).await;
        if let Some(ready) = server
            .ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            ready.disconnected.cancel();
        }
        let mut status = server.status.lock().unwrap_or_else(|e| e.into_inner());
        status.state = "failed";
        status.tool_count = 0;
        status.error = Some(
            result
                .err()
                .unwrap_or_else(|| "MCP transport disconnected".into()),
        );
        if cancel.is_cancelled() {
            break;
        }
    }
}

async fn run(
    server: &Arc<Server>,
    cancel: &CancellationToken,
    permit: tokio::sync::SemaphorePermit<'_>,
) -> Result<(), String> {
    let env = super::config::child_env(&server.config)?;
    *server.secrets.lock().unwrap_or_else(|e| e.into_inner()) = server
        .config
        .env
        .keys()
        .filter_map(|key| env.get(key).cloned())
        .collect();
    let mut command = tokio::process::Command::new(&server.config.command[0]);
    command
        .args(&server.config.command[1..])
        .env_clear()
        .envs(env)
        .process_group(0);
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "cannot spawn MCP command (details omitted)".to_string())?;
    let pid = transport
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .filter(|id| *id > 0)
        .ok_or("MCP child has no process id")?;
    let group = Group(Pid::from_raw(pid));
    let drain = stderr.map(|mut stderr| {
        let server = server.clone();
        tokio::spawn(async move {
            let mut bytes = [0; 4096];
            while let Ok(n) = stderr.read(&mut bytes).await {
                if n == 0 {
                    break;
                }
                let mut ring = server.stderr.lock().unwrap_or_else(|e| e.into_inner());
                ring.extend(&bytes[..n]);
                while ring.len() > 64 * 1024 {
                    ring.pop_front();
                }
                let mut lines = ring.iter().filter(|b| **b == b'\n').count();
                while lines > 200 {
                    if ring.pop_front() == Some(b'\n') {
                        lines -= 1;
                    }
                }
            }
        })
    });
    let result = async {
        let startup = async {
            let mut client = ().serve(transport).await.map_err(|_| "MCP initialization failed (invalid JSON-RPC or closed transport)".to_string())?;
            let discovery = discover(server, client.peer()).await;
            match discovery {
                Ok(tools) => Ok((client, tools)),
                Err(error) => { let _ = client.close().await; Err(error) }
            }
        };
        let (client, tools) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = tokio::time::timeout(Duration::from_millis(server.config.startup_timeout_ms), startup) => result.map_err(|_| "MCP startup timed out".to_string())??,
        };
        drop(permit);
        let disconnected = CancellationToken::new();
        *server.ready.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ready { peer: client.peer().clone(), disconnected: disconnected.clone(), tools });
        {
            let mut status = server.status.lock().unwrap_or_else(|e| e.into_inner());
            status.state = "ready";
            status.error = None;
        }
        // Keep the service handle until explicit closure; dropping it only requests cancellation.
        let close = client.cancellation_token();
        let mut waiting = Box::pin(client.waiting());
        tokio::select! {
            _ = cancel.cancelled() => { close.cancel(); },
            _ = &mut waiting => { disconnected.cancel(); return Err("MCP transport disconnected".into()); }
        }
        disconnected.cancel();
        let grace = Duration::from_millis(crate::tools::bash_grace_ms());
        let _ = tokio::time::timeout(grace, &mut waiting).await;
        let _ = killpg(group.0, Signal::SIGTERM);
        Ok(())
    }.await;
    drop(group);
    if let Some(drain) = drain {
        let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
    }
    result
}

async fn discover(
    server: &Arc<Server>,
    peer: &rmcp::Peer<rmcp::RoleClient>,
) -> Result<Vec<McpTool>, String> {
    let mut cursor = None;
    let mut cursors = HashSet::new();
    let mut definitions = vec![];
    for _ in 0..100 {
        let page = peer
            .list_tools(Some(
                rmcp::model::PaginatedRequestParams::default().with_cursor(cursor),
            ))
            .await
            .map_err(|_| "MCP tools/list failed".to_string())?;
        definitions.extend(page.tools);
        if definitions.len() > 10_000 {
            return Err("MCP tool inventory exceeds 10000 tools".into());
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
        if !cursors.insert(cursor.clone()) {
            return Err("MCP tools/list repeated a cursor".into());
        }
        if cursors.len() == 100 {
            return Err("MCP tools/list exceeds 100 pages".into());
        }
    }
    definitions.sort_by(|a, b| a.name.cmp(&b.name));
    let mut names = HashSet::new();
    let mut tools = vec![];
    let mut skipped = vec![];
    for definition in definitions {
        if server.config.tools.iter().any(|(pattern, allowed)| {
            !allowed
                && globset::Glob::new(pattern)
                    .is_ok_and(|g| g.compile_matcher().is_match(definition.name.as_ref()))
        }) {
            continue;
        }
        match McpTool::new(server.clone(), definition) {
            Ok(tool) if names.insert(tool.wire.clone()) => tools.push(tool),
            Ok(tool) => skipped.push(format!("{}: wire-name collision", tool.original)),
            Err(error) => skipped.push(error),
        }
    }
    let mut status = server.status.lock().unwrap_or_else(|e| e.into_inner());
    status.tool_count = tools.len();
    status.skipped_tools = skipped;
    Ok(tools)
}
