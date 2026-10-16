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
