//! MCP tools inside sessions: shutdown order, subagent inventories and `nav run` startup.
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use withnav::server::{AppState, ServerConfig};

mod common;
use common::server::{TestServerConfig, captain_command, wait_for};
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_response, tool_calls_chunk};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py");

fn with_mcp(cfg: &ServerConfig, server: Value, yolo: bool) {
    let mut settings = withnav::settings::load(&cfg.config_dir).unwrap();
    settings.set_dotted("mcp", json!({ "fake": server }));
    settings.set_dotted("yolo", json!(yolo));
    withnav::settings::save(&cfg.config_dir, &settings).unwrap();
}

async fn ready(state: &AppState) {
    wait_for("MCP discovery", || {
        state
            .service()
            .mcp_statuses()
            .iter()
            .any(|s| s.server_id == "fake" && s.state == "ready")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_stops_sessions_before_mcp_so_a_queued_call_never_runs() {
    let env = common::TestEnv::new();
    let target = env.ws.join("queued.txt");
    let provider = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[&tool_calls_chunk(&[
            ("slow", "mcp__fake__slow", json!({})),
            (
                "queued",
                "write",
                json!({"path": target, "content": "must not exist"}),
            ),
        ])])),
        text_response("done"),
    ])
    .await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .config();
    with_mcp(
        &cfg,
        json!({"command":[common::python(),FIXTURE],"env":{"SLOW_SECONDS":"5"}}),
        true,
    );
    let state = AppState::new(cfg).unwrap();
    ready(&state).await;
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    state
        .send(captain_command(id, "slow then write".into()))
        .await
        .unwrap();
    wait_for("the slow MCP call", || !provider.bodies().is_empty()).await;
    // The provider answered; give the actor time to dispatch the slow call.
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::timeout(Duration::from_secs(15), state.service().close_all())
        .await
        .expect("shutdown is bounded");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!target.exists(), "the queued call ran during MCP shutdown");
}

/// A writable subagent with `mcp_child.md` and `mcp_denied.md` definitions.
fn install_agents(ws: &Path) {
    let dir = ws.join(".withnav/agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("mcpchild.md"),
        "---\nmode: subagent\naccess: full\n---\nUse the tools.",
    )
    .unwrap();
    std::fs::write(
        dir.join("mcpdenied.md"),
        "---\nmode: subagent\naccess: full\ntools:\n  mcp__fake__echo: false\n---\nNo echo.",
    )
    .unwrap();
}

async fn tool_names(provider: &common::sse::CapturingProvider, n: usize) -> Vec<String> {
    wait_for("the provider request", || provider.bodies().len() > n).await;
    let body: Value = serde_json::from_str(&provider.bodies()[n]).unwrap();
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagents_receive_mcp_tools_within_parent_and_definition_limits() {
    for (parent_agent, child_agent, expected) in [
        ("build", "mcpchild", true),
        ("build", "mcpdenied", false),
        ("nomcp", "mcpchild", false),
    ] {
        let env = common::TestEnv::new();
        install_agents(&env.ws);
        std::fs::write(
            env.ws.join(".withnav/agents/nomcp.md"),
            "---\nmode: primary\naccess: full\ntools:\n  mcp__fake__echo: false\n---\nNo echo.",
        )
        .unwrap();
        let provider = spawn_capturing_provider(vec![text_response("child done")]).await;
        let cfg = TestServerConfig::mock(&env.root, &provider.url)
            .chat_tools(128_000)
            .rules_home(&env.home)
            .config();
        with_mcp(&cfg, json!({"command":[common::python(),FIXTURE]}), false);
        let state = AppState::new(cfg).unwrap();
        ready(&state).await;
        let parent = state
            .create_session_as(&env.ws, None, false, Some(parent_agent))
            .await
            .unwrap();
        let child = state
            .create_child_session(parent, child_agent, "inspect")
            .await
            .unwrap();
        let policy: Value = serde_json::from_str(
            &state
                .child_metadata(child)
                .await
                .unwrap()
                .unwrap()
                .policy_snapshot,
        )
        .unwrap();
        assert_eq!(
            policy["mcp__fake__echo"],
            json!(expected),
            "{parent_agent} -> {child_agent}: {policy}"
        );
        let tools = tool_names(&provider, 0).await;
        assert_eq!(
            tools.iter().any(|t| t == "mcp__fake__echo"),
            expected,
            "{parent_agent} -> {child_agent}: {tools:?}"
        );
        if expected {
            assert!(tools.iter().any(|t| t == "mcp__fake__slow"), "{tools:?}");
        }
        state.service().close_all().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nav_run_waits_for_a_server_that_initialises_after_a_delay() {
    let env = common::TestEnv::new();
    let provider = spawn_capturing_provider(vec![text_response("done")]).await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url).rules_home(&env.home);
    cfg.config();
    let models = cfg.models_path();
    let config_dir = env.root.join("config/withnav");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.json"),
        json!({
            "agents":{"build":{"model":"mock/mock-model"}},
            "mcp":{"fake":{"command":[common::python(),FIXTURE],"env":{"INIT_DELAY_SECONDS":"1"},"startup_timeout_ms":5000}}
        })
        .to_string(),
    )
    .unwrap();
    let (root, ws) = (env.root.clone(), env.ws.clone());
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_nav"))
            .args(["run", "--workspace"])
            .arg(&ws)
            .arg("--models")
            .arg(&models)
            .args(["--prompt", "hello"])
            .env("WITHNAV_TEST_KEY", "test-key")
            .env("WITHNAV_CONFIG_DIR", root.join("config/withnav"))
            .env("WITHNAV_DATA_DIR", root.join("data/withnav"))
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let tools = tool_names(&provider, 0).await;
    assert!(tools.iter().any(|t| t == "mcp__fake__echo"), "{tools:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_inherits_the_parents_frozen_inventory_not_later_servers() {
    let env = common::TestEnv::new();
    install_agents(&env.ws);
    let provider = spawn_capturing_provider(vec![text_response("child done")]).await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .chat_tools(128_000)
        .rules_home(&env.home)
        .config();
    with_mcp(
        &cfg,
        json!({"command":[common::python(),FIXTURE],"env":{"INIT_DELAY_SECONDS":"1"}}),
        false,
    );
    let state = AppState::new(cfg).unwrap();
    // The parent freezes its inventory before the server is ready.
    let parent = state.create_session(&env.ws, None, false).await.unwrap();
    ready(&state).await;
    let child = state
        .create_child_session(parent, "mcpchild", "inspect")
        .await
        .unwrap();
    let policy: Value = serde_json::from_str(
        &state
            .child_metadata(child)
            .await
            .unwrap()
            .unwrap()
            .policy_snapshot,
    )
    .unwrap();
    assert!(policy.get("mcp__fake__echo").is_none(), "{policy}");
    let tools = tool_names(&provider, 0).await;
    assert!(!tools.iter().any(|t| t.starts_with("mcp__")), "{tools:?}");
    state.service().close_all().await;
}

// Given MCP App tool calls over the route, under its 2 MiB body limit
// Then arguments over 1 MiB or nested deeper than 64 levels get a 400 naming the bound, and
//      a call within both bounds reaches the server
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mcp_app_call_route_refuses_oversized_and_deep_arguments() {
    let env = common::TestEnv::new();
    let provider = spawn_capturing_provider(vec![]).await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .config();
    with_mcp(&cfg, json!({"command":[common::python(),FIXTURE]}), false);
    let state = AppState::new(cfg).unwrap();
    ready(&state).await;
    let router = withnav::server::app(state.clone());
    let call = |arguments: Value| {
        let router = router.clone();
        async move {
            let body = json!({"name": "echo", "arguments": arguments});
            common::server::request_json(&router, "POST", "/v1/mcp/fake/call", Some(&body)).await
        }
    };
    let (status, body) = call(json!({"text": "x".repeat(1536 * 1024)})).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(
        body.to_string()
            .contains("MCP tool arguments are larger than 1024 KiB"),
        "{body}"
    );
    let mut deep = json!(1);
    for _ in 0..64 {
        deep = json!([deep]);
    }
    let (status, body) = call(json!({"text": deep})).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(
        body.to_string()
            .contains("MCP tool arguments are nested deeper than 64 levels"),
        "{body}"
    );
    let (status, body) = call(json!({"text": "ok"})).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["content"][0]["text"], "{\"text\": \"ok\"}");
    state.service().close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_session_hears_the_mcp_status_and_the_route_returns_the_same_snapshot() {
    let env = common::TestEnv::new();
    let provider = spawn_capturing_provider(vec![]).await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .config();
    // The server answers initialize once the gate file exists, created after the session
    // subscribed: it turns ready while the session listens, however slow the runner is.
    let gate = env.root.join("mcp-ready");
    with_mcp(
        &cfg,
        json!({"command":[common::python(),FIXTURE],"env":{"INIT_GATE":gate},"startup_timeout_ms":30000}),
        false,
    );
    let state = AppState::new(cfg).unwrap();
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();
    std::fs::write(&gate, "").unwrap();
    let events = common::server::collect_until(sub, |e| {
        e.kind == "mcp.status" && e.payload["servers"][0]["state"] == "ready"
    })
    .await;
    let statuses: Vec<_> = events.iter().filter(|e| e.kind == "mcp.status").collect();
    let last = &statuses.last().unwrap().payload;
    assert_eq!(last["servers"][0]["server_id"], "fake");
    assert_eq!(last["servers"][0]["transport"], "stdio");
    assert!(
        last["servers"][0]["tool_count"].as_u64().unwrap() > 0,
        "{last}"
    );
    // Revisions never go back within one incarnation (a change seen twice may repeat one).
    let revisions: Vec<u64> = statuses
        .iter()
        .map(|e| e.payload["revision"].as_u64().unwrap())
        .collect();
    assert!(revisions.windows(2).all(|w| w[0] <= w[1]), "{revisions:?}");
    // Live-only: never stored, so a resumed conversation does not replay stale servers.
    assert!(
        common::server::stored_events(&state, id, "mcp.status")
            .await
            .is_empty()
    );

    let router = withnav::server::app(state.clone());
    let (status, body) = common::server::request_json(&router, "GET", "/v1/mcp", None).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["incarnation"], last["incarnation"]);
    assert!(
        body["revision"].as_u64() >= last["revision"].as_u64(),
        "{body}"
    );
    assert_eq!(body["servers"][0]["state"], "ready");
    state.service().close_all().await;
}

async fn raw(router: &axum::Router, method: &str, path: &str, body: &str) -> (u16, Value) {
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_owned()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

// Given MCP servers in every state an app can meet: ready, slow, crashing, disabled and
// misconfigured
// When an app reads resources and calls tools through the HTTP bridge
// Then each failure class has its own status and reason, and a server's oversized error text
// reaches the app redacted, on one line and cut to the documented bound
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_app_routes_map_each_failure_class_and_bound_server_errors() {
    let env = common::TestEnv::new();
    let cfg = TestServerConfig::unreachable(&env.root).config();
    let secret = "tok-sentinel-0123456789";
    let mut settings = withnav::settings::load(&cfg.config_dir).unwrap();
    settings.set_dotted(
        "mcp",
        json!({
            "fake": {"command": [common::python(), FIXTURE], "env": {"API_TOKEN": secret}},
            "slow": {"command": [common::python(), FIXTURE], "call_timeout_ms": 300, "env": {"SLOW_SECONDS": "30"}},
            "crashy": {"command": [common::python(), FIXTURE]},
            "off": {"command": [common::python(), FIXTURE], "enabled": false},
            "broken": {"command": [common::python(), FIXTURE], "call_timeout_ms": 0},
        }),
    );
    withnav::settings::save(&cfg.config_dir, &settings).unwrap();
    let state = AppState::new(cfg).unwrap();
    wait_for("MCP discovery", || {
        let statuses = state.service().mcp_statuses();
        ["fake", "slow", "crashy"].iter().all(|id| {
            statuses
                .iter()
                .any(|s| s.server_id == *id && s.state == "ready")
        })
    })
    .await;
    let router = withnav::server::app(state.clone());
    let call = |server: &str, body: Value| {
        let router = router.clone();
        let path = format!("/v1/mcp/{server}/call");
        async move { raw(&router, "POST", &path, &body.to_string()).await }
    };
    let resource = |server: &str, uri: &str| {
        let router = router.clone();
        let path = format!("/v1/mcp/{server}/resource?uri={uri}");
        async move { raw(&router, "GET", &path, "").await }
    };

    let (status, body) = resource("fake", "ui://fixture/app.html").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["mime_type"], "text/html;profile=mcp-app");
    let (status, body) = call("fake", json!({"name": "fail"})).await;
    assert_eq!(
        status, 200,
        "an isError result is a successful call: {body}"
    );
    assert_eq!(body["isError"], true);

    // The caller's mistakes: 400, with a stable message that never quotes the request back.
    let (status, body) = resource("fake", "file:///etc/passwd").await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (400, Some("mcp_invalid_request"))
    );
    let (status, body) = raw(&router, "GET", "/v1/mcp/fake/resource", "").await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (400, Some("mcp_invalid_request"))
    );
    let quoted = "q".repeat(10_000);
    let (status, body) = call("fake", json!({"name": "echo", "arguments": quoted})).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (400, Some("mcp_invalid_request"))
    );
    assert_eq!(body["error"], "invalid MCP App request");
    for (method, path) in [
        ("GET", "/v1/mcp/%FF/resource?uri=ui://fixture/app.html"),
        ("POST", "/v1/mcp/%FF/call"),
    ] {
        let (status, body) = raw(&router, method, path, "{\"name\":\"echo\"}").await;
        assert_eq!(
            (status, body["reason"].as_str()),
            (400, Some("mcp_invalid_request")),
            "{path}"
        );
    }
    let (status, body) = raw(&router, "POST", "/v1/mcp/fake/call", "{\"name\":").await;
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid MCP App request"))
    );
    let (status, body) = call(
        "fake",
        json!({"name": "echo", "arguments": {"rpc_error": "text must be a string"}}),
    )
    .await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (400, Some("mcp_invalid_params")),
        "{body}"
    );
    assert_eq!(
        body["error"],
        "MCP server refused the tools/call arguments: text must be a string"
    );

    // Unknown names: 404, and the requested id is not echoed.
    let long_id = "u".repeat(2_000);
    let (status, body) = call(&long_id, json!({"name": "echo"})).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (404, Some("unknown MCP server"))
    );
    assert_eq!(body["reason"], "mcp_unknown_server");
    let (status, body) = call("fake", json!({"name": "missing"})).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (404, Some("mcp_unknown_tool"))
    );

    // Configured but not serving: 503, whether disabled, misconfigured or gone mid-call.
    for server in ["off", "broken"] {
        let (status, body) = call(server, json!({"name": "echo"})).await;
        assert_eq!(
            (status, body["reason"].as_str()),
            (503, Some("mcp_unavailable")),
            "{server}"
        );
        let (status, body) = resource(server, "ui://fixture/app.html").await;
        assert_eq!(
            (status, body["reason"].as_str()),
            (503, Some("mcp_unavailable")),
            "{server}"
        );
    }
    let (status, body) = call("crashy", json!({"name": "crash"})).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (503, Some("mcp_unavailable")),
        "{body}"
    );

    // No answer within the server's call timeout: 504.
    let (status, body) = call("slow", json!({"name": "slow"})).await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (504, Some("mcp_timeout"))
    );
    assert_eq!(body["error"], "MCP tools/call timed out");

    // A server-side JSON-RPC error: 502, its oversized text redacted, one line, bounded.
    let oversized = format!("boom\n{}{secret}{}", "a".repeat(200), "z".repeat(10_000));
    let (status, body) = call(
        "fake",
        json!({"name": "echo", "arguments": {"rpc_error": oversized, "rpc_code": -32603}}),
    )
    .await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (502, Some("mcp_rejected")),
        "{body}"
    );
    let error = body["error"].as_str().unwrap();
    let detail = error
        .strip_prefix("MCP server rejected tools/call: ")
        .unwrap_or_else(|| panic!("unexpected message: {error}"));
    assert_eq!(detail.chars().count(), withnav::mcp::UI_ERROR_DETAIL_CHARS);
    assert!(
        detail.starts_with("boom a") && detail.ends_with('…'),
        "{detail}"
    );
    assert!(detail.contains("[redacted]"), "{detail}");
    assert!(!error.contains("tok-") && !error.contains('\n'), "{error}");

    state.service().close_all().await;
}
