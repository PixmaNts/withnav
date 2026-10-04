//! The plan agent's role guard through the session service: Jev (a scripted
//! System One endpoint on the `instinct` alias) is asked about every exception,
//! and only a valid `conforms` lets a call run without the driver.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use navai::agent::permissions::PermissionDecision;
use navai::bus::command::{Command, ContentBlock, DeliveryMode};
use navai::bus::event::EventEnvelope;
use navai::server::AppState;

mod common;
use common::server::TestServerConfig;
use common::sse::{http_sse, spawn_responding_provider, sse_body, text_chunk, CapturingProvider};

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
    Env {
        _tmp: tmp,
        root,
        ws,
    }
}

fn write_call(path: &Path) -> Vec<u8> {
    let chunk = json!({"choices": [{"index": 0, "finish_reason": "tool_calls",
    "delta": {"tool_calls": [{"index": 0, "id": "w1", "function": {
        "name": "write",
        "arguments": json!({"path": path, "content": "x"}).to_string(),
    }}]}}]})
    .to_string();
    http_sse(&sse_body(&[&chunk]))
}

fn verdict(choice: &str) -> Vec<u8> {
    let mut value = json!({"answers": {"role_conformance": {
        "type": "choice",
        "choice": choice,
        "probabilities": {"conforms": 0.1, "violates": 0.1, "uncertain": 0.1},
        "confidence": 0.9,
    }}});
    for (name, choice) in [
        ("call_effect", "read_write"),
        ("call_scope", "inside"),
        ("call_risk", "low"),
    ] {
        value["answers"][name] =
            json!({"type":"choice", "choice":choice, "probabilities":{}, "confidence":1.0});
    }
    let body = value.to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A provider that answers chat requests from `chat` and System One requests
/// with `jev`, in order, by path.
async fn provider(chat: Vec<Vec<u8>>, jev: Vec<Vec<u8>>) -> CapturingProvider {
    let count = chat.len() + jev.len();
    let (chat_n, jev_n) = (
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
    );
    spawn_responding_provider(count, move |_, request| {
        let next = |n: &std::sync::atomic::AtomicUsize| {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        };
        if request.starts_with("POST /v1/systemone") || request.contains("/systemone ") {
            { let mut response = jev[next(&jev_n)].clone(); if !request.contains("role_conformance") { let text = String::from_utf8(response).unwrap(); let body = text.split("\r\n\r\n").nth(1).unwrap(); let mut value: Value = serde_json::from_str(body).unwrap(); value["answers"].as_object_mut().unwrap().remove("role_conformance"); let body = value.to_string(); response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes(); } response }
        } else {
            chat[next(&chat_n)].clone()
        }
    })
    .await
}

fn state(env: &Env, provider: &CapturingProvider, with_jev: bool) -> AppState {
    let mut config = TestServerConfig::mock(&env.root, &provider.url);
    if with_jev {
        config = config.extra_alias("instinct", "jev-test");
    }
    config.state()
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

async fn wait_events(
    state: &AppState,
    id: ulid::Ulid,
    kind: &str,
    count: usize,
) -> Vec<EventEnvelope> {
    for _ in 0..500 {
        let found = events(state, id, kind).await;
        if found.len() >= count {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {count} `{kind}` event(s)");
}

async fn wake(state: &AppState, id: ulid::Ulid, text: &str) {
    state
        .send(Command {
            command_id: ulid::Ulid::new(),
            session_id: id,
            source_actor_id: navai::navigator::NAVIGATOR_ACTOR_ID.to_string(),
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

/// The task Jev was sent in its `n`th question (0-based).
fn task_asked(provider: &CapturingProvider, n: usize) -> Value {
    let body: Value = serde_json::from_str(&system_one_requests(provider)[n]).unwrap();
    let state: Value = serde_json::from_str(body["state"].as_str().unwrap()).unwrap();
    state["untrusted_evidence"]["driver_task"].clone()
}

async fn plan_session(state: &AppState, env: &Env) -> ulid::Ulid {
    state
        .create_session_as(&env.ws, None, None, Some("plan"))
        .await
        .expect("create")
}

fn system_one_requests(provider: &CapturingProvider) -> Vec<String> {
    provider
        .bodies()
        .into_iter()
        .filter(|body| body.contains("role_conformance"))
        .collect()
}

// Given plan asked to write markdown
// Then no request and no question to Jev
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_writes_notes_md_without_a_request() {
    let env = env();
    let target = env.ws.join("notes.md");
    let provider = provider(vec![write_call(&target), done()], vec![verdict("conforms")]).await;
    let state = state(&env, &provider, true);
    let id = plan_session(&state, &env).await;
    say(&state, id, "write the notes").await;
    wait_events(&state, id, "task.done", 1).await;
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    assert!(events(&state, id, "permission.requested").await.is_empty());
    assert!(system_one_requests(&provider).is_empty());
}

fn done() -> Vec<u8> {
    http_sse(&sse_body(&[&text_chunk("done")]))
}

// Given plan writing code and Jev answering `conforms`
// Then the call runs without a request, Jev was asked the typed question with
//      the exact call, and the durable tool.post_call carries the verdict
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conforming_exception_runs_and_is_recorded() {
    let env = env();
    let target = env.ws.join("main.rs");
    let provider = provider(vec![write_call(&target), done()], vec![verdict("conforms")]).await;
    let state = state(&env, &provider, true);
    let id = plan_session(&state, &env).await;
    say(&state, id, "write the stub").await;
    wait_events(&state, id, "task.done", 1).await;

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    assert!(events(&state, id, "permission.requested").await.is_empty());
    let asked = system_one_requests(&provider);
    assert_eq!(asked.len(), 1);
    let body: Value = serde_json::from_str(&asked[0]).unwrap();
    assert_eq!(body["questions"]["role_conformance"]["type"], "choice");
    let options: Vec<_> = body["questions"]["role_conformance"]["criteria"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(options, ["conforms", "uncertain", "violates"]);
    let described: Value = serde_json::from_str(body["state"].as_str().unwrap()).unwrap();
    assert!(
        described["role_policy"]
            .as_str()
            .unwrap()
            .contains("plan mode"),
        "the role prompt"
    );
    let evidence = &described["untrusted_evidence"];
    assert_eq!(evidence["driver_task"], "write the stub");
    assert_eq!(
        described["host_facts"]["destination"],
        target.to_str().unwrap()
    );
    assert_eq!(
        evidence["arguments"],
        json!({"path": target, "content": "x"}),
        "exact arguments"
    );
    assert!(body["questions"]["role_conformance"]["instructions"]
        .as_str()
        .unwrap()
        .contains("untrusted"));

    let post = events(&state, id, "tool.post_call").await;
    assert_eq!(post.len(), 1);
    assert_eq!(
        post[0].payload["metadata"]["guard"],
        json!({"permission_gate": {"policy_version":1,"source":"jev"}})
    );
}

// Given plan writing code and Jev not conforming, or not available
// Then the driver is asked, with the reason; nothing runs; allow-folder is one
//      approval; approving runs the call
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn violates_uncertain_and_no_jev_all_ask_the_driver() {
    for (jev, with_jev, reason) in [
        (vec![verdict("violates")], true, "uncertain"),
        (vec![verdict("uncertain")], true, "uncertain"),
        (vec![verdict("perhaps")], true, "malformed"),
        (vec![], false, "Jev unavailable"),
    ] {
        let env = env();
        let target = env.ws.join("main.rs");
        let provider = provider(vec![write_call(&target), done()], jev).await;
        let state = state(&env, &provider, with_jev);
        let id = plan_session(&state, &env).await;
        say(&state, id, "write the stub").await;
        let requested = wait_events(&state, id, "permission.requested", 1).await;
        let payload = &requested[0].payload;
        assert!(
            payload["reason"].as_str().unwrap().contains(reason),
            "{reason}: {payload}"
        );
        assert_eq!(payload["agent"], "plan");
        assert!(!target.exists(), "{reason}: nothing ran");
        assert_eq!(
            system_one_requests(&provider).len(),
            usize::from(with_jev),
            "{reason}"
        );

        let request: ulid::Ulid = payload["request_id"].as_str().unwrap().parse().unwrap();
        state
            .answer_permission(id, request, PermissionDecision::AllowFolder)
            .await
            .unwrap();
        wait_events(&state, id, "task.done", 1).await;
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        let resolved = events(&state, id, "permission.resolved").await;
        assert_eq!(
            resolved[0].payload["resolution"], "allow_once",
            "{reason}: an exception of plan is never a folder grant"
        );
    }
}

// Given a build session
// Then Jev is never asked and the usual request comes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_is_not_guarded() {
    let env = env();
    let target = env.ws.join("main.rs");
    let provider = provider(vec![write_call(&target), done()], vec![]).await;
    let state = state(&env, &provider, false);
    let id = state.create_session(&env.ws, None, None).await.unwrap();
    say(&state, id, "write the stub").await;
    let requested = wait_events(&state, id, "permission.requested", 1).await;
    assert!(requested[0].payload["reason"]
        .as_str()
        .unwrap()
        .contains("Jev unavailable"));
    assert!(system_one_requests(&provider).is_empty());
}

// Given a plan conversation where the driver spoke, then stopped and resumed
//       with a navigator wake as the only new message
// Then Jev is sent the driver's stored words as the task, not the wake
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_session_judges_against_the_drivers_stored_words() {
    let env = env();
    let target = env.ws.join("main.rs");
    let provider = provider(
        vec![write_call(&target), write_call(&target)],
        vec![verdict("violates"), verdict("violates")],
    )
    .await;
    let first = state(&env, &provider, true);
    let id = plan_session(&first, &env).await;
    say(&first, id, "write the stub").await;
    wait_events(&first, id, "permission.requested", 1).await;
    first.close_session(id).await.unwrap();
    drop(first);

    let second = state(&env, &provider, true);
    second
        .create_or_resume_session(&env.ws, None, None, Some(id))
        .await
        .unwrap();
    wake(&second, id, "drift: look at the lexer").await;
    wait_events(&second, id, "permission.requested", 2).await;
    assert_eq!(task_asked(&provider, 0), "write the stub");
    assert_eq!(
        task_asked(&provider, 1),
        "write the stub",
        "after a resume, and with a wake as the newest message"
    );
}

// Given a plan conversation the driver never spoke in
// Then Jev is sent `task: null`, before and after a resume
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_driver_message_the_task_is_null() {
    let env = env();
    let target = env.ws.join("main.rs");
    let provider = provider(
        vec![write_call(&target), write_call(&target)],
        vec![verdict("violates"), verdict("violates")],
    )
    .await;
    let first = state(&env, &provider, true);
    let id = plan_session(&first, &env).await;
    wake(&first, id, "drift: look at the lexer").await;
    wait_events(&first, id, "permission.requested", 1).await;
    first.close_session(id).await.unwrap();
    drop(first);

    let second = state(&env, &provider, true);
    second
        .create_or_resume_session(&env.ws, None, None, Some(id))
        .await
        .unwrap();
    wake(&second, id, "drift again").await;
    wait_events(&second, id, "permission.requested", 2).await;
    assert_eq!(task_asked(&provider, 0), Value::Null);
    assert_eq!(task_asked(&provider, 1), Value::Null);
}
