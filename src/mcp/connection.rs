//! Bounded discovery and process-group supervision for one stdio server.
#[cfg(windows)]
fn resolve_windows_command(
    command: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> std::io::Result<std::path::PathBuf> {
    use std::path::Path;
    let path = Path::new(command);
    if path.extension().is_some() || command.contains(['/', '\\', ':']) {
        return Ok(path.to_path_buf());
    }
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let extensions = lookup("PATHEXT").unwrap_or(".COM;.EXE;.BAT;.CMD");
    use windows_sys::Win32::System::SystemInformation::{
        GetSystemDirectoryW, GetWindowsDirectoryW,
    };
    let mut directories: Vec<_> = std::env::split_paths(lookup("PATH").unwrap_or(""))
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect();
    if let Some(app_dir) = std::env::current_exe()?.parent() {
        directories.push(app_dir.to_path_buf());
    }
    directories.push(windows_directory(GetSystemDirectoryW)?);
    directories.push(windows_directory(GetWindowsDirectoryW)?);
    for dir in directories {
        for extension in extensions
            .split(';')
            .filter(|ext| ext.starts_with('.') && !ext.contains(['/', '\\', ':']))
        {
            let candidate = dir.join(format!("{command}{extension}"));
            if candidate.is_file() {
                // Absolute targets keep std's .cmd/.bat escaping and avoid a second PATH lookup.
                return std::path::absolute(candidate);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("MCP command {command:?} not found in child Path/PATHEXT"),
    ))
}

#[cfg(windows)]
fn windows_directory(
    get_directory: unsafe extern "system" fn(*mut u16, u32) -> u32,
) -> std::io::Result<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let mut buffer = vec![0; 260];
    loop {
        let length = unsafe { get_directory(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if length == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if length < buffer.len() {
            return Ok(std::ffi::OsString::from_wide(&buffer[..length]).into());
        }
        buffer.resize(length + 1, 0);
    }
}

#[cfg(all(test, windows))]
mod windows_command_tests {
    use super::*;

    #[test]
    fn system_command_starts_with_isolated_child_path() {
        let directory = tempfile::tempdir().unwrap();
        let env = std::collections::BTreeMap::from([
            ("Path".into(), format!(";{};", directory.path().display())),
            ("PATHEXT".into(), ".EXE;.CMD".into()),
        ]);
        let program = resolve_windows_command("cmd", &env).unwrap();
        let system_dir =
            windows_directory(windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW)
                .unwrap();
        assert_eq!(program, system_dir.join("cmd.EXE"));
        assert!(std::process::Command::new(program)
            .args(["/d", "/c", "exit", "0"])
            .env_clear()
            .envs(env)
            .status()
            .unwrap()
            .success());
    }
}

use super::{tool::McpTool, Ready, Server};
#[cfg(unix)]
use nix::{
    sys::signal::{killpg, Signal},
    unistd::Pid,
};
use rmcp::ServiceExt;
use std::{collections::HashSet, process::Stdio, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
struct Group(Pid);
#[cfg(unix)]
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
        publish_failure(
            &server,
            result
                .err()
                .unwrap_or_else(|| "MCP transport disconnected".into()),
        );
        if cancel.is_cancelled() {
            break;
        }
    }
}

/// Withdraws the server's tools and records why. Called as soon as a failure is known,
/// before the process cleanup that may take a grace period, so a reader never sees a
/// failed server as still starting.
fn publish_failure(server: &Server, error: String) {
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
    status.error = Some(error);
}

async fn run(
    server: &Arc<Server>,
    cancel: &CancellationToken,
    permit: tokio::sync::SemaphorePermit<'_>,
) -> Result<(), String> {
    let (env, secrets) = super::config::child_env(&server.config)?;
    *server.secrets.lock().unwrap_or_else(|e| e.into_inner()) = secrets.clone();
    #[cfg(unix)]
    let program = std::path::PathBuf::from(&server.config.command[0]);
    #[cfg(windows)]
    let program = resolve_windows_command(&server.config.command[0], &env)
        .map_err(|error| server.redact(&error.to_string()))?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(&server.config.command[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(unix)]
    let mut child = command
        .spawn()
        .map_err(|_| "cannot spawn MCP command (details omitted)".to_string())?;
    #[cfg(windows)]
    let (mut child, group) =
        crate::tools::process_job::Job::spawn(&mut command, None).map_err(|error| {
            server.redact(&format!("cannot spawn MCP command in process job: {error}"))
        })?;
    #[cfg(unix)]
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .filter(|id| *id > 0)
        .ok_or("MCP child has no process id")?;
    #[cfg(unix)]
    let group = Group(Pid::from_raw(pid));
    let stdin = child.stdin.take().ok_or("MCP stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("MCP stdout unavailable")?;
    let stderr = child.stderr.take();
    let drain = stderr.map(|mut stderr| {
        let server = server.clone();
        tokio::spawn(async move {
            let mut bytes = [0; 4096];
            while let Ok(n) = stderr.read(&mut bytes).await {
                if n == 0 {
                    break;
                }
                let mut ring = server.stderr.lock().unwrap_or_else(|e| e.into_inner());
                ring.push(&bytes[..n], &secrets, false);
            }
            server
                .stderr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&[], &secrets, true);
        })
    });
    let result: Result<(), String> = async {
        let startup = async {
            let mut client = ().serve((stdout, stdin)).await.map_err(|_| "MCP initialization failed (invalid JSON-RPC or closed transport)".to_string())?;
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
        Ok(())
    }.await;
    if let Err(error) = &result {
        publish_failure(server, error.clone());
    }
    // Initialization futures own only stdio, not the child. Dropping them closes stdin;
    // give both cooperative EOF and SIGTERM handlers time before the group kill fallback.
    let _ = tokio::time::timeout(Duration::from_millis(100), child.wait()).await;
    #[cfg(unix)]
    {
        let _ = killpg(group.0, Signal::SIGTERM);
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(crate::tools::bash_grace_ms());
        // The launcher may already be gone while descendants still run SIGTERM cleanup.
        // Reap the launcher but use group existence, not its exit, to end the grace period.
        while !matches!(killpg(group.0, None), Err(nix::errno::Errno::ESRCH))
            && tokio::time::Instant::now() < deadline
        {
            let _ = child.try_wait();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    drop(group);
    let _ = child.wait().await;
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
    for reason in &status.skipped_tools {
        tracing::warn!(server = %server.id, reason = %server.redact(reason), "MCP tool skipped");
    }
    Ok(tools)
}
