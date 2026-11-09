//! Streamable HTTP MCP servers against a local fake: headers reach the server, their
//! credentials never come back in results, statuses or logs, and failures say why.
//! One test per binary: it installs the global subscriber and changes the environment.

use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing_subscriber::prelude::*;
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::any};
use withnav::{
    mcp::McpManager,
    settings::Settings,
    tools::{Registry, ToolContext, ToolStatus},
};

const TOKEN: &str = "mcp-http-header-sentinel";

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

/// A stateless Streamable HTTP MCP server answering every POST with one JSON body. It
/// refuses a request without the expected bearer token, as a hosted server would.
fn fake_mcp(request: &Request) -> ResponseTemplate {
    let authorized = request
        .headers
        .get("authorization")
        .is_some_and(|value| value.as_bytes() == format!("Bearer {TOKEN}").as_bytes());
    if request.url.path() == "/hang" {
        return ResponseTemplate::new(200).set_delay(Duration::from_secs(5));
    }
    if !authorized && request.url.path() == "/bare" {
        // A refusal without a challenge, its body naming a value that must not be shown.
        return ResponseTemplate::new(403).set_body_string("forbidden: body-sentinel");
    }
    if !authorized {
        return ResponseTemplate::new(401).insert_header("www-authenticate", "Bearer");
    }
    if request.method.as_str() != "POST" {
        return ResponseTemplate::new(405);
    }
    let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
    let Some(id) = body.get("id").cloned() else {
        return ResponseTemplate::new(202);
    };
    let result = match body["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": body["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake-http", "version": "1.0.0"},
        }),
        Some("tools/list") => json!({"tools": [
            {"name": "echo", "description": "Echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
            {"name": "whoami", "description": "Echo the authorization header", "inputSchema": {"type": "object"}},
        ]}),
        Some("resources/read") => json!({"contents": [{
            "uri": body["params"]["uri"], "mimeType": "text/html;profile=mcp-app", "blob": TOKEN,
        }]}),
        Some("tools/call") => {
            let text = match body["params"]["name"].as_str() {
                Some("echo") => body["params"]["arguments"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                _ => request.headers["authorization"]
                    .to_str()
                    .unwrap()
                    .to_owned(),
            };
            json!({"content": [{"type": "text", "text": text}]})
        }
        _ => {
            return ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown method"},
            }));
        }
    };
    ResponseTemplate::new(200).set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[tokio::test]
async fn http_server_sends_headers_redacts_them_and_reports_reachability() {
    let logs = Captured::default();
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("trace"))
        .with(withnav::app::logging::mcp_safe_filter())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(logs.clone()),
        )
        .init();
    let fake = MockServer::start().await;
    Mock::given(any()).respond_with(fake_mcp).mount(&fake).await;
    // A port nothing listens on: bound, then released.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    // SAFETY: this binary holds this one test, and no server is started yet, so no other thread
    // reads the environment while it changes.
    unsafe { std::env::set_var("WITHNAV_MCP_HTTP_TEST_TOKEN", TOKEN) };
    let mut settings = Settings::default();
    settings.set_dotted(
        "mcp",
        json!({
            "web": {
                "type": "http",
                "url": format!("{}/web", fake.uri()),
                "headers": {"Authorization": "Bearer {env:WITHNAV_MCP_HTTP_TEST_TOKEN}", "X-Client": "withnav"},
            },
            "noauth": {"type": "http", "url": format!("{}/noauth", fake.uri())},
            "down": {"type": "http", "url": format!("http://127.0.0.1:{closed_port}/mcp")},
            "bare": {"type": "http", "url": format!("{}/bare", fake.uri())},
            "hang": {"type": "http", "url": format!("{}/hang", fake.uri()), "startup_timeout_ms": 300},
        }),
    );
    let manager = McpManager::start(&settings);
    let status = |id: &str| {
        manager
            .statuses()
            .into_iter()
            .find(|s| s.server_id == id)
            .unwrap()
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while status("web").state != "ready"
            || status("noauth").error.is_none()
            || status("down").error.is_none()
            || status("hang").error.is_none()
            || status("bare").error.is_none()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("servers must settle: {:?}", manager.statuses()));

    let snapshot = manager.status_snapshot();
    let web = snapshot
        .servers
        .iter()
        .find(|s| s.server_id == "web")
        .unwrap();
    assert_eq!((web.transport.as_str(), web.tool_count), ("http", 2));
    let noauth = status("noauth").error.unwrap();
    assert!(noauth.contains("authorization"), "{noauth}");
    let down = status("down").error.unwrap();
    assert!(down.contains("unreachable"), "{down}");
    assert_eq!(status("hang").error.unwrap(), "MCP startup timed out");
    let bare = status("bare").error.unwrap();
    assert!(bare.contains("authorization (HTTP 403)"), "{bare}");
    assert!(!bare.contains("body-sentinel"), "{bare}");

    let mut registry = Registry::new();
    for tool in manager.snapshot() {
        registry.register(Box::new(tool));
    }
    let cx = ToolContext {
        secrets: None,
        binding: None,
        workspace: std::env::temp_dir(),
        readable_roots: vec![],
        writable_roots: vec![],
    };
    let echo = registry
        .dispatch(
            "mcp__web__echo",
            json!({"text": "hello"}),
            Default::default(),
            &cx,
        )
        .await
        .unwrap();
    assert_eq!(
        (echo.status, echo.output.as_str()),
        (ToolStatus::Ok, "hello")
    );
    let whoami = registry
        .dispatch("mcp__web__whoami", json!({}), Default::default(), &cx)
        .await
        .unwrap();
    assert_eq!(whoami.output, "[redacted]");
    let resource = manager
        .read_ui_resource("web", "ui://web/app.html")
        .await
        .unwrap();
    assert!(resource["blob"].is_null(), "{resource}");

    let posts: Vec<_> = fake
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/web" && r.method.as_str() == "POST")
        .collect();
    assert!(posts.len() >= 4, "initialize, initialized, list, calls");
    for post in &posts {
        assert_eq!(post.headers["x-client"], "withnav");
        assert_eq!(
            post.headers["authorization"].as_bytes(),
            format!("Bearer {TOKEN}").as_bytes()
        );
    }
    manager.shutdown().await;
    // SAFETY: the manager is shut down, so nothing reads the environment any more.
    unsafe { std::env::remove_var("WITHNAV_MCP_HTTP_TEST_TOKEN") };

    let statuses = format!("{:?}", manager.statuses());
    assert!(!statuses.contains(TOKEN), "{statuses}");
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains(TOKEN), "logs exposed the header secret");
}
