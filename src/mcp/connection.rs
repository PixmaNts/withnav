//! Bounded discovery and supervision for one server: a stdio process group or an HTTP connection.
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
        assert!(
            std::process::Command::new(program)
                .args(["/d", "/c", "exit", "0"])
                .env_clear()
                .envs(env)
                .status()
                .unwrap()
                .success()
        );
    }
}

use super::{Ready, Server, tool::McpTool};
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
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

/// Hears `notifications/tools/list_changed`: the supervisor rereads the list on wake-up.
/// One stored wake-up is enough, so changes close together are read once.
struct ToolsChanged(Arc<tokio::sync::Notify>);

impl rmcp::ClientHandler for ToolsChanged {
    async fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        self.0.notify_one();
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
        server.notify();
        let result = run(&server, &cancel, permit).await;
        // Published again after `run` returned: the stderr tail is complete only now.
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
    {
        let mut status = server.status.lock().unwrap_or_else(|e| e.into_inner());
        status.state = "failed";
        status.tool_count = 0;
        status.error = Some(error);
    }
    server.notify();
}

async fn run(
    server: &Arc<Server>,
    cancel: &CancellationToken,
    permit: tokio::sync::SemaphorePermit<'_>,
) -> Result<(), String> {
    // The startup timeout counts from here, so launching the process (slow on a busy Windows
    // host, where it also joins a job object) is part of it: a server that hangs is marked
    // failed `startup_timeout_ms` after it started, which is what `nav doctor` waits for.
    let startup_deadline =
        tokio::time::Instant::now() + Duration::from_millis(server.config.startup_timeout_ms);
    if server.config.transport == "http" {
        return run_http(server, cancel, permit, startup_deadline).await;
    }
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
    // The tail is read by every status snapshot, but a new line does not announce one: a chatty
    // server would publish to every conversation per line. Clients show the tail of a failed
    // server, and the failure is announced once the tail is complete (after `run`).
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
    let tools_changed = Arc::new(tokio::sync::Notify::new());
    let initialize = {
        let tools_changed = tools_changed.clone();
        async move {
            ToolsChanged(tools_changed)
                .serve((stdout, stdin))
                .await
                .map_err(|_| {
                    "MCP initialization failed (invalid JSON-RPC or closed transport)".to_string()
                })
        }
    };
    let result = serve(
        server,
        cancel,
        permit,
        startup_deadline,
        initialize,
        tools_changed,
    )
    .await;
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
    if let Some(mut drain) = drain
        && tokio::time::timeout(Duration::from_secs(1), &mut drain)
            .await
            .is_err()
    {
        // A grandchild still holds stderr open: stop reading, so nothing changes the tail
        // after the failure below is published.
        drain.abort();
        let _ = drain.await;
    }
    result
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ToolsChanged>;

/// Initializes, discovers the tools within the startup deadline, publishes the server as
/// ready, then holds the connection until it drops or the manager shuts down. Shared by
/// every transport; publishes its own failure before the caller's cleanup.
async fn serve(
    server: &Arc<Server>,
    cancel: &CancellationToken,
    permit: tokio::sync::SemaphorePermit<'_>,
    startup_deadline: tokio::time::Instant,
    initialize: impl std::future::Future<Output = Result<Client, String>>,
    tools_changed: Arc<tokio::sync::Notify>,
) -> Result<(), String> {
    let result: Result<(), String> = async {
        let startup = async {
            let mut client = initialize.await?;
            let discovery = discover(server, client.peer(), startup_deadline).await;
            match discovery {
                Ok(inventory) => Ok((client, inventory)),
                Err(error) => {
                    let _ = client.close().await;
                    Err(error)
                }
            }
        };
        let (client, (tools, prompts)) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = tokio::time::timeout_at(startup_deadline, startup) => result.map_err(|_| "MCP startup timed out".to_string())??,
        };
        drop(permit);
        let disconnected = CancellationToken::new();
        *server.ready.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ready {
            peer: client.peer().clone(),
            disconnected: disconnected.clone(),
            tools,
            prompts,
        });
        {
            let mut status = server.status.lock().unwrap_or_else(|e| e.into_inner());
            status.state = "ready";
            status.error = None;
        }
        server.notify();
        // Keep the service handle until explicit closure; dropping it only requests cancellation.
        let close = client.cancellation_token();
        let peer = client.peer().clone();
        let mut waiting = Box::pin(client.waiting());
        let mut rereading = false;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => { close.cancel(); break; },
                _ = &mut waiting => { disconnected.cancel(); return Err("MCP transport disconnected".into()); }
                reread = tokio::time::timeout(
                    Duration::from_millis(server.config.startup_timeout_ms),
                    discover(
                        server,
                        &peer,
                        tokio::time::Instant::now()
                            + Duration::from_millis(server.config.startup_timeout_ms),
                    ),
                ), if rereading => {
                    rereading = false;
                    match reread {
                        Ok(Ok((tools, prompts))) => {
                            if let Some(ready) = server.ready.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                                ready.tools = tools;
                                ready.prompts = prompts;
                            }
                            server.notify();
                        }
                        // The tools held stay: a failed reread is not a failed server.
                        Ok(Err(error)) => tracing::warn!(server = %server.id, error = %server.redact(&error), "MCP tool list not refreshed"),
                        Err(_) => tracing::warn!(server = %server.id, "MCP tool list not refreshed: timed out"),
                    }
                }
                _ = tools_changed.notified(), if !rereading => { rereading = true; }
            }
        }
        disconnected.cancel();
        let grace = Duration::from_millis(crate::tools::bash_grace_ms());
        let _ = tokio::time::timeout(grace, &mut waiting).await;
        Ok(())
    }.await;
    if let Err(error) = &result {
        publish_failure(server, error.clone());
    }
    result
}

/// A Streamable HTTP server: no process to supervise, so the connection is the lifecycle.
/// The URL is never part of an error: reqwest's messages carry it, and it may hold a token.
async fn run_http(
    server: &Arc<Server>,
    cancel: &CancellationToken,
    permit: tokio::sync::SemaphorePermit<'_>,
    startup_deadline: tokio::time::Instant,
) -> Result<(), String> {
    use rmcp::transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    };
    let (headers, secrets) = super::config::http_headers(&server.config)?;
    *server.secrets.lock().unwrap_or_else(|e| e.into_inner()) = secrets;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(server.config.url.as_str())
            .custom_headers(headers),
    );
    let tools_changed = Arc::new(tokio::sync::Notify::new());
    let initialize = {
        let tools_changed = tools_changed.clone();
        async move {
            ToolsChanged(tools_changed)
                .serve(transport)
                .await
                .map_err(|e| http_failure(&e))
        }
    };
    serve(
        server,
        cancel,
        permit,
        startup_deadline,
        initialize,
        tools_changed,
    )
    .await
}

/// What `nav doctor` and clients show when an HTTP server cannot be initialized: whether it
/// was reached and what it answered, in fixed words that carry no URL, body or header.
fn http_failure(error: &rmcp::service::ClientInitializeError) -> String {
    use rmcp::transport::streamable_http_client::StreamableHttpError;
    let rmcp::service::ClientInitializeError::TransportError { error, .. } = error else {
        return "MCP initialization failed (invalid JSON-RPC response)".into();
    };
    match error
        .error
        .downcast_ref::<StreamableHttpError<reqwest::Error>>()
    {
        Some(StreamableHttpError::Client(e)) if e.is_connect() => {
            "MCP server unreachable (connection failed)".into()
        }
        Some(StreamableHttpError::Client(e)) if e.is_timeout() => {
            "MCP server unreachable (timed out)".into()
        }
        Some(StreamableHttpError::AuthRequired(_) | StreamableHttpError::InsufficientScope(_)) => {
            "MCP server requires authorization (HTTP 401/403); OAuth is not supported, set headers"
                .into()
        }
        Some(StreamableHttpError::HttpStatus(status)) => http_status(status.status),
        // A non-success answer without a challenge reads `HTTP <status>: <body>`; keep the
        // status, never the body.
        Some(StreamableHttpError::UnexpectedServerResponse(text)) => text
            .strip_prefix("HTTP ")
            .and_then(|rest| rest.get(..3))
            .and_then(|code| code.parse().ok())
            .map_or_else(
                || "MCP initialization failed over HTTP (unexpected response)".into(),
                http_status,
            ),
        _ => "MCP initialization failed over HTTP (unexpected response)".into(),
    }
}

fn http_status(status: u16) -> String {
    match status {
        401 | 403 => format!(
            "MCP server requires authorization (HTTP {status}); OAuth is not supported, set headers"
        ),
        _ => format!("MCP server answered HTTP {status}"),
    }
}

async fn discover(
    server: &Arc<Server>,
    peer: &rmcp::Peer<rmcp::RoleClient>,
    startup_deadline: tokio::time::Instant,
) -> Result<(Vec<McpTool>, Vec<super::prompt::Prompt>), String> {
    let capabilities = peer
        .peer_info()
        .map(|info| info.capabilities.clone())
        .unwrap_or_default();
    let offers_resources = capabilities.resources.is_some();
    let offers_prompts = capabilities.prompts.is_some();
    let mut definitions = vec![];
    // A server that offers resources or prompts and declares no tools has no tools/list to
    // answer. One that declares none is still asked, as before resources were supported:
    // some servers answer tools/list without declaring the capability.
    if capabilities.tools.is_some() || !(offers_resources || offers_prompts) {
        let mut cursor = None;
        let mut cursors = HashSet::new();
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
    }
    definitions.sort_by(|a, b| a.name.cmp(&b.name));
    let mut names = HashSet::new();
    let mut tools = vec![];
    let mut skipped = vec![];
    let denied = |name: &str| {
        server.config.tools.iter().any(|(pattern, allowed)| {
            !allowed
                && globset::Glob::new(pattern).is_ok_and(|g| g.compile_matcher().is_match(name))
        })
    };
    for definition in definitions {
        if denied(definition.name.as_ref()) {
            continue;
        }
        match McpTool::new(server.clone(), definition) {
            Ok(tool) if names.insert(tool.wire.clone()) => tools.push(tool),
            Ok(tool) => skipped.push(format!("{}: wire-name collision", tool.original)),
            Err(error) => skipped.push(error),
        }
    }
    // The resource tools come after the server's own: a server tool that already has the
    // same wire name keeps it. The server's deny globs apply to them by their local name.
    if offers_resources {
        for tool in super::resource::tools(server) {
            if denied(&tool.original) {
                continue;
            }
            if names.insert(tool.wire.clone()) {
                tools.push(tool);
            } else {
                skipped.push(format!("{}: name taken by a server tool", tool.original));
            }
        }
    }
    // Prompts are optional: their listing ends a little before the startup deadline, so a
    // server that never answers prompts/list still starts, with its tools and no prompts.
    let prompts = if offers_prompts {
        let deadline = startup_deadline
            .checked_sub(Duration::from_millis(100))
            .unwrap_or(startup_deadline);
        let (prompts, notes) = super::prompt::discover(server, peer, deadline).await;
        skipped.extend(notes);
        prompts
    } else {
        vec![]
    };
    let mut status = server.status.lock().unwrap_or_else(|e| e.into_inner());
    status.tool_count = tools.len();
    status.skipped_tools = skipped;
    for reason in &status.skipped_tools {
        tracing::warn!(server = %server.id, reason = %server.redact(reason), "MCP tool skipped");
    }
    Ok((tools, prompts))
}
