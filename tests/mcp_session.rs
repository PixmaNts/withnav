//! MCP tools inside sessions: shutdown order, subagent inventories and `nav run` startup.
use serde_json::{json, Value};
use std::time::Duration;
use withnav::server::{AppState, ServerConfig};

mod common;
use common::server::{captain_command, wait_for, TestServerConfig};
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
        json!({"command":["python3",FIXTURE],"env":{"SLOW_SECONDS":"5"}}),
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
