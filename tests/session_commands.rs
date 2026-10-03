//! Session commands: the built-ins through `Service::run_command`, the
//! refusals, the durable `command.result` and its replay, the HTTP endpoints
//! and `navai run`.

use axum::http::StatusCode;
use serde_json::{json, Value};

use navai::bus::event::EventEnvelope;
use navai::server::{app, AppState};

mod common;
use common::server::{
    collect_until, post_message_ok, request_json, spawn_server, TestServerConfig,
};
use common::sse::{http_sse, spawn_capturing_provider, sse_body};

fn tmp_ws() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws");
    (tmp, ws)
}

fn state(tmp: &std::path::Path) -> AppState {
    TestServerConfig::unreachable(tmp)
        .extra_alias("other", "other-model")
        .state()
}

async fn results(state: &AppState, id: ulid::Ulid) -> Vec<EventEnvelope> {
    state.history().flush().await.expect("flush");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        navai::bus::ResumeOutcome::Replay(events) => events
            .into_iter()
            .filter(|e| e.kind == "command.result")
            .collect(),
        other => panic!("expected replay, got {other:?}"),
    }
}

// Given a live session
// When /help runs
// Then it lists every registered command and publishes a durable command.result
#[tokio::test]
async fn help_lists_commands_and_publishes_a_result() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state.create_session(&ws, None, None).await.expect("create");
    let out = state.run_command(id, "help", "").await.expect("run");
    assert!(out.ok, "{out:?}");
    let names: Vec<String> = out.data.as_ref().unwrap()["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["help", "model", "new", "stop", "context", "variant", "compact"]
    );
    assert_eq!(state.list_commands().len(), 7);

    let events = results(&state, id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].payload["operation_id"],
        out.operation_id.to_string()
    );
    assert_eq!(events[0].payload["name"], "help");
    assert_eq!(events[0].payload["ok"], true);
}

// Given a session on the default alias
// When /model runs (with a leading slash too)
// Then it names the alias, the window and where the window came from
#[tokio::test]
async fn model_reports_alias_window_and_source() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state.create_session(&ws, None, None).await.expect("create");
    let out = state.run_command(id, "/model", "").await.expect("run");
    assert!(out.ok, "{out:?}");
    let data = out.data.unwrap();
    assert_eq!(data["alias"], "default");
    assert_eq!(data["model"], "mock-model");
    assert_eq!(data["window"], 128000);
    assert_eq!(data["window_source"], "models.json");
}

// Given a session
// When /new runs with and without an alias
// Then a fresh session exists each time, the old one stays live, and the id returns
#[tokio::test]
async fn new_creates_a_fresh_session_and_keeps_the_old_one() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state.create_session(&ws, None, None).await.expect("create");

    let out = state.run_command(id, "new", "").await.expect("run");
    assert!(out.ok, "{out:?}");
    let fresh: ulid::Ulid = out.data.as_ref().unwrap()["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(fresh, id);
    assert_eq!(state.session_summary(fresh).await.unwrap().model, "default");

    let out = state.run_command(id, "new", "other").await.expect("run");
    let other: ulid::Ulid = out.data.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(state.session_summary(other).await.unwrap().model, "other");

    assert_eq!(state.session_summary(id).await.unwrap().actor_state, "idle");
    assert_eq!(state.session_count(), 3);

    let bad = state.run_command(id, "new", "nope").await.expect("run");
    assert!(!bad.ok);
    assert!(bad.error.unwrap().contains("nope"), "names the alias");
    assert_eq!(state.session_count(), 3, "no session for a bad alias");
}

// Given a session
// When /stop runs
// Then its command.result is stored before the session ends
#[tokio::test]
async fn stop_ends_the_session_after_publishing_its_result() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state.create_session(&ws, None, None).await.expect("create");
    let out = state.run_command(id, "stop", "").await.expect("run");
    assert!(out.ok);
    assert_eq!(
        state.session_summary(id).await.unwrap().actor_state,
        "stopped"
    );
    let events = results(&state, id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["name"], "stop");
}

// Given a session
// When the command is unknown, or registered but not built yet
// Then the outcome is an error that lists close names, or says "not available yet"
#[tokio::test]
async fn unknown_and_unbuilt_commands_answer_clear_errors() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state.create_session(&ws, None, None).await.expect("create");

    let out = state.run_command(id, "hlep", "").await.expect("run");
    assert!(!out.ok);
    assert!(out.error.unwrap().contains("/help"));

    for name in ["context", "compact"] {
        let out = state.run_command(id, name, "").await.expect("run");
        assert!(!out.ok, "{name}");
        assert_eq!(out.error.unwrap(), format!("/{name} is not available yet"));
    }
    let events = results(&state, id).await;
    assert_eq!(events.len(), 3, "failures are published too");
    assert!(events.iter().all(|e| e.payload["ok"] == false));
}

// Given a session that does not exist, or a stopped one
// When a command is run
// Then it is a session error, not a command outcome
#[tokio::test]
async fn a_session_that_cannot_take_commands_is_an_error() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let err = state
        .run_command(ulid::Ulid::new(), "help", "")
        .await
        .unwrap_err();
    assert!(matches!(err, navai::error::ServerError::SessionNotFound(_)));
    let id = state.create_session(&ws, None, None).await.expect("create");
    state.close_session(id).await.expect("close");
    state.history().flush().await.expect("flush");
    let err = state.run_command(id, "help", "").await.unwrap_err();
    assert!(matches!(err, navai::error::ServerError::ResumeRequired));
}

// Given an agent waiting on its `ask`
// When a command arrives
// Then it is refused, never queued, and the ask is still answered by the next message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_during_a_pending_ask_is_refused() {
    let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"which scope?\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let provider = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[ask])),
        http_sse(&sse_body(&[done])),
    ])
    .await;
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::mock(tmp.path(), &provider.url).state();
    let (addr, _task) = spawn_server(state.clone()).await;
    let base = format!("http://{addr}");
    let id = state.create_session(&ws, None, None).await.expect("create");
    let sub = state.subscribe(id, None).await.expect("sub");
    post_message_ok(&base, &id.to_string(), "go").await;
    collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;

    for name in ["help", "new", "stop"] {
        let out = state.run_command(id, name, "").await.expect("run");
        assert!(!out.ok, "{name}");
        assert!(out.error.unwrap().contains("waiting for an answer"));
    }
    assert_eq!(state.session_count(), 1, "/new did not run");
    assert_eq!(
        state.session_summary(id).await.unwrap().actor_state,
        "idle",
        "/stop did not run"
    );

    // The refused commands queued nothing: the next message answers the ask
    // and is the only thing the agent sees.
    let sub = state.subscribe(id, None).await.expect("sub");
    post_message_ok(&base, &id.to_string(), "local").await;
    collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    let bodies = provider.bodies();
    let last: Value = serde_json::from_str(bodies.last().unwrap()).unwrap();
    let tool: Vec<&Value> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tool.len(), 1);
    assert_eq!(tool[0]["content"], "local");
    assert_eq!(
        last["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .count(),
        1
    );
}

// Given the HTTP surface
// When commands are listed and run
// Then the bodies are {commands} and {operation_id, ok, data?, error?}
#[tokio::test]
async fn http_endpoints_list_and_run_commands() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let router = app(state.clone());
    let id = state.create_session(&ws, None, None).await.expect("create");

    let (status, body) = request_json(&router, "GET", "/v1/commands", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["commands"][0]["name"], "help");
    assert_eq!(body["commands"][2]["args_hint"], "[alias]");

    let path = format!("/v1/sessions/{id}/commands");
    let (status, body) =
        request_json(&router, "POST", &path, Some(&json!({"name": "model"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["alias"], "default");
    assert!(body["operation_id"]
        .as_str()
        .unwrap()
        .parse::<ulid::Ulid>()
        .is_ok());
    assert!(body.get("error").is_none());

    let (status, body) = request_json(
        &router,
        "POST",
        &path,
        Some(&json!({"name": "compact", "arguments": "tests"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"], "/compact is not available yet");
    assert!(body.get("data").is_none());

    let (status, _) = request_json(&router, "POST", &path, Some(&json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{}/commands", ulid::Ulid::new()),
        Some(&json!({"name": "help"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// Given an oversized data payload
// When it is bounded for command.result
// Then a marker replaces it
#[test]
fn oversized_result_data_is_replaced_by_a_marker() {
    let big = json!({"k": "x".repeat(20_000)});
    let bounded = navai::bus::event::CommandResult::bound_data(big);
    assert_eq!(bounded["truncated"], true);
}

fn run_navai(
    root: &std::path::Path,
    models: &std::path::Path,
    ws: &std::path::Path,
    prompt: &str,
) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_navai"))
        .args(["run", "--workspace"])
        .arg(ws)
        .arg("--models")
        .arg(models)
        .args(["--prompt", prompt])
        .env("NAVAI_TEST_KEY", "test-key")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .output()
        .expect("run the binary")
}

// Given `navai run --prompt "/help"`
// Then it prints the command list and exits 0 without a provider call; an
//      unknown command prints an error and exits 1
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navai_run_executes_a_command() {
    let (tmp, ws) = tmp_ws();
    let cfg = TestServerConfig::unreachable(tmp.path());
    cfg.config();
    let (root, models) = (tmp.path().to_path_buf(), cfg.models_path());
    let (ok, bad) = tokio::task::spawn_blocking(move || {
        (
            run_navai(&root, &models, &ws, "/help"),
            run_navai(&root, &models, &ws, "/nope"),
        )
    })
    .await
    .expect("join");
    assert_eq!(ok.status.code(), Some(0), "{ok:?}");
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(
        stdout.contains("/help") && stdout.contains("/new [alias]"),
        "{stdout}"
    );
    assert_eq!(bad.status.code(), Some(1), "{bad:?}");
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown command /nope"));
}

// Given `navai run --prompt "//text"`
// Then the agent receives the literal message "/text"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navai_run_double_slash_sends_a_literal_message() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[done]))]).await;
    let (tmp, ws) = tmp_ws();
    let cfg = TestServerConfig::mock(tmp.path(), &provider.url);
    cfg.config();
    let (root, models) = (tmp.path().to_path_buf(), cfg.models_path());
    let out = tokio::task::spawn_blocking(move || run_navai(&root, &models, &ws, "//text"))
        .await
        .expect("join");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let body: Value = serde_json::from_str(&provider.bodies()[0]).unwrap();
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"], "/text");
}
