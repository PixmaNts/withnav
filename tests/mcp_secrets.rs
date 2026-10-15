//! Configured MCP secrets never reach the provider, the event store or doctor output, in
//! any of the representations a server can emit them in.
use serde_json::json;
use std::time::Duration;
use withnav::server::AppState;

mod common;
use common::server::{captain_command, stored_events, wait_for, TestServerConfig};
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_response, tool_calls_chunk};

/// A newline, a double quote and a backslash all change representation in JSON text.
const SECRET: &str = "multi\nline\"quote\\back-mcp-sentinel";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escaped_secrets_never_reach_provider_events_or_store() {
    let env = common::TestEnv::new();
    let provider = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[&tool_calls_chunk(&[
            ("structured", "mcp__fake__leak_structured", json!({})),
            ("text", "mcp__fake__leak_text", json!({})),
        ])])),
        text_response("done"),
    ])
    .await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .config();
    let mut settings = withnav::settings::load(&cfg.config_dir).unwrap();
    settings.set_dotted("mcp", json!({"fake":{"command":[common::python(),concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/mcp_server.py")],"env":{"SECRET":SECRET}}}));
    settings.set_dotted("yolo", json!(true));
    withnav::settings::save(&cfg.config_dir, &settings).unwrap();
    let history_db = cfg.history_db.clone();
    let state = AppState::new(cfg).unwrap();
    wait_for("MCP discovery", || {
        state
            .service()
            .mcp_statuses()
            .iter()
            .any(|s| s.server_id == "fake" && s.state == "ready")
    })
    .await;
    let status = state.service().mcp_statuses().remove(0);
    assert!(
        status
            .skipped_tools
            .iter()
            .any(|reason| reason.contains("contains a configured secret")),
        "{:?}",
        status.skipped_tools
    );
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    state
        .send(captain_command(id, "leak".into()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while provider.bodies().len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the tool results reach the provider");
    let posts = stored_events(&state, id, "tool.post_call").await;
    assert_eq!(posts.len(), 2, "{posts:?}");
    for post in &posts {
        assert!(post.payload["output"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));
    }
    let first = provider.bodies()[0].clone();
    assert!(first.contains("mcp__fake__described"));
    assert!(!first.contains("mcp__fake__named_"));
    for body in provider.bodies() {
        assert!(!body.contains("mcp-sentinel"), "provider saw: {body}");
    }
    for post in posts {
        assert!(!post.payload.to_string().contains("mcp-sentinel"));
    }
    state.service().close_all().await;
    for suffix in ["", "-wal"] {
        let path = format!("{}{suffix}", history_db.display());
        if let Ok(bytes) = std::fs::read(&path) {
            assert!(
                !bytes.windows(13).any(|w| w == b"-mcp-sentinel"),
                "{path} stores the secret"
            );
        }
    }
    let joined = status.skipped_tools.join(" ");
    assert!(!joined.contains("mcp-sentinel"));
}
