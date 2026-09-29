//! The navigator profile: creating a session with `profile: "navigator"`
//! resolves the `cognition` alias and pushes the cognition role prompt as a
//! SECOND system message at the head of the actor's transcript — observed
//! on the wire as the request the provider receives (wiremock), with the
//! Phase-1 agent loop's own system contract still first.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use rust_agent::server::{AppState, ServerConfig};

fn write_test_models(dir: &Path, base_url: &str) {
    std::fs::write(
        dir.join("models.json"),
        json!({
            "providers": {
                "mock": {
                    "base_url": base_url,
                    "api_key_env": "RUST_AGENT_TEST_NAV_KEY",
                    "headers": {},
                    "connect_timeout_ms": 2000,
                    "request_timeout_ms": 600000
                }
            },
            "models": {
                "cognition": {
                    "provider": "mock",
                    "model": "mock-opus",
                    "reasoning_history": "preserve"
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
}

fn test_state(tmp: &Path, base_url: &str) -> AppState {
    write_test_models(tmp, base_url);
    let config_dir = tmp.join("cfg");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let auth_path = config_dir.join("auth.json");
    std::fs::write(
        &auth_path,
        json!({ "mock": { "api_key": "test-key" } }).to_string(),
    )
    .expect("write auth.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
    }
    AppState::new(ServerConfig {
        bind: "127.0.0.1:0".parse().expect("loopback bind"),
        models_path: Some(tmp.join("models.json")),
        config_dir,
        allow_non_loopback: false,
        bearer_token: None,
        history_db: None,
    })
}

/// Drains the sequencer stream (replay + live) until `pred` matches, then
/// settles and drains what is ready in the same tick.
async fn collect_until(
    sub: rust_agent::bus::SubscribeReply,
    pred: impl Fn(&rust_agent::bus::event::EventEnvelope) -> bool,
) -> Vec<rust_agent::bus::event::EventEnvelope> {
    let mut out = Vec::new();
    if let rust_agent::bus::ResumeOutcome::Replay(replay) = sub.replay {
        out.extend(replay);
    }
    if let Some(mut live) = sub.live {
        while let Some(env) = tokio::time::timeout(Duration::from_secs(5), live.recv())
            .await
            .ok()
            .flatten()
        {
            let matched = pred(&env);
            out.push(env);
            if matched {
                tokio::time::sleep(Duration::from_millis(50)).await;
                while let Ok(env) = live.try_recv() {
                    out.push(env);
                }
                break;
            }
        }
    }
    out
}

// Given a session created with profile "navigator" against a scripted
// wiremock provider
// When a message is posted and the turn completes
// Then the request the provider received carries TWO system messages — the
//      agent loop's own contract FIRST, COGNITION_PROMPT SECOND — before
//      the user message; and the session resolved the `cognition` alias.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigator_profile_sends_the_cognition_prompt_as_second_system_message() {
    let server = MockServer::start().await;
    let done =
        r#"{"choices":[{"index":0,"delta":{"content":"guide answer"},"finish_reason":"stop"}]}"#;
    let sse = format!("data: {done}\n\ndata: [DONE]\n\n");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let state = test_state(&root, &server.uri());
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws dir");

    let id = state
        .create_session(&ws, None, Some("navigator"))
        .await
        .expect("create navigator session");
    let handle = state.handle(id).expect("live");
    assert_eq!(
        handle.model, "cognition",
        "the navigator profile resolves the cognition alias"
    );
    let sub = handle
        .sequencer
        .subscribe_since(None)
        .await
        .expect("subscribe");

    // Post the message through the same route core POST /messages uses.
    let command = rust_agent::server::routes::build_command(
        id,
        &rust_agent::server::routes::PostMessageRequest {
            target_actor_id: "agent:main".to_string(),
            mode: Some("next-turn".to_string()),
            content: vec![rust_agent::server::routes::WireContentBlock::Text {
                text: "hello there".to_string(),
            }],
        },
        rust_agent::bus::command::DeliveryMode::NextTurn,
    );
    let ack = rust_agent::server::routes::route_message(&handle, command)
        .await
        .expect("routed");
    assert!(ack.accepted);

    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == "turn.end" && e.payload["outcome"] == "done"),
        "the scripted turn completed: {:?}",
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>()
    );

    // The wire request: the cognition prompt rides as the SECOND system
    // message, behind the agent loop's own contract, ahead of the user turn.
    let requests = server.received_requests().await;
    let requests = requests.expect("wiremock retains requests");
    assert_eq!(requests.len(), 1, "exactly one provider call");
    let body: Value =
        serde_json::from_slice(&requests[0].body).expect("request body is the chat request");
    assert_eq!(body["model"], "mock-opus");
    let messages = body["messages"].as_array().expect("messages array");
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap_or("")).collect();
    let system_count = roles.iter().filter(|r| **r == "system").count();
    assert_eq!(
        system_count, 2,
        "two system messages (loop contract + cognition role): {roles:?}"
    );
    assert_eq!(messages[0]["role"], "system", "loop contract first");
    assert_eq!(messages[1]["role"], "system", "cognition prompt second");
    assert_eq!(
        messages[1]["content"],
        rust_agent::server::session::COGNITION_PROMPT,
        "the second system message IS the cognition prompt"
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "hello there");
}
