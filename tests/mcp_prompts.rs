#![cfg(feature = "test-support")]
//! A server's prompts become the Captain's `/<server>:<prompt>` commands: listed with the
//! built-ins, their arguments checked before anything is asked, and their messages sent to
//! the agent as bounded, redacted, untrusted data inside the Captain's message.
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use withnav::{
    mcp::{McpManager, PROMPT_BLOCK_CLOSE, PROMPT_BLOCK_OPEN},
    server::{AppState, app},
    settings::Settings,
};
mod common;
use common::TestEnv as Env;
use common::server::{TestServerConfig, request_json, stored_events};
use common::sse::{spawn_capturing_provider, text_response};

const SECRET: &str = "prompt-secret-sentinel";

fn manager(env: Value, server: Value) -> std::sync::Arc<McpManager> {
    let mut config = json!({
        "command": [common::python(), concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py")],
        "env": env,
        "call_timeout_ms": 2000,
    });
    if let (Some(config), Some(extra)) = (config.as_object_mut(), server.as_object()) {
        config.extend(extra.clone());
    }
    let mut settings = Settings::default();
    settings.set_dotted("mcp", json!({ "fixture": config }));
    McpManager::start(&settings)
}

async fn settled(manager: &McpManager) -> withnav::mcp::ServerStatus {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = manager.statuses().remove(0);
            if status.state != "starting" {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture must settle")
}

/// The JSON block of a prompt message, decoded.
fn block(text: &str) -> Value {
    let start = text.find(PROMPT_BLOCK_OPEN).expect("block") + PROMPT_BLOCK_OPEN.len();
    let end = text.rfind(PROMPT_BLOCK_CLOSE).expect("block end");
    serde_json::from_str(&text[start..end]).expect("valid JSON")
}

fn redact_nothing(text: &str) -> String {
    text.to_owned()
}

// Given a server that offers prompts over two pages
// Then each typable prompt is a `<server>:<prompt>` command with its arguments as the hint,
//      and a prompt whose name is not one word is skipped with a note
#[tokio::test]
async fn prompts_are_listed_as_server_commands() {
    let manager = manager(json!({"PROMPTS": "1"}), json!({}));
    let status = settled(&manager).await;
    assert_eq!(status.state, "ready");
    let commands = manager.prompt_commands();
    let names: Vec<_> = commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "fixture:big",
            "fixture:fail",
            "fixture:hello",
            "fixture:review",
            "fixture:slow"
        ]
    );
    let review = &commands[3];
    assert_eq!(review.args_hint, "<topic> [depth]");
    assert_eq!(review.description, "Review some code");
    assert_eq!(commands[2].description, "Say hello");
    assert_eq!(commands[0].description, "MCP prompt from fixture");
    assert!(
        status
            .skipped_tools
            .iter()
            .any(|s| s.contains("not a single command word")),
        "{:?}",
        status.skipped_tools
    );
    assert!(manager.prompt("fixture:nope").is_none());
    assert!(manager.prompt("other:review").is_none());
    manager.shutdown().await;
}

#[tokio::test]
async fn a_server_without_prompts_lists_none_and_a_prompts_only_server_is_not_asked_for_tools() {
    let plain = manager(json!({}), json!({}));
    assert_eq!(settled(&plain).await.state, "ready");
    assert!(plain.prompt_commands().is_empty());
    plain.shutdown().await;

    let only = manager(json!({"PROMPTS_ONLY": "1"}), json!({}));
    let status = settled(&only).await;
    assert_eq!(status.state, "ready", "{:?}", status.error);
    assert_eq!(status.tool_count, 0);
    assert_eq!(only.prompt_commands().len(), 5);
    only.shutdown().await;
}

// Given a server whose prompts/list never answers
// Then it still starts, with its tools and no prompts, and says why
#[tokio::test]
async fn a_hanging_prompt_listing_keeps_the_server_and_its_tools() {
    let manager = manager(
        json!({"PROMPTS": "1", "PROMPTS_HANG": "1"}),
        json!({"startup_timeout_ms": 1000}),
    );
    let status = settled(&manager).await;
    assert_eq!(status.state, "ready", "{:?}", status.error);
    assert!(status.tool_count > 0);
    assert!(manager.prompt_commands().is_empty());
    assert!(
        status
            .skipped_tools
            .iter()
            .any(|s| s.contains("prompts/list timed out")),
        "{:?}",
        status.skipped_tools
    );
    manager.shutdown().await;
}

// Given a prompt with a required argument
// When the Captain runs it without that argument, then with it
// Then the first run is refused with the usage before the server is asked; the second
//      brings back the messages as text: pictures named, secrets hidden, delimiter escaped
#[tokio::test]
async fn a_prompt_is_fetched_as_redacted_untrusted_text() {
    let manager = manager(json!({"PROMPTS": "1", "SECRET": SECRET}), json!({}));
    settled(&manager).await;
    // A prompt with an argument named after a configured secret is never listed.
    let listing = serde_json::to_string(&manager.prompt_commands()).unwrap();
    assert!(
        !listing.contains("leaky") && !listing.contains("sentinel"),
        "{listing}"
    );
    let review = manager.prompt("fixture:review").unwrap();
    let usage = review.arguments("").unwrap_err();
    assert!(usage.contains("missing argument topic"), "{usage}");
    assert!(
        usage.contains("usage: /fixture:review <topic> [depth]"),
        "{usage}"
    );
    assert!(usage.contains("What to review"), "{usage}");

    let arguments = review.arguments("topic=parser depth=2").unwrap();
    let session_secret = |text: &str| text.replace("parser", "[secret PARSER]");
    let fetched = review
        .fetch(arguments, &CancellationToken::new(), &session_secret)
        .await
        .unwrap();
    assert!(
        fetched.text.starts_with(&format!(
            "Run the MCP prompt /fixture:review.\n{PROMPT_BLOCK_OPEN}"
        )),
        "{}",
        fetched.text
    );
    assert!(!fetched.text.contains(SECRET));
    assert_eq!(fetched.text.matches(PROMPT_BLOCK_CLOSE).count(), 1);
    assert_eq!(fetched.messages, 3);
    let value = block(&fetched.text);
    assert_eq!(value["server"], "fixture");
    assert_eq!(value["arguments"]["topic"], "[secret PARSER]");
    assert!(value["note"].as_str().unwrap().contains("untrusted"));
    let messages = value["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    let first = messages[0]["text"].as_str().unwrap();
    assert!(
        first.starts_with("Review [secret PARSER] at depth 2"),
        "{first}"
    );
    assert_eq!(
        messages[1]["text"],
        "[image: image/png, 8 bytes, not shown]"
    );
    assert_eq!(
        messages[2]["text"],
        "[resource x://doc (text/plain)]\ndoc body"
    );
    assert!(!value["description"].as_str().unwrap().contains(SECRET));
    manager.shutdown().await;
}

#[tokio::test]
async fn a_large_prompt_is_bounded_and_server_errors_stay_hidden() {
    let manager = manager(json!({"PROMPTS": "1", "SECRET": SECRET}), json!({}));
    settled(&manager).await;
    let big = manager.prompt("fixture:big").unwrap();
    let fetched = big
        .fetch(
            big.arguments("").unwrap(),
            &CancellationToken::new(),
            &redact_nothing,
        )
        .await
        .unwrap();
    assert!(fetched.truncated);
    let start = fetched.text.find(PROMPT_BLOCK_OPEN).unwrap() + PROMPT_BLOCK_OPEN.len();
    let end = fetched.text.rfind(PROMPT_BLOCK_CLOSE).unwrap();
    assert!(end - start <= 64 * 1024 + 2, "{}", end - start);
    assert_eq!(block(&fetched.text)["truncated"], true);

    let fail = manager.prompt("fixture:fail").unwrap();
    let error = fail
        .fetch(
            Default::default(),
            &CancellationToken::new(),
            &redact_nothing,
        )
        .await
        .unwrap_err();
    assert_eq!(error, "MCP server rejected prompts/get");

    let hello = manager.prompt("fixture:hello").unwrap();
    assert!(
        hello.arguments("extra").is_err(),
        "a prompt without arguments takes none"
    );

    let slow = manager.prompt("fixture:slow").unwrap();
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.cancel();
    });
    let error = slow
        .fetch(Default::default(), &cancel, &redact_nothing)
        .await
        .unwrap_err();
    assert_eq!(error, "the prompt was cancelled");
    manager.shutdown().await;
}

// Given a session and a server that offers prompts
// When a client lists the commands and runs `/fake:review`
// Then the prompt is listed after the built-ins; without its argument it is refused;
//      with it, the Captain's message carries the prompt block and the agent receives it
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_runs_as_a_captain_command_through_the_session() {
    let env = Env::new();
    let provider = spawn_capturing_provider(vec![text_response("reviewed")]).await;
    let cfg = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.root.join("home"))
        .config();
    let mut settings = withnav::settings::load(&cfg.config_dir).unwrap();
    settings.set_dotted("mcp", json!({"fake":{"command":[common::python(),concat!(env!("CARGO_MANIFEST_DIR"),"/tests/fixtures/mcp_server.py")],"env":{"PROMPTS":"1","SECRET":SECRET}}}));
    withnav::settings::save(&cfg.config_dir, &settings).unwrap();
    let state = AppState::new(cfg).unwrap();
    common::server::wait_for("MCP discovery", || {
        state
            .service()
            .mcp_statuses()
            .iter()
            .any(|s| s.server_id == "fake" && s.state == "ready")
    })
    .await;
    let router = app(state.clone());
    let (status, body) = request_json(&router, "GET", "/v1/commands", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = body["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names[0], "help");
    let review = body["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "fake:review")
        .unwrap_or_else(|| panic!("{names:?}"));
    assert_eq!(review["args_hint"], "<topic> [depth]");

    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let path = format!("/v1/sessions/{id}/commands");
    let (status, refused) = request_json(
        &router,
        "POST",
        &path,
        Some(&json!({"name": "/fake:review", "arguments": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refused}");
    assert_eq!(refused["ok"], false);
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("missing argument topic")
    );
    assert!(
        stored_events(&state, id, "message.received")
            .await
            .is_empty()
    );

    let (status, ran) = request_json(
        &router,
        "POST",
        &path,
        Some(&json!({"name": "fake:review", "arguments": "topic=\"the parser\""})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ran}");
    assert_eq!(ran["ok"], true, "{ran}");
    assert_eq!(ran["data"]["server"], "fake");
    assert_eq!(ran["data"]["prompt"], "review");
    let received = stored_events(&state, id, "message.received").await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].payload["source"], "user:http");
    assert_eq!(received[0].payload["command_id"], ran["data"]["command_id"]);
    let content = received[0].payload["content"].as_str().unwrap();
    assert!(!content.contains(SECRET));
    assert_eq!(
        block(content)["messages"][0]["text"]
            .as_str()
            .unwrap()
            .split(" </")
            .next(),
        Some("Review the parser at depth -")
    );
    let results = stored_events(&state, id, "command.result").await;
    let names: Vec<_> = results.iter().map(|e| e.payload["name"].clone()).collect();
    assert_eq!(
        names.iter().filter(|n| *n == "fake:review").count(),
        2,
        "the refusal and the run each publish a result: {names:?}"
    );
    common::server::wait_for("the agent's request", || !provider.bodies().is_empty()).await;
    let request = &provider.bodies()[0];
    assert!(
        request.contains("Run the MCP prompt /fake:review."),
        "{request}"
    );
    assert!(request.contains("Review the parser at depth -"));
    assert!(!request.contains(SECRET));
    state.service().close_all().await;
}
