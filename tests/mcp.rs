use serde_json::json;
mod common;
use std::{path::Path, time::Duration};
use tokio_util::sync::CancellationToken;
use withnav::{
    mcp::McpManager,
    settings::Settings,
    tools::{Registry, ToolContext, ToolStatus},
};

fn settings(timeout: u64) -> Settings {
    let mut settings = Settings::default();
    settings.set_dotted("mcp", json!({"fixture": {"command": ["python3", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py")], "call_timeout_ms": timeout}}));
    settings
}

async fn ready(manager: &McpManager) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.statuses().iter().any(|s| s.state == "ready") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture must initialize");
}

fn registry(manager: &McpManager) -> Registry {
    let mut registry = Registry::new();
    for tool in manager.snapshot() {
        registry.register(Box::new(tool));
    }
    registry
}

fn context() -> ToolContext {
    ToolContext {
        binding: None,
        workspace: Path::new("/tmp/opencode").into(),
        writable_roots: vec![],
        readable_roots: vec![],
    }
}

#[tokio::test]
async fn registry_call_pagination_conversion_cancel_crash_and_restart() {
    let manager = McpManager::start(&settings(2000));
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
    let manager = McpManager::start(&settings(80));
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
    let script = format!("import subprocess,time; p=subprocess.Popen(['sleep','60']); open({:?},'w').write(str(p.pid)); time.sleep(60)", pidfile.to_string_lossy());
    settings.set_dotted(
        "mcp",
        json!({"hang": {"command":["python3", "-c", script], "startup_timeout_ms":10000}}),
    );
    let start = std::time::Instant::now();
    let manager = McpManager::start(&settings);
    assert!(start.elapsed() < Duration::from_millis(100));
    assert!(manager.snapshot().is_empty());
    tokio::time::timeout(Duration::from_secs(3), async {
        while !pidfile.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid: i32 = std::fs::read_to_string(pidfile).unwrap().parse().unwrap();
    manager.shutdown().await;
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
}

#[tokio::test]
async fn service_session_creation_does_not_wait_for_initialization() {
    let (tmp, workspace) = common::server::temp_workspace();
    let config = common::server::TestServerConfig::unreachable(tmp.path()).config();
    let mut settings = withnav::settings::load(&config.config_dir).unwrap();
    settings.set_dotted("mcp", json!({"hang":{"command":["python3",concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py"),"hang"],"startup_timeout_ms":10000}}));
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
    settings.set_dotted("mcp.noisy", json!({"command":["python3","-c","import os,sys; sys.stderr.write(os.environ['SECRET']+' '+('x'*100000)); sys.stderr.flush()"],"env":{"SECRET":"mcp-secret-sentinel"},"startup_timeout_ms":1000}));
    let manager = McpManager::start(&settings);
    ready(&manager).await;
    assert!(!registry(&manager)
        .names()
        .contains(&"mcp__fixture__slow".into()));
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
