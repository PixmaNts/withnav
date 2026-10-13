//! Separate process permits a real global subscriber, including rmcp's spawned tasks.
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing_subscriber::prelude::*;
use withnav::{
    mcp::McpManager,
    settings::Settings,
    tools::{Registry, ToolContext},
};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn verbose_logging_cannot_expose_init_notification_or_response_secrets() {
    let logs = Captured::default();
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            "trace,rmcp::service=trace",
        ))
        .with(withnav::app::logging::mcp_safe_filter())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(logs.clone()),
        )
        .init();
    let sentinel = "mcp-env-token-log-sentinel";
    std::env::set_var("WITHNAV_MCP_TEST_TOKEN", sentinel);
    let mut settings = Settings::default();
    settings.set_dotted("mcp", json!({"fake":{"command":["python3",concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/mcp_server.py")],"env":{"COMPOSITE":"Bearer {env:WITHNAV_MCP_TEST_TOKEN}"}}}));
    let manager = McpManager::start(&settings);
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.snapshot().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut registry = Registry::new();
    for tool in manager.snapshot() {
        registry.register(Box::new(tool));
    }
    let cx = ToolContext {
        binding: None,
        workspace: "/tmp/opencode".into(),
        readable_roots: vec![],
        writable_roots: vec![],
    };
    let result = registry
        .dispatch("mcp__fake__secret", json!({}), Default::default(), &cx)
        .await
        .unwrap();
    assert_eq!(result.output, "[redacted]");
    let result = registry
        .dispatch(
            "mcp__fake__secret",
            json!({"rpc_error":true}),
            Default::default(),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(result.error.unwrap().code, "mcp_tool_error");
    let boundary = format!("{}{} retained tail", "x".repeat(65534), sentinel);
    let result = registry
        .dispatch(
            "mcp__fake__echo",
            json!({"text":boundary}),
            Default::default(),
            &cx,
        )
        .await
        .unwrap();
    assert!(!result.output.contains("mcp-env"));
    assert!(result.truncated.is_some());
    let tail = &manager.statuses()[0].stderr_tail;
    assert!(tail.ends_with("[redacted] retained tail"));
    assert!(!tail.contains(sentinel));
    manager.shutdown().await;
    std::env::remove_var("WITHNAV_MCP_TEST_TOKEN");
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        !logs.contains(sentinel),
        "logs exposed the environment secret"
    );
    assert!(!logs.contains("peer_info"));
}
