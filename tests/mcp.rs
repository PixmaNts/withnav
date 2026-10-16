use serde_json::json;
mod common;
#[cfg(windows)]
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use withnav::{
    mcp::McpManager,
    settings::Settings,
    tools::{Registry, ToolContext, ToolStatus},
};

fn settings(timeout: u64) -> Settings {
    let mut settings = Settings::default();
    settings.set_dotted("mcp", json!({"fixture": {"command": [common::python(), concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py")], "call_timeout_ms": timeout}}));
    settings
}

#[cfg(windows)]
#[tokio::test]
async fn extensionless_command_resolves_cmd_shim_from_child_path() {
    let directory = tempfile::tempdir().unwrap();
    let python = common::python();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_server.py");
    std::fs::write(
        directory.path().join("fake-mcp.cmd"),
        format!("@echo off\r\n\"{python}\" \"{}\" %*\r\n", fixture.display()),
    )
    .unwrap();
    let mut settings = Settings::default();
    settings.set_dotted(
        "mcp",
        json!({"fixture": {
            "command": ["fake-mcp"],
            "env": {"Path": directory.path(), "PATHEXT": ".EXE;.CMD;.BAT"}
        }}),
    );
    let manager = McpManager::start(&settings);
    ready(&manager).await;
    assert!(
        registry(&manager)
            .names()
            .contains(&"mcp__fixture__echo".into())
    );
    manager.shutdown().await;
}

async fn ready(manager: &McpManager) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.statuses().iter().any(|s| s.state == "ready") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture must initialize: {:?}", manager.statuses()));
}

fn registry(manager: &McpManager) -> Registry {
    // Permission-cutover integration tests can use this same fixture with server id `fake`:
    // register manager.snapshot(), then dispatch `mcp__fake__echo` through the ordinary gate.
    let mut registry = Registry::new();
    for tool in manager.snapshot() {
        registry.register(Box::new(tool));
    }
    registry
}

#[tokio::test]
async fn startup_cancel_and_timeout_allow_sigterm_cleanup() {
    for cancel in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cleanup");
        let mut settings = Settings::default();
        settings.set_dotted("mcp", json!({"hang":{"command":[common::python(),concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/mcp_server.py"),"hang"],"startup_timeout_ms":200,"env":{"CLEANUP_MARKER":marker.to_string_lossy()}}}));
        let manager = McpManager::start(&settings);
        tokio::time::sleep(Duration::from_millis(100)).await;
        if cancel {
            manager.shutdown().await;
        }
        // Unix SIGTERM handlers can flush cleanup markers before the group is killed.
        #[cfg(unix)]
        tokio::time::timeout(Duration::from_secs(3), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("hanging initialization must get graceful cleanup");
        #[cfg(windows)]
        if !cancel {
            tokio::time::timeout(Duration::from_secs(3), async {
                while !manager
                    .statuses()
                    .iter()
                    .any(|status| status.error.is_some())
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("hanging initialization must time out");
        }
        manager.shutdown().await;
    }
}

fn context() -> ToolContext {
    ToolContext {
        binding: None,
        workspace: std::env::temp_dir(),
        writable_roots: vec![],
        readable_roots: vec![],
    }
}

#[tokio::test]
async fn registry_call_pagination_conversion_cancel_crash_and_restart() {
    let settings = settings(2000);
    let manager = McpManager::start(&settings);
    ready(&manager).await;
    let registry = registry(&manager);
    let cx = context();
    assert_eq!(registry.names().len(), 8);
    assert_eq!(manager.statuses()[0].skipped_tools.len(), 1);
    let call = |name: &'static str| {
        registry.dispatch(name, json!({"text":"hello"}), CancellationToken::new(), &cx)
    };
    let result = call("mcp__fixture__echo").await.unwrap();
    assert_eq!(result.output, "{\"text\": \"hello\"}");
    assert_eq!(result.metadata["mcp_tool"], "echo");
    let result = call("mcp__fixture__big").await.unwrap();
    assert_eq!(result.truncated.unwrap().original_bytes, 100000);
    assert!(result.output.len() <= 65536);
    let result = call("mcp__fixture__fail").await.unwrap();
    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(result.error.unwrap().plugin_id.as_deref(), Some("fixture"));
    assert_eq!(
        registry
            .schemas()
            .iter()
            .find(|t| t.function.name == "mcp__fixture__reference")
            .unwrap()
            .function
            .parameters["properties"]["text"]["type"],
        "string"
    );
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        token.cancel();
    });
    let result = registry
        .dispatch("mcp__fixture__slow", json!({}), cancel, &cx)
        .await
        .unwrap();
    cancel_task.await.unwrap();
    assert_eq!(result.status, ToolStatus::Interrupted);
    let result = call("mcp__fixture__cancellations").await.unwrap();
    assert_eq!(result.output, "1");
    let result = call("mcp__fixture__crash").await.unwrap();
    assert_eq!(result.error.unwrap().code, "mcp_disconnected");
    tokio::time::sleep(Duration::from_millis(100)).await;
    ready(&manager).await;
    assert_eq!(
        call("mcp__fixture__echo").await.unwrap().status,
        ToolStatus::Ok
    );
    manager.shutdown().await;
    assert!(manager.snapshot().is_empty());
    assert_eq!(
        call("mcp__fixture__echo")
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        "mcp_disconnected"
    );
}

#[tokio::test]
async fn timeout_sends_cancellation_and_keeps_connection() {
    let settings = settings(80);
    let manager = McpManager::start(&settings);
    ready(&manager).await;
    let registry = registry(&manager);
    let cx = context();
    let result = registry
        .dispatch(
            "mcp__fixture__slow",
            json!({}),
            CancellationToken::new(),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(result.error.unwrap().code, "mcp_timeout");
    let result = registry
        .dispatch(
            "mcp__fixture__cancellations",
            json!({}),
            CancellationToken::new(),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(result.output, "1");
    manager.shutdown().await;
}

#[tokio::test]
async fn slow_start_is_nonblocking_and_shutdown_kills_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let pidfile = directory.path().join("pid");
    let mut settings = Settings::default();
    let script = format!(
        "import subprocess,time,sys; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); open({:?},'w').write(str(p.pid)); time.sleep(60)",
        pidfile.to_string_lossy()
    );
    settings.set_dotted(
        "mcp",
        json!({"hang": {"command":[common::python(), "-c", script], "startup_timeout_ms":10000}}),
    );
    let start = std::time::Instant::now();
    let manager = McpManager::start(&settings);
    assert!(start.elapsed() < Duration::from_millis(100));
    assert!(manager.snapshot().is_empty());
    // The PID file may exist before its contents have been written.
    let pid: i32 = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    #[cfg(windows)]
    let process = unsafe {
        windows_sys::Win32::System::Threading::OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE,
            0,
            pid as u32,
        )
    };
    #[cfg(windows)]
    assert!(
        !process.is_null(),
        "descendant process must exist before shutdown"
    );
    manager.shutdown().await;
    #[cfg(unix)]
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if status.is_err() || status.unwrap().split_whitespace().nth(2) == Some("Z") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant must not survive shutdown");
    #[cfg(windows)]
    {
        let waited =
            unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(process, 3000) };
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(process);
        }
        assert_eq!(waited, 0, "descendant must not survive shutdown");
    }
}

#[tokio::test]
async fn service_session_creation_does_not_wait_for_initialization() {
    let (tmp, workspace) = common::server::temp_workspace();
    let config = common::server::TestServerConfig::unreachable(tmp.path()).config();
    let mut settings = withnav::settings::load(&config.config_dir).unwrap();
    settings.set_dotted("mcp", json!({"hang":{"command":[common::python(),concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py"),"hang"],"startup_timeout_ms":10000}}));
    withnav::settings::save(&config.config_dir, &settings).unwrap();
    let state = withnav::server::AppState::new(config).unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        state.service().create_session(&workspace, None, false),
    )
    .await
    .unwrap()
    .unwrap();
    state.service().close_all().await;
}

#[tokio::test]
async fn filters_disabled_servers_and_stderr_redaction() {
    let mut settings = settings(2000);
    settings.set_dotted("mcp.fixture.tools", json!({"s*":false}));
    settings.set_dotted(
        "mcp.disabled",
        json!({"command":["no-such-command"],"enabled":false}),
    );
    settings.set_dotted("mcp.bad.id", json!({"command":["no-such-command"]}));
    settings.set_dotted("mcp.noisy", json!({"command":[common::python(),"-c","import os,sys; sys.stderr.write(os.environ['SECRET']+' '+('x'*100000)); sys.stderr.flush()"],"env":{"SECRET":"mcp-secret-sentinel"},"startup_timeout_ms":1000}));
    let manager = McpManager::start(&settings);
    ready(&manager).await;
    assert!(
        !registry(&manager)
            .names()
            .contains(&"mcp__fixture__slow".into())
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager
            .statuses()
            .iter()
            .find(|s| s.server_id == "noisy")
            .unwrap()
            .stderr_tail
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let statuses = manager.statuses();
    assert_eq!(
        statuses
            .iter()
            .find(|s| s.server_id == "disabled")
            .unwrap()
            .state,
        "disabled"
    );
    let noisy = statuses.iter().find(|s| s.server_id == "noisy").unwrap();
    assert!(noisy.stderr_tail.len() <= 65536);
    assert!(!noisy.stderr_tail.contains("mcp-secret-sentinel"));
    manager.shutdown().await;
}

#[tokio::test]
// Unix SIGTERM handlers provide descendant cleanup before forced termination.
#[cfg(unix)]
async fn launcher_exit_does_not_shortcut_descendant_sigterm_grace() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("descendant-cleanup");
    let started = dir.path().join("descendant-ready");
    let launcher_pid = dir.path().join("launcher-pid");
    let descendant = format!(
        "import signal,time,sys;\ndef cleanup(*_):\n time.sleep(0.15)\n open({:?},'w').write('cleanup')\n sys.exit(0)\nsignal.signal(signal.SIGTERM,cleanup)\nopen({:?},'w').write('ready')\ntime.sleep(60)",
        marker.to_string_lossy(),
        started.to_string_lossy()
    );
    // The launcher exits after its child installs the handler, leaving the group alive.
    let launcher = format!(
        "import subprocess,time,os,sys; open({:?},'w').write(str(os.getpid())); subprocess.Popen([sys.executable,'-c',{:?}]);\nwhile not os.path.exists({:?}): time.sleep(0.01)",
        launcher_pid.to_string_lossy(),
        descendant,
        started.to_string_lossy()
    );
    let mut settings = Settings::default();
    settings.set_dotted(
        "mcp",
        json!({"launcher":{"command":[common::python(),"-c",launcher],"startup_timeout_ms":300}}),
    );
    let manager = McpManager::start(&settings);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(launcher_pid).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if stat.as_ref().is_err()
                || stat
                    .as_ref()
                    .is_ok_and(|s| s.split_whitespace().nth(2) == Some("Z"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("launcher must exit before shutdown");
    manager.shutdown().await;
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "cleanup");
}
