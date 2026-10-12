//! Session commands: the built-ins through `Service::run_command`, the
//! refusals, the durable `command.result` and its replay, the HTTP endpoints
//! and `nav run`.

use axum::http::StatusCode;
use serde_json::{json, Value};

use withnav::bus::event::EventEnvelope;
use withnav::server::{app, AppState};

mod common;
use common::server::{
    collect_until, post_message_ok, request_json, spawn_server, temp_workspace as tmp_ws,
    TestServerConfig,
};
use common::sse::{http_sse, spawn_capturing_provider, sse_body};

fn state(tmp: &std::path::Path) -> AppState {
    TestServerConfig::unreachable(tmp)
        .extra_alias("other", "other-model")
        .state()
}

#[tokio::test]
async fn bypass_is_ephemeral_isolated_and_idempotently_sequenced() {
    use withnav::bus::runtime::bypass::BypassOrigin;
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let first = state.create_session(&ws, None, false).await.unwrap();
    let second = state.create_session(&ws, None, false).await.unwrap();
    let initial = state.session_summary(first).await.unwrap();
    assert!(!initial.bypass);
    assert!(initial.incarnation.is_some());
    let enabled = state
        .set_bypass(first, true, BypassOrigin::Http)
        .await
        .unwrap();
    assert!(enabled.enabled && enabled.changed);
    let repeated = state
        .set_bypass(first, true, BypassOrigin::Http)
        .await
        .unwrap();
    assert!(!repeated.changed);
    assert_eq!(repeated.bypass_seq, enabled.bypass_seq);
    let snapshot = state.session_summary(first).await.unwrap();
    assert!(snapshot.bypass);
    assert_eq!(snapshot.bypass_seq, enabled.bypass_seq);
    assert_eq!(snapshot.incarnation, initial.incarnation);
    assert!(!state.session_summary(second).await.unwrap().bypass);
    let disabled = state
        .set_bypass(first, false, BypassOrigin::Cli)
        .await
        .unwrap();
    assert!(!disabled.enabled && disabled.changed);
    assert!(disabled.bypass_seq > enabled.bypass_seq);
    state.history().flush().await.unwrap();
    let sub = state.subscribe(first, None).await.unwrap();
    let withnav::bus::ResumeOutcome::Replay(events) = sub.replay else {
        panic!("expected replay");
    };
    let toggles: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "session.bypass")
        .collect();
    assert_eq!(toggles.len(), 2);
    assert_eq!(toggles[0].payload, json!({"enabled":true,"by":"user:http"}));
    assert_eq!(toggles[1].payload, json!({"enabled":false,"by":"user:cli"}));
    state.close_session(first).await.unwrap();
    let stored = state.session_summary(first).await.unwrap();
    assert!(!stored.bypass);
    assert_eq!(stored.incarnation, None);
    assert_eq!(stored.bypass_seq, stored.current_seq);
}

// Given a session with bypass on that is stopped and resumed
// Then the resumed runtime starts off, and its history says so before
//      anything else the resume wrote
#[tokio::test]
async fn resume_resets_bypass_off_with_an_event_first() {
    use withnav::bus::runtime::bypass::BypassOrigin;
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state.create_session(&ws, None, false).await.unwrap();
    state
        .set_bypass(id, true, BypassOrigin::Http)
        .await
        .unwrap();
    state.close_session(id).await.unwrap();
    let stored = state.session_summary(id).await.unwrap();
    let (resumed, _) = state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .unwrap();
    assert_eq!(resumed, id);
    let summary = state.session_summary(id).await.unwrap();
    assert!(!summary.bypass);
    state.history().flush().await.unwrap();
    let sub = state.subscribe(id, None).await.unwrap();
    let withnav::bus::ResumeOutcome::Replay(events) = sub.replay else {
        panic!("expected replay");
    };
    let resets: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "session.bypass" && e.payload["by"] == "system:resume")
        .collect();
    assert_eq!(resets.len(), 1);
    assert_eq!(
        resets[0].payload,
        json!({"enabled":false,"by":"system:resume"})
    );
    assert_eq!(
        resets[0].seq,
        stored.current_seq + 1,
        "the first event of the resume"
    );
    assert_eq!(summary.bypass_seq, resets[0].seq);
    // The earlier ON stays in the history; replay never restores it.
    assert!(events
        .iter()
        .any(|e| e.kind == "session.bypass" && e.payload["enabled"] == true));
    state.close_session(id).await.unwrap();
}

async fn results(state: &AppState, id: ulid::Ulid) -> Vec<EventEnvelope> {
    state.history().flush().await.expect("flush");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        withnav::bus::ResumeOutcome::Replay(events) => events
            .into_iter()
            .filter(|e| e.kind == "command.result")
            .collect(),
        other => panic!("session {id}: expected command.result replay, got {other:?}"),
    }
}

// Given a live session
// When /help runs
// Then it lists every registered command and publishes a durable command.result
#[tokio::test]
async fn help_lists_commands_and_publishes_a_result() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
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
        ["help", "model", "new", "stop", "context", "variant", "compact", "agent", "bypass"]
    );
    assert_eq!(state.list_commands().len(), 9);

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
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let out = state.run_command(id, "/model", "").await.expect("run");
    assert!(out.ok, "{out:?}");
    let data = out.data.unwrap();
    assert_eq!(data["model"], "mock/mock-model");
    assert!(data.get("alias").is_none());
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
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");

    let out = state.run_command(id, "new", "").await.expect("run");
    assert!(out.ok, "{out:?}");
    let fresh: ulid::Ulid = out.data.as_ref().unwrap()["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(fresh, id);
    assert_eq!(
        state.session_summary(fresh).await.unwrap().model,
        "mock/mock-model"
    );

    let out = state.run_command(id, "new", "plan").await.expect("run");
    let other: ulid::Ulid = out.data.unwrap()["session_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        state.session_summary(other).await.unwrap().model,
        "mock/mock-model"
    );

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
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
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
// When the command is unknown
// Then the outcome is an error that lists close names
#[tokio::test]
async fn unknown_and_unbuilt_commands_answer_clear_errors() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");

    let out = state.run_command(id, "hlep", "").await.expect("run");
    assert!(!out.ok);
    assert!(out.error.unwrap().contains("/help"));

    let events = results(&state, id).await;
    assert_eq!(events.len(), 1, "failures are published too");
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
        .run_command(ulid::Ulid::generate(), "help", "")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        withnav::error::ServerError::SessionNotFound(_)
    ));
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    state.close_session(id).await.expect("close");
    state.history().flush().await.expect("flush");
    let err = state.run_command(id, "help", "").await.unwrap_err();
    assert!(matches!(err, withnav::error::ServerError::ResumeRequired));
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
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
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
    let bypass = state.run_command(id, "bypass", "on").await.unwrap();
    assert!(bypass.ok);
    assert_eq!(bypass.data.as_ref().unwrap()["enabled"], true);
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
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");

    let (status, body) = request_json(&router, "GET", "/v1/commands", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["commands"][0]["name"], "help");
    assert_eq!(body["commands"][2]["args_hint"], "[agent]");

    let path = format!("/v1/sessions/{id}/commands");
    let (status, body) =
        request_json(&router, "POST", &path, Some(&json!({"name": "model"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["model"], "mock/mock-model");
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
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("nothing to compact"),
        "{body}"
    );
    assert!(body.get("data").is_none());

    let (status, _) = request_json(&router, "POST", &path, Some(&json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{}/commands", ulid::Ulid::generate()),
        Some(&json!({"name": "help"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bypass_http_requires_configured_authentication() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let router = app(state.clone());
    let id = state.create_session(&ws, None, false).await.unwrap();
    for (suffix, body) in [
        ("bypass", json!({"enabled": true})),
        ("commands", json!({"name": "bypass", "arguments": "on"})),
        ("commands", json!({"name": " /bypass ", "arguments": "off"})),
    ] {
        let (status, body) = request_json(
            &router,
            "POST",
            &format!("/v1/sessions/{id}/{suffix}"),
            Some(&body),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "auth_required");
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("bypass can only be changed by an authenticated client"));
    }
    let (status, body) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/commands"),
        Some(&json!({"name": "bypass"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["enabled"], false);
}

#[tokio::test]
async fn bypass_http_is_strict_and_matches_slash_commands() {
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::unreachable(tmp.path())
        .bearer_token("test-token")
        .state();
    let router = app(state.clone());
    let id = state.create_session(&ws, None, false).await.unwrap();
    let path = format!("/v1/sessions/{id}/bypass");
    for (suffix, body) in [
        ("bypass", json!({"enabled": true})),
        ("commands", json!({"name": "bypass", "arguments": "on"})),
    ] {
        use tower::ServiceExt;
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/v1/sessions/{id}/{suffix}"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let router = router.layer(axum::middleware::from_fn(
        |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
            request
                .headers_mut()
                .insert("authorization", "Bearer test-token".parse().unwrap());
            next.run(request).await
        },
    ));
    for body in [
        json!({}),
        json!({"enabled": "on"}),
        json!({"enabled": null}),
        json!({"enabled": true, "by": "user:cli"}),
        json!({"enabled": false, "extra": true}),
    ] {
        let (status, _) = request_json(&router, "POST", &path, Some(&body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let (status, enabled) =
        request_json(&router, "POST", &path, Some(&json!({"enabled": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(enabled["enabled"], true);
    assert_eq!(enabled["changed"], true);
    let (status, off) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/commands"),
        Some(&json!({"name": "bypass", "arguments": "off"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(off["data"]["enabled"], false);
    let (_, enabled) = request_json(&router, "POST", &path, Some(&json!({"enabled": true}))).await;
    let report = state.run_command(id, "bypass", "").await.unwrap();
    assert!(report.ok);
    assert_eq!(
        report.data.as_ref().unwrap()["bypass_seq"],
        enabled["bypass_seq"]
    );
    assert_eq!(report.data.as_ref().unwrap()["changed"], false);
    let repeat = state.run_command(id, "bypass", "on").await.unwrap();
    assert_eq!(repeat.data, report.data);
    let off = state.run_command(id, "bypass", "off").await.unwrap();
    assert_eq!(off.data.as_ref().unwrap()["enabled"], false);
    assert_eq!(off.data.as_ref().unwrap()["changed"], true);
    for argument in ["toggle", "on off", "ON"] {
        let out = state.run_command(id, "bypass", argument).await.unwrap();
        assert!(!out.ok);
        assert_eq!(out.error.as_deref(), Some("usage: /bypass [on|off]"));
    }
}

// Given a session
// When a command is sent over HTTP as `stop`, `/stop`, `//stop`, ` /stop` or `STOP`
// Then only the first two (after one slash and trimming) stop it; the others
//      are unknown commands that stop nothing
#[tokio::test]
async fn command_names_are_normalised_once() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let router = app(state.clone());
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let path = format!("/v1/sessions/{id}/commands");

    for name in ["//stop", "STOP", "/ stop", "//", ""] {
        let (status, body) =
            request_json(&router, "POST", &path, Some(&json!({ "name": name }))).await;
        assert_eq!(status, StatusCode::OK, "{name:?}");
        assert_eq!(body["ok"], false, "{name:?}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("unknown command"),
            "{name:?}: {body}"
        );
    }
    assert_eq!(
        state.session_summary(id).await.unwrap().actor_state,
        "idle",
        "no unknown form stopped the session"
    );

    let (_, body) = request_json(&router, "POST", &path, Some(&json!({"name": " /stop "}))).await;
    assert_eq!(body["ok"], true);
    assert_eq!(
        state.session_summary(id).await.unwrap().actor_state,
        "stopped"
    );
    let events = results(&state, id).await;
    assert_eq!(events.last().unwrap().payload["name"], "stop");

    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let path = format!("/v1/sessions/{id}/commands");
    let (_, body) = request_json(&router, "POST", &path, Some(&json!({"name": "stop"}))).await;
    assert_eq!(body["ok"], true);
    assert_eq!(
        state.session_summary(id).await.unwrap().actor_state,
        "stopped"
    );
}

// Given a session
// When an argument-free command gets arguments
// Then it fails with a usage error and does nothing
#[tokio::test]
async fn arguments_to_argument_free_commands_are_rejected() {
    let (tmp, ws) = tmp_ws();
    let state = state(tmp.path());
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");

    let out = state
        .run_command(id, "stop", "anything")
        .await
        .expect("run");
    assert!(!out.ok);
    assert_eq!(out.error.unwrap(), "usage: /stop");
    let out = state.run_command(id, "help", "x").await.expect("run");
    assert_eq!(out.error.unwrap(), "usage: /help");
    assert_eq!(state.session_summary(id).await.unwrap().actor_state, "idle");
    assert_eq!(results(&state, id).await.len(), 2, "failures are durable");
}

// Given aliases in models.json
// When /model runs
// Then it counts the chat aliases and lists their names, sorted
#[tokio::test]
async fn model_lists_a_bounded_alias_catalog() {
    let (tmp, ws) = tmp_ws();
    let state = TestServerConfig::unreachable(tmp.path())
        .chat_tools(128_000)
        .extra_alias("other", "other-model")
        .state();
    let id = state
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let out = state.run_command(id, "model", "").await.expect("run");
    let catalog = &out.data.unwrap()["catalog"];
    assert_eq!(catalog["count"], 2);
    assert_eq!(
        catalog["models"],
        json!(["mock/mock-model", "mock/other-model"])
    );
    assert!(catalog["more"].as_str().unwrap().contains("/v1/models"));
}

// Given a session saved while the agent waited on an `ask`
// When it is resumed and a command arrives at once
// Then the command is refused: the status knew of the ask from the start
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_pending_ask_refuses_an_immediate_command() {
    let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"which scope?\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[ask]))]).await;
    let (tmp, ws) = tmp_ws();
    let config = TestServerConfig::mock(tmp.path(), &provider.url);
    let first = config.state();
    let (addr, task) = spawn_server(first.clone()).await;
    let id = first
        .create_session(&ws, None, false)
        .await
        .expect("create");
    let sub = first.subscribe(id, None).await.expect("sub");
    post_message_ok(&format!("http://{addr}"), &id.to_string(), "go").await;
    collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;
    // Closing joins the runtime and flushes its final transcript snapshot.
    first.close_session(id).await.expect("close");
    let loaded = first.history().load(id).await.expect("load").expect("row");
    assert!(!loaded.transcript.is_empty(), "transcript snapshot stored");
    task.abort();

    let second = config.state();
    let (resumed, _) = second
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("resume");
    let out = second.run_command(resumed, "help", "").await.expect("run");
    assert!(!out.ok, "{out:?}");
    assert!(out.error.unwrap().contains("waiting for an answer"));
}

// Given an oversized data payload
// When it is bounded for command.result
// Then a marker replaces it
#[test]
fn oversized_result_data_is_replaced_by_a_marker() {
    let big = json!({"k": "x".repeat(20_000)});
    let bounded = withnav::bus::event::CommandResult::bound_data(big);
    assert_eq!(bounded["truncated"], true);
}

fn run_withnav(
    root: &std::path::Path,
    models: &std::path::Path,
    ws: &std::path::Path,
    prompt: &str,
) -> std::process::Output {
    std::fs::create_dir_all(root.join("config/withnav")).unwrap();
    std::fs::write(
        root.join("config/withnav/config.json"),
        r#"{"agents":{"build":{"model":"mock/mock-model"}}}"#,
    )
    .unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_nav"))
        .args(["run", "--workspace"])
        .arg(ws)
        .arg("--models")
        .arg(models)
        .args(["--prompt", prompt])
        .env("WITHNAV_TEST_KEY", "test-key")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .output()
        .expect("run the binary")
}

// Given `nav run --prompt "/help"`
// Then it prints the command list and exits 0 without a provider call; an
//      unknown command prints an error and exits 1
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withnav_run_executes_a_command() {
    let (tmp, ws) = tmp_ws();
    let cfg = TestServerConfig::unreachable(tmp.path());
    cfg.config();
    let (root, models) = (tmp.path().to_path_buf(), cfg.models_path());
    let (ok, bad) = tokio::task::spawn_blocking(move || {
        (
            run_withnav(&root, &models, &ws, "/help"),
            run_withnav(&root, &models, &ws, "/nope"),
        )
    })
    .await
    .expect("join");
    assert_eq!(ok.status.code(), Some(0), "{ok:?}");
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(
        stdout.contains("/help") && stdout.contains("/new [agent]"),
        "{stdout}"
    );
    assert_eq!(bad.status.code(), Some(1), "{bad:?}");
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown command /nope"));
}

// Given `nav run --prompt "//text"`
// Then the agent receives the literal message "/text"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withnav_run_double_slash_sends_a_literal_message() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let provider = spawn_capturing_provider(vec![http_sse(&sse_body(&[done]))]).await;
    let (tmp, ws) = tmp_ws();
    let cfg = TestServerConfig::mock(tmp.path(), &provider.url);
    cfg.config();
    let (root, models) = (tmp.path().to_path_buf(), cfg.models_path());
    let out = tokio::task::spawn_blocking(move || run_withnav(&root, &models, &ws, "//text"))
        .await
        .expect("join");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let body: Value = serde_json::from_str(&provider.bodies()[0]).unwrap();
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"], "/text");
}
