//! The permission gate through the session service and the HTTP surface: a
//! gated tool call yields the turn, the driver answers with a typed decision,
//! and the decision (never chat text) resumes it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{json, Value};

use withnav::bus::command::{Command, ContentBlock, DeliveryMode};
use withnav::bus::event::EventEnvelope;
use withnav::server::{app, AppState};

mod common;
use common::server::{request_json, TestServerConfig};
use common::sse::{
    http_sse, spawn_capturing_provider, spawn_holding_provider, spawn_streaming_provider, sse_body,
    text_chunk, tool_call_chunk, CapturingProvider,
};

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    ws: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonical");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    Env {
        _tmp: tmp,
        root,
        ws,
    }
}

fn config(env: &Env, provider: &CapturingProvider) -> TestServerConfig {
    TestServerConfig::mock(&env.root, &provider.url).rules_home(&env.root.join("home"))
}

/// One assistant message calling each `(id, tool, arguments)`.
fn calls(calls: &[(&str, &str, Value)]) -> Vec<u8> {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({"index": index, "id": id,
                   "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    let chunk = json!({"choices": [{"index": 0, "finish_reason": "tool_calls",
                                    "delta": {"tool_calls": calls}}]})
    .to_string();
    http_sse(&sse_body(&[&chunk]))
}

fn write_call(id: &str, path: &Path) -> (String, Value) {
    (
        id.to_string(),
        json!({"path": path, "content": format!("from {id}")}),
    )
}

fn one_write(id: &str, path: &Path) -> Vec<u8> {
    let (id, args) = write_call(id, path);
    calls(&[(&id, "write", args)])
}

fn done(text: &str) -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk(text)]))
}

async fn provider(responses: Vec<Vec<u8>>) -> CapturingProvider {
    spawn_capturing_provider(responses).await
}

async fn say(state: &AppState, id: ulid::Ulid, text: &str) {
    state
        .send(Command {
            command_id: ulid::Ulid::new(),
            session_id: id,
            source_actor_id: "user:test".to_string(),
            target_actor_id: "agent:main".to_string(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            created_at: std::time::SystemTime::now(),
        })
        .await
        .expect("send");
}

/// Posts `text` into the session as the navigator would (an in-process wake).
async fn wake(state: &AppState, id: ulid::Ulid, text: &str) {
    state
        .send(Command {
            command_id: ulid::Ulid::new(),
            session_id: id,
            source_actor_id: withnav::navigator::NAVIGATOR_ACTOR_ID.to_string(),
            target_actor_id: "agent:main".to_string(),
            mode: DeliveryMode::NextTurn,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            created_at: std::time::SystemTime::now(),
        })
        .await
        .expect("wake");
}

/// Every stored event of `kind`, in order.
async fn events(state: &AppState, id: ulid::Ulid, kind: &str) -> Vec<EventEnvelope> {
    state.history().flush().await.expect("flush");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        withnav::bus::ResumeOutcome::Replay(all) => {
            all.into_iter().filter(|e| e.kind == kind).collect()
        }
        other => panic!("expected replay, got {other:?}"),
    }
}

/// Waits until at least `count` events of `kind` exist; returns them all.
async fn wait_events(
    state: &AppState,
    id: ulid::Ulid,
    kind: &str,
    count: usize,
) -> Vec<EventEnvelope> {
    for _ in 0..1000 {
        let found = events(state, id, kind).await;
        if found.len() >= count {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {count} `{kind}` event(s)");
}

async fn answer(
    router: &axum::Router,
    id: ulid::Ulid,
    request: &str,
    body: Value,
) -> (StatusCode, Value) {
    request_json(
        router,
        "POST",
        &format!("/v1/sessions/{id}/permissions/{request}"),
        Some(&body),
    )
    .await
}

fn request_id(event: &EventEnvelope) -> String {
    event.payload["request_id"].as_str().unwrap().to_string()
}

fn body_of(provider: &CapturingProvider, n: usize) -> String {
    provider.bodies()[n].clone()
}

// Given a write, when the driver allows it once
// Then the call runs once, its real result is what the next model request
//      carries, and the request is stored with its resolution
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_once_runs_the_call_and_the_model_sees_its_result() {
    let env = env();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target), done("written")]).await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write a.txt").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    let payload = &requested[0].payload;
    assert_eq!(payload["tool"], "write");
    assert_eq!(payload["tool_call_id"], "w1");
    assert_eq!(payload["agent"], "build");
    assert_eq!(payload["scope"], json!({"kind": "folder", "value": env.ws}));
    assert!(payload["arguments"].as_str().unwrap().len() <= 4096);
    assert!(!target.exists(), "the call has not run");
    wait_events(&state, id, "turn.end", 1).await;

    // Commands that change the session are refused while a request waits.
    let (_, refused) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/commands"),
        Some(&json!({"name": "agent", "arguments": "plan"})),
    )
    .await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("approval"));

    let request = request_id(&requested[0]);
    let (status, body) = answer(&router, id, &request, json!({"decision": "allow_once"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], true);

    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "from w1");
    let next = body_of(&provider, 1);
    assert!(next.contains("\"tool_call_id\":\"w1\""), "{next}");
    assert!(next.contains("wrote "), "the real result: {next}");

    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["request_id"], request.as_str());
    assert_eq!(resolved[0].payload["resolution"], "allow_once");
    // Stored: a stopped conversation replays both events.
    state.close_session(id).await.unwrap();
    assert_eq!(events(&state, id, "permission.requested").await.len(), 1);
    assert_eq!(events(&state, id, "permission.resolved").await.len(), 1);

    // A duplicate and a request that never existed are stale; an unknown
    // session is 404.
    let (stale, body) = answer(&router, id, &request, json!({"decision": "allow_once"})).await;
    assert_ne!(stale, StatusCode::OK, "{body}");
    let live = {
        let provider = self::provider(vec![one_write("w9", &target), done("x")]).await;
        let state = config(&env, &provider).state();
        let id = state.create_session(&env.ws, None, false).await.unwrap();
        say(&state, id, "again").await;
        let requested = wait_events(&state, id, "permission.requested", 1).await;
        (app(state), id, request_id(&requested[0]))
    };
    let (router2, id2, request2) = live;
    let (status, body) = answer(&router2, id2, &request2, json!({"decision": "allow_once"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = answer(&router2, id2, &request2, json!({"decision": "allow_once"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate: {body}");
    assert_eq!(body["reason"], "permission_stale");
    let (status, _) = answer(
        &router2,
        id2,
        &ulid::Ulid::new().to_string(),
        json!({"decision": "allow_once"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "unknown request id");
    let (status, _) = answer(
        &router2,
        ulid::Ulid::new(),
        &request2,
        json!({"decision": "allow_once"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown session");
    let (status, _) = answer(&router2, id2, &request2, json!({"decision": "maybe"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "not a decision");
}

fn authenticated_router(state: withnav::server::AppState) -> axum::Router {
    app(state).layer(axum::middleware::from_fn(
        |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
            request
                .headers_mut()
                .insert("authorization", "Bearer test-token".parse().unwrap());
            next.run(request).await
        },
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypass_resolves_pending_permission_once_with_bypass_provenance() {
    let env = env();
    let target = env.ws.join("bypass.txt");
    let provider = provider(vec![one_write("bypass-write", &target), done("written")]).await;
    let state = config(&env, &provider).bearer_token("test-token").state();
    let router = authenticated_router(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, id, "write the file").await;
    wait_events(&state, id, "permission.requested", 1).await;
    for _ in 0..2 {
        let (status, body) = request_json(
            &router,
            "POST",
            &format!("/v1/sessions/{id}/bypass"),
            Some(&json!({"enabled":true})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "from bypass-write"
    );
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["approved_by"], "bypass");
    let posts = events(&state, id, "tool.post_call").await;
    assert_eq!(posts.len(), 1);
    assert!(serde_json::to_string(&posts[0].payload)
        .unwrap()
        .contains("bypass"));
    state.close_session(id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypass_leaves_pending_sensitive_request_open() {
    let env = env();
    let target = env.ws.join("auth.json");
    let provider = provider(vec![one_write("protected-write", &target)]).await;
    let state = config(&env, &provider).bearer_token("test-token").state();
    let router = authenticated_router(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, id, "write credentials").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    assert_eq!(requested[0].payload["requires_driver"], true);
    let (status, body) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/bypass"),
        Some(&json!({"enabled":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(events(&state, id, "permission.resolved").await.is_empty());
    assert!(!target.exists());
    state.close_session(id).await.unwrap();
    assert!(!target.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_driver_answer_keeps_driver_provenance_after_bypass() {
    let env = env();
    let target = env.ws.join("driver.txt");
    let provider = provider(vec![one_write("driver-write", &target), done("written")]).await;
    let state = config(&env, &provider).bearer_token("test-token").state();
    let router = authenticated_router(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, id, "write the file").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    let (status, body) = answer(
        &router,
        id,
        &request_id(&requested[0]),
        json!({"decision":"allow_once"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/bypass"),
        Some(&json!({"enabled":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_events(&state, id, "task.done", 1).await;
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert!(resolved[0].payload.get("approved_by").is_none());
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "from driver-write"
    );
    state.close_session(id).await.unwrap();
}

// Given allow-folder on one folder
// Then a write below it runs without a request, a write in another folder
//      asks, and a denial with a message reaches the model as
//      `permission_denied`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allow_folder_covers_descendants_and_deny_reaches_the_model() {
    let env = env();
    let (first, nested, other) = (
        env.ws.join("src/a.txt"),
        env.ws.join("src/deep/b.txt"),
        env.ws.join("other/c.txt"),
    );
    let provider = provider(vec![
        one_write("w1", &first),
        one_write("w2", &nested),
        one_write("w3", &other),
        done("understood"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write three files").await;
    let first_request = wait_events(&state, id, "permission.requested", 1).await;
    let (status, _) = answer(
        &router,
        id,
        &request_id(&first_request[0]),
        json!({"decision": "allow_folder"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The old folder answer is one approval; even the descendant asks again.
    let requested = wait_events(&state, id, "permission.requested", 2).await;
    assert_eq!(requested.len(), 2, "exactly one more request");
    assert_eq!(requested[1].payload["tool_call_id"], "w2");
    assert!(first.exists() && !nested.exists(), "only w1 ran");
    assert!(!other.exists());

    answer(
        &router,
        id,
        &request_id(&requested[1]),
        json!({"decision": "allow_once"}),
    )
    .await;
    let requested = wait_events(&state, id, "permission.requested", 3).await;
    assert_eq!(requested[2].payload["tool_call_id"], "w3");
    assert!(nested.exists());

    let (status, _) = answer(
        &router,
        id,
        &request_id(&requested[2]),
        json!({"decision": "deny", "message": "keep out of other/"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_events(&state, id, "task.done", 1).await;
    assert!(!other.exists(), "a denied call never runs");
    let last = body_of(&provider, 3);
    assert!(last.contains("permission_denied"), "{last}");
    assert!(last.contains("keep out of other/"), "{last}");

    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved[0].payload["resolution"], "allow_once");
    assert_eq!(resolved[2].payload["resolution"], "deny");
    assert_eq!(resolved[2].payload["message"], "keep out of other/");
}

// Given two writes in one assistant message
// When the first is denied
// Then the second is still processed (it asks on its own)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn siblings_after_a_denial_are_processed() {
    let env = env();
    let (a, b) = (env.ws.join("a.txt"), env.ws.join("b.txt"));
    let (id_a, args_a) = write_call("w1", &a);
    let (id_b, args_b) = write_call("w2", &b);
    let provider = provider(vec![
        calls(&[(&id_a, "write", args_a), (&id_b, "write", args_b)]),
        done("ok"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "go").await;
    let first = wait_events(&state, id, "permission.requested", 1).await;
    answer(
        &router,
        id,
        &request_id(&first[0]),
        json!({"decision": "deny"}),
    )
    .await;
    let both = wait_events(&state, id, "permission.requested", 2).await;
    assert_eq!(both[1].payload["tool_call_id"], "w2");
    answer(
        &router,
        id,
        &request_id(&both[1]),
        json!({"decision": "allow_once"}),
    )
    .await;
    wait_events(&state, id, "task.done", 1).await;
    assert!(!a.exists());
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "from w2");
    let last = body_of(&provider, 1);
    assert!(last.contains("\"tool_call_id\":\"w1\"") && last.contains("\"tool_call_id\":\"w2\""));
    assert!(last.contains("permission_denied"));
}

// Given a pending request
// When chat text arrives
// Then it never approves: the request is cancelled, nothing runs, and the
//      text is an ordinary message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_text_never_approves() {
    let env = env();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target), done("fine")]).await;
    let state = config(&env, &provider).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write it").await;
    wait_events(&state, id, "permission.requested", 1).await;
    say(&state, id, "yes, allow it").await;
    let resolved = wait_events(&state, id, "permission.resolved", 1).await;
    assert_eq!(resolved[0].payload["resolution"], "cancelled");
    wait_events(&state, id, "task.done", 1).await;
    assert!(!target.exists());
    let next = body_of(&provider, 1);
    assert!(next.contains("yes, allow it"));
}

// Given a pending request
// When the session is stopped
// Then the held call never runs and permission.resolved says cancelled
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_while_a_request_is_pending_cancels_it() {
    let env = env();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target)]).await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write it").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    wait_events(&state, id, "turn.end", 1).await;
    state.close_session(id).await.unwrap();

    assert!(!target.exists());
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["resolution"], "cancelled");
    assert_eq!(
        resolved[0].payload["request_id"],
        requested[0].payload["request_id"]
    );
    let (status, _) = answer(
        &router,
        id,
        &request_id(&requested[0]),
        json!({"decision": "allow_once"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a stopped session needs a resume"
    );
}

// Given a request that was pending when the process ended
// When the conversation is resumed
// Then the request expires, its call is settled as interrupted and never
//      replayed, and the next model request carries the settled result
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_expires_the_pending_request_without_replaying_it() {
    let env = env();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target), done("after restart")]).await;
    let first = config(&env, &provider).state();
    let id = first.create_session(&env.ws, None, false).await.unwrap();
    say(&first, id, "write it").await;
    let requested = wait_events(&first, id, "permission.requested", 1).await;
    wait_events(&first, id, "turn.end", 1).await;
    // The transcript snapshot of the yielded turn is stored.
    for _ in 0..100 {
        let stored = first.history().load(id).await.unwrap().unwrap();
        if !stored.transcript.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The process "dies": no stop, no resolution.
    let second = config(&env, &provider).state();
    let (resumed, _) = second
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    assert_eq!(resumed, id);

    let expired = wait_events(&second, id, "permission.resolved", 1).await;
    assert_eq!(expired[0].payload["resolution"], "expired");
    assert_eq!(
        expired[0].payload["request_id"],
        requested[0].payload["request_id"]
    );
    let router = app(second.clone());
    let (status, _) = answer(
        &router,
        id,
        &request_id(&requested[0]),
        json!({"decision": "allow_once"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "an expired request is stale");

    say(&second, id, "carry on").await;
    wait_events(&second, id, "task.done", 1).await;
    assert!(!target.exists(), "nothing is replayed");
    let next = body_of(&provider, 1);
    assert!(next.contains("\"tool_call_id\":\"w1\""), "{next}");
    assert!(next.contains("carry on"));
}

fn run_navai(env: &Env, models: &Path, extra: &[&str]) -> std::process::Output {
    std::fs::create_dir_all(env.root.join("config/navai")).unwrap();
    std::fs::write(
        env.root.join("config/navai/config.json"),
        r#"{"agents":{"build":{"model":"mock/mock-model"}}}"#,
    )
    .unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_nav"))
        .args(["run", "--workspace"])
        .arg(&env.ws)
        .arg("--models")
        .arg(models)
        .args(extra)
        .env("NAVAI_TEST_KEY", "test-key")
        .env("HOME", env.root.join("home"))
        .env("XDG_DATA_HOME", env.root.join("data"))
        .env("XDG_CONFIG_HOME", env.root.join("config"))
        .output()
        .expect("run the binary")
}

// Given `navai run` reaching a gated call
// Then it stops with `permission_required` and exit 3, never hanging or
//      approving; with --yes the session's bypass lets ordinary work finish
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navai_run_stops_on_a_permission_request_unless_yes() {
    let env = env();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![
        one_write("w1", &target),
        one_write("w2", &target),
        done("done"),
    ])
    .await;
    let cfg = config(&env, &provider);
    cfg.config();
    let models = cfg.models_path();

    let (root_env, models_a) = (env.root.clone(), models.clone());
    let ws = env.ws.clone();
    let stopped = tokio::task::spawn_blocking(move || {
        let env = Env {
            _tmp: tempfile::tempdir().unwrap(),
            root: root_env,
            ws,
        };
        run_navai(&env, &models_a, &["--prompt", "write a.txt"])
    })
    .await
    .unwrap();
    assert_eq!(stopped.status.code(), Some(3), "{stopped:?}");
    let stderr = String::from_utf8_lossy(&stopped.stderr);
    assert!(
        stderr.contains(&format!("permission_required: write {}", env.ws.display())),
        "{stderr}"
    );
    assert!(!target.exists());

    let (root_env, ws) = (env.root.clone(), env.ws.clone());
    let approved = tokio::task::spawn_blocking(move || {
        let env = Env {
            _tmp: tempfile::tempdir().unwrap(),
            root: root_env,
            ws,
        };
        run_navai(&env, &models, &["--yes", "--prompt", "write a.txt"])
    })
    .await
    .unwrap();
    assert_eq!(approved.status.code(), Some(0), "{approved:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "from w2");
}

// Given `navai run --yes` whose model writes a credentials file
// Then the hard floor holds: it prints the request with its reason, exits 3
//      and writes nothing
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navai_run_yes_stops_at_the_hard_floor_without_side_effects() {
    let env = env();
    let target = env.ws.join("auth.json");
    let provider = provider(vec![one_write("w1", &target), done("never")]).await;
    let cfg = config(&env, &provider);
    cfg.config();
    let models = cfg.models_path();
    let (root_env, ws) = (env.root.clone(), env.ws.clone());
    let output = tokio::task::spawn_blocking(move || {
        let env = Env {
            _tmp: tempfile::tempdir().unwrap(),
            root: root_env,
            ws,
        };
        run_navai(&env, &models, &["--yes", "--prompt", "write auth.json"])
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("permission_required: write "), "{stderr}");
    assert!(
        stderr.contains(&format!("{}: ", env.ws.display())),
        "{stderr}"
    );
    assert!(!target.exists());
    assert_eq!(
        provider.requests().len(),
        1,
        "the run stopped at the request"
    );
}

// Given `navai run --yes` where a child session reaches the hard floor
// Then the mirrored request ends the whole run with exit 3, and the child
//      writes nothing
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navai_run_yes_stops_when_a_child_reaches_the_hard_floor() {
    let env = env();
    let agents = env.ws.join(".withnav/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite the requested change.",
    )
    .unwrap();
    let target = env.ws.join("auth.json");
    let child_target = target.clone();
    // The parent's second answer is held open, so the child's request is what
    // ends the run.
    let provider = spawn_holding_provider(
        3,
        2,
        std::sync::Arc::new(tokio::sync::Notify::new()),
        std::sync::Arc::new(tokio::sync::Notify::new()),
        move |_, request| {
            if request.contains("Write the requested change.") {
                one_write("child-write", &child_target)
            } else {
                http_sse(&sse_body(&[&tool_call_chunk(
                    "spawn",
                    "task",
                    &json!({"agent":"writer","prompt":"write auth.json"}),
                )]))
            }
        },
    )
    .await;
    let cfg = config(&env, &provider);
    cfg.config();
    let models = cfg.models_path();
    let started = std::time::Instant::now();
    let (root_env, ws) = (env.root.clone(), env.ws.clone());
    let output = tokio::task::spawn_blocking(move || {
        let env = Env {
            _tmp: tempfile::tempdir().unwrap(),
            root: root_env,
            ws,
        };
        run_navai(&env, &models, &["--yes", "--prompt", "delegate"])
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "ended promptly");
    assert!(!target.exists());
}

// Given a decision already taken in while the resumed turn stalls
// When a second decision arrives for the same request
// Then it is refused at once, not after the turn
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_answer_is_refused_at_once_while_the_resumed_turn_runs() {
    let env = env();
    let target = env.ws.join("a.txt");
    let mut provider = spawn_streaming_provider().await;
    let state = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.root.join("home"))
        .state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write it").await;
    let (_, first) = provider.requests.recv().await.expect("first request");
    first
        .send(tool_call_chunk(
            "w1",
            "write",
            &json!({"path": target, "content": "x"}),
        ))
        .unwrap();
    drop(first);
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    let request = request_id(&requested[0]);

    let (status, body) = answer(&router, id, &request, json!({"decision": "allow_once"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The continuation asks the model and stalls there.
    let (_, stalled) = provider.requests.recv().await.expect("continuation");

    let second = tokio::time::timeout(
        Duration::from_secs(2),
        answer(&router, id, &request, json!({"decision": "deny"})),
    )
    .await
    .expect("the duplicate must not wait for the resumed turn");
    assert_eq!(second.0, StatusCode::CONFLICT, "{}", second.1);
    assert_eq!(second.1["reason"], "permission_stale");

    stalled.send(text_chunk("done")).unwrap();
    drop(stalled);
    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
}

// Given a pending request
// When `/stop` is sent as a command
// Then it is accepted, the request ends as cancelled and nothing runs
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stop_command_is_accepted_while_a_request_is_pending() {
    let env = env();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target)]).await;
    let state = config(&env, &provider).state();
    let router = app(state.clone());
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write it").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    wait_events(&state, id, "turn.end", 1).await;

    let (_, stop) = request_json(
        &router,
        "POST",
        &format!("/v1/sessions/{id}/commands"),
        Some(&json!({"name": "stop", "arguments": ""})),
    )
    .await;
    assert_eq!(stop["ok"], true, "{stop}");

    assert!(!target.exists());
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["resolution"], "cancelled");
    assert_eq!(
        resolved[0].payload["request_id"],
        requested[0].payload["request_id"]
    );
}

// Given a permission request that waits for a decision
// When /compact runs, now or accepted behind the turn that raised the request
// Then it is refused, nothing is summarized, and a deferred one is refused
//      under the operation id it was accepted with
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_while_a_permission_request_is_open_is_refused() {
    let env = env();
    let target = env.ws.join("a.txt");
    let provider = provider(vec![one_write("w1", &target), done("written")]).await;
    let state = config(&env, &provider).chat_tools(20_000).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write a.txt").await;
    wait_events(&state, id, "permission.requested", 1).await;
    wait_events(&state, id, "turn.end", 1).await;

    let out = state.run_command(id, "compact", "").await.unwrap();
    assert!(!out.ok, "{out:?}");
    let error = out.error.unwrap();
    assert!(
        error.contains("permission request"),
        "the reason names the pending request: {error}"
    );
    assert_eq!(provider.bodies().len(), 1, "no summary request");
    assert!(events(&state, id, "context.compacted").await.is_empty());
    assert!(!target.exists(), "the held call did not run");
}

// Given a /compact accepted while a turn runs
// When the turn ends on a permission request
// Then the deferred run is refused under its own operation id
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deferred_compact_is_refused_when_the_turn_ends_on_a_permission_request() {
    let env = env();
    let target = env.ws.join("a.txt");
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let provider = common::sse::spawn_gated_provider(
        vec![one_write("w1", &target), done("written")],
        release.clone(),
    )
    .await;
    let state = config(&env, &provider).chat_tools(20_000).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write a.txt").await;
    common::server::wait_for("the turn's request", || provider.bodies().len() == 1).await;
    let accepted = state.run_command(id, "compact", "").await.unwrap();
    assert_eq!(accepted.data.as_ref().unwrap()["accepted"], true);
    release.notify_one();
    wait_events(&state, id, "permission.requested", 1).await;

    let results = wait_events(&state, id, "command.result", 2).await;
    let refusal = results
        .iter()
        .find(|e| e.payload["ok"] == false)
        .expect("a refusal");
    assert_eq!(
        refusal.payload["operation_id"],
        accepted.operation_id.to_string()
    );
    assert!(refusal.payload["error"]
        .as_str()
        .unwrap()
        .contains("permission request"));
    assert_eq!(provider.bodies().len(), 1, "no summary request");
    assert!(events(&state, id, "context.compacted").await.is_empty());
}

/// Waits for request number `n` (1-based) and returns its id.
async fn nth_request(state: &AppState, id: ulid::Ulid, n: usize) -> ulid::Ulid {
    let found = wait_events(state, id, "permission.requested", n).await;
    request_id(&found[n - 1]).parse().unwrap()
}

async fn allow(state: &AppState, id: ulid::Ulid, request: ulid::Ulid) {
    state
        .answer_permission(
            id,
            request,
            withnav::agent::permissions::PermissionDecision::AllowOnce,
        )
        .await
        .expect("answer");
}

// Given a model message with two writes, and a navigator wake posted while
//       the first request waits
// When the driver approves the first, the second raises its own request, and
//      the driver approves that too
// Then no request is cancelled, both files are written, and the wake runs
//      as its own turn only after the turn that held the requests has ended
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_held_behind_a_request_does_not_cancel_the_next_request() {
    let env = env();
    let (a, b) = (env.ws.join("a.txt"), env.ws.join("b.txt"));
    let (id_a, args_a) = write_call("w1", &a);
    let (id_b, args_b) = write_call("w2", &b);
    let provider = provider(vec![
        calls(&[(&id_a, "write", args_a), (&id_b, "write", args_b)]),
        done("both written"),
        done("wake seen"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write both").await;
    let first = nth_request(&state, id, 1).await;
    wake(&state, id, "drift: look at the parser").await;
    // The wake is accepted (durable) while the request waits...
    wait_events(&state, id, "message.received", 2).await;
    assert!(events(&state, id, "permission.resolved").await.is_empty());

    allow(&state, id, first).await;
    let second = nth_request(&state, id, 2).await;
    assert_ne!(first, second);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        events(&state, id, "permission.resolved").await.len(),
        1,
        "the second request is still open"
    );
    assert_eq!(provider.bodies().len(), 1, "the wake did not start a turn");

    allow(&state, id, second).await;
    wait_events(&state, id, "task.done", 2).await;
    let resolved = events(&state, id, "permission.resolved").await;
    assert_eq!(resolved.len(), 2);
    assert!(resolved
        .iter()
        .all(|e| e.payload["resolution"] == "allow_once"));
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "from w1");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "from w2");
    let bodies = provider.bodies();
    assert_eq!(bodies.len(), 3);
    assert!(!bodies[1].contains("drift: look at the parser"));
    assert!(
        bodies[2].contains("drift: look at the parser"),
        "{}",
        bodies[2]
    );
}

// Given a write whose destination moves while its approval waits, and a wake
//       posted behind the request
// When the driver approves the first request and the call asks again
// Then the fresh request is not cancelled by the held wake, and the wake runs
//      after the turn
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_held_behind_a_request_does_not_cancel_a_revalidated_request() {
    let env = env();
    let reviewed = env.ws.join("reviewed");
    let moved = env.ws.join("moved");
    std::fs::create_dir_all(&reviewed).unwrap();
    std::fs::create_dir_all(&moved).unwrap();
    let link = env.ws.join("link");
    std::os::unix::fs::symlink(&reviewed, &link).unwrap();
    let provider = provider(vec![
        one_write("w1", &link.join("f.txt")),
        done("written"),
        done("wake seen"),
    ])
    .await;
    let state = config(&env, &provider).state();
    let id = state.create_session(&env.ws, None, false).await.unwrap();

    say(&state, id, "write through the link").await;
    let first = nth_request(&state, id, 1).await;
    wake(&state, id, "drift: look at the parser").await;
    wait_events(&state, id, "message.received", 2).await;

    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&moved, &link).unwrap();
    allow(&state, id, first).await;
    let second = nth_request(&state, id, 2).await;
    assert_ne!(first, second, "a moved destination asks again");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(events(&state, id, "permission.resolved").await.len(), 1);
    assert_eq!(provider.bodies().len(), 1, "the wake did not start a turn");

    allow(&state, id, second).await;
    wait_events(&state, id, "task.done", 2).await;
    let resolved = events(&state, id, "permission.resolved").await;
    assert!(resolved
        .iter()
        .all(|e| e.payload["resolution"] == "allow_once"));
    assert_eq!(
        std::fs::read_to_string(moved.join("f.txt")).unwrap(),
        "from w1"
    );
    assert!(!reviewed.join("f.txt").exists());
    assert!(provider.bodies()[2].contains("drift: look at the parser"));
}
