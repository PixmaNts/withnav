//! The permission gate through the session service and the HTTP surface: a
//! gated tool call yields the turn, the driver answers with a typed decision,
//! and the decision (never chat text) resumes it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{json, Value};

use navai::bus::command::{Command, ContentBlock, DeliveryMode};
use navai::bus::event::EventEnvelope;
use navai::server::{app, AppState};

mod common;
use common::server::{request_json, TestServerConfig};
use common::sse::{http_sse, spawn_capturing_provider, sse_body, text_chunk, CapturingProvider};

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

/// Every stored event of `kind`, in order.
async fn events(state: &AppState, id: ulid::Ulid, kind: &str) -> Vec<EventEnvelope> {
    state.history().flush().await.expect("flush");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    match sub.replay {
        navai::bus::ResumeOutcome::Replay(all) => {
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
    let id = state.create_session(&env.ws, None, None).await.unwrap();

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
        let id = state.create_session(&env.ws, None, None).await.unwrap();
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
    let id = state.create_session(&env.ws, None, None).await.unwrap();

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

    // w2 runs on the grant; w3 is in another folder and asks.
    let requested = wait_events(&state, id, "permission.requested", 2).await;
    assert_eq!(requested.len(), 2, "exactly one more request");
    assert_eq!(requested[1].payload["tool_call_id"], "w3");
    assert!(first.exists() && nested.exists(), "w1 and w2 ran");
    assert!(!other.exists());

    let (status, _) = answer(
        &router,
        id,
        &request_id(&requested[1]),
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
    assert_eq!(resolved[0].payload["resolution"], "allow_folder");
    assert_eq!(resolved[1].payload["resolution"], "deny");
    assert_eq!(resolved[1].payload["message"], "keep out of other/");
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
    let id = state.create_session(&env.ws, None, None).await.unwrap();

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
    let id = state.create_session(&env.ws, None, None).await.unwrap();

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
    let id = state.create_session(&env.ws, None, None).await.unwrap();

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
    let id = first.create_session(&env.ws, None, None).await.unwrap();
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
        .create_or_resume_session(&env.ws, None, None, Some(id))
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
    std::process::Command::new(env!("CARGO_BIN_EXE_navai"))
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
//      approving; with --yes it approves once and finishes
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
