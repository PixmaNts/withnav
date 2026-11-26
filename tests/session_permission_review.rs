#![cfg(feature = "test-support")]
//! The parallel metacognition review of a permission question, through the session service and
//! the HTTP surface: a scripted reviewer answers beside the Captain's card, the first decisive
//! answer wins, the review only ever allows once, and the Captain can hold the card.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use withnav::bus::event::{EventEnvelope, EventKind};
use withnav::server::{AppState, app};
use withnav::session::ChildPreparationBarrier;
use withnav::session::permission_review::PermissionReviewer;

mod common;
use common::TestEnv as Env;
use common::server::{TestServerConfig, captain_command, request_json, stored_events as events};
use common::sse::{CapturingProvider, http_sse, sse_body, text_response as done, tool_calls_chunk};

#[test]
fn permission_review_is_a_durable_wire_event() {
    let kind = EventKind::from_wire("permission.review").expect("review kind is registered");
    assert_eq!(kind.as_str(), "permission.review");
    assert!(kind.is_durable());
}

/// One scripted answer of the reviewer.
#[derive(Clone)]
enum Script {
    Answer(&'static str),
    /// Never answers: waits until the review is cancelled (or its deadline passes).
    Hang,
}

const ALLOW: &str = r#"{"decision":"allow_once","conclusion":"Writing the notes is the request itself.","basis":"write the notes","consequence":"creates notes.txt in the project"}"#;
const WAIT: &str = r#"{"decision":"wait","conclusion":"The write goes beyond the request.","basis":"read the notes","consequence":"changes notes.txt"}"#;

#[derive(Default)]
struct Scripted {
    answers: Mutex<VecDeque<Script>>,
    inputs: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(answers: &[Script]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.iter().cloned().collect()),
            ..Self::default()
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn inputs(&self) -> Vec<String> {
        self.inputs.lock().unwrap().clone()
    }
}

impl PermissionReviewer for Scripted {
    fn model(&self) -> String {
        "scripted".into()
    }

    fn review<'a>(
        &'a self,
        _instructions: &'a str,
        input: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Option<String>> {
        self.inputs.lock().unwrap().push(input.to_owned());
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Script::Hang);
        Box::pin(async move {
            match next {
                Script::Answer(text) => Some(text.to_owned()),
                Script::Hang => {
                    cancel.cancelled().await;
                    None
                }
            }
        })
    }
}

/// A provider that answers every Instinct question with 0 (not requested, not read-only, not
/// recoverable: the built-in flow asks the Captain) and the model with `chat`, in order.
async fn provider(chat: Vec<Vec<u8>>) -> CapturingProvider {
    let chat = Mutex::new(chat.into_iter());
    common::sse::spawn_responding_provider(200, move |_, request| {
        if request.contains("/systemone") {
            let body: Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap_or_default())
                    .unwrap_or_default();
            let answers: serde_json::Map<String, Value> = body["questions"]
                .as_object()
                .map(|q| {
                    q.keys()
                        .map(|id| (id.clone(), json!({"type": "noul", "noul": 0.0})))
                        .collect()
                })
                .unwrap_or_default();
            let body = json!({ "answers": answers }).to_string();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        } else {
            chat.lock().unwrap().next().unwrap_or_else(|| done("done"))
        }
    })
    .await
}

fn write_call(id: &str, path: &std::path::Path) -> Vec<u8> {
    http_sse(&sse_body(&[&tool_calls_chunk(&[(
        id,
        "write",
        json!({"path": path, "content": format!("from {id}")}),
    )])]))
}

fn set_review(env: &Env, review: &str) {
    let file = env.root.join("cfg/config.json");
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    settings["permissions"] = json!({"review": review});
    std::fs::write(&file, settings.to_string()).unwrap();
}

struct Session {
    provider: CapturingProvider,
    state: AppState,
    id: ulid::Ulid,
    router: axum::Router,
    target: std::path::PathBuf,
}

/// A root session under `review` whose model writes `notes.txt` (the built-in flow asks the
/// Captain), then says done.
async fn session(env: &Env, review: &str, reviewer: Arc<Scripted>, deadline: Duration) -> Session {
    let target = env.ws.join("notes.txt");
    session_with(
        env,
        review,
        reviewer,
        deadline,
        vec![write_call("w1", &target), done("written")],
    )
    .await
}

/// [`session`] whose model answers `chat`, in order.
async fn session_with(
    env: &Env,
    review: &str,
    reviewer: Arc<Scripted>,
    deadline: Duration,
    chat: Vec<Vec<u8>>,
) -> Session {
    let target = env.ws.join("notes.txt");
    let provider = provider(chat).await;
    let state = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .extra_alias("instinct", "instinct-test")
        .bearer_token("test-token")
        .state();
    set_review(env, review);
    state
        .service()
        .install_permission_reviewer(reviewer, deadline);
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    let router = authenticated(state.clone());
    Session {
        provider,
        state,
        id,
        router,
        target,
    }
}

fn authenticated(state: AppState) -> axum::Router {
    app(state).layer(axum::middleware::from_fn(
        |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
            request
                .headers_mut()
                .insert("authorization", "Bearer test-token".parse().unwrap());
            next.run(request).await
        },
    ))
}

async fn say(state: &AppState, id: ulid::Ulid, text: &str) {
    state
        .send(captain_command(id, text.to_string()))
        .await
        .expect("send");
}

/// Waits until at least `count` events of `kind` exist; returns them all.
async fn wait_events(
    state: &AppState,
    id: ulid::Ulid,
    kind: &str,
    count: usize,
) -> Vec<EventEnvelope> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let found = events(state, id, kind).await;
            if found.len() >= count {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("session {id}: timed out waiting for {count} `{kind}` events"))
}

/// The request raised by the write, once the turn yielded.
async fn requested(s: &Session) -> EventEnvelope {
    say(&s.state, s.id, "write the notes").await;
    let request = wait_events(&s.state, s.id, "permission.requested", 1)
        .await
        .remove(0);
    wait_events(&s.state, s.id, "turn.end", 1).await;
    request
}

fn request_id(event: &EventEnvelope) -> String {
    event.payload["request_id"].as_str().unwrap().to_owned()
}

async fn answer(s: &Session, request: &str, body: Value) -> (StatusCode, Value) {
    request_json(
        &s.router,
        "POST",
        &format!("/v1/sessions/{}/permissions/{request}", s.id),
        Some(&body),
    )
    .await
}

async fn waiting(s: &Session) -> Vec<Value> {
    let (status, body) = request_json(&s.router, "GET", "/v1/waiting", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["waiting"].as_array().cloned().unwrap_or_default()
}

/// The lines of the review log.
async fn log_lines(env: &Env, count: usize) -> Vec<Value> {
    let path = env.root.join("data/permission-reviews.jsonl");
    for _ in 0..400 {
        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        if lines.len() >= count {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the review log never reached {count} lines");
}

async fn review_states(s: &Session) -> Vec<String> {
    events(&s.state, s.id, "permission.review")
        .await
        .iter()
        .map(|e| e.payload["state"].as_str().unwrap().to_owned())
        .collect()
}

fn parked() -> ChildPreparationBarrier {
    ChildPreparationBarrier::default()
}

// Given `permissions.review: on` and a review that finds the write implied by the request
// When the Captain does not answer
// Then the review allows the call once: it runs, its events and its decision row name the
//      review, the resolution carries the short reasoning, and the log records the winner
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_review_allows_once_while_the_captain_is_away() {
    let env = Env::new();
    let reviewer = Scripted::new(&[Script::Answer(ALLOW)]);
    let s = session(&env, "on", reviewer.clone(), Duration::from_secs(20)).await;
    let request = requested(&s).await;
    assert!(
        request.payload["call_digest"]
            .as_str()
            .is_some_and(|d| d.len() == 64)
    );
    assert!(request.payload["turn_id"].is_string());
    wait_events(&s.state, s.id, "task.done", 1).await;
    assert_eq!(std::fs::read_to_string(&s.target).unwrap(), "from w1");
    let resolved = events(&s.state, s.id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["resolution"], "allow_once");
    assert_eq!(resolved[0].payload["approved_by"], "metacognition");
    assert_eq!(
        resolved[0].payload["review"],
        json!({
            "conclusion": "Writing the notes is the request itself.",
            "basis": "write the notes",
            "consequence": "creates notes.txt in the project",
        })
    );
    let post = events(&s.state, s.id, "tool.post_call").await;
    assert_eq!(post[0].payload["metadata"]["approved_by"], "metacognition");
    assert_eq!(review_states(&s).await, ["reviewing"]);
    let rows = withnav::permission::store::PermissionStore::open(env.root.join("data"))
        .unwrap()
        .list_decisions(
            &withnav::permission::decisions::DecisionFilter {
                session_id: Some(s.id.to_string()),
                ..Default::default()
            },
            10,
        )
        .unwrap();
    assert_eq!(rows[0].approved_by.as_deref(), Some("metacognition"));
    let line = log_lines(&env, 1).await.remove(0);
    assert_eq!(line["mode"], "on");
    assert_eq!(line["review"]["decision"], "allow_once");
    assert_eq!(line["winner"], "metacognition");
    assert_eq!(line["call_digest"], request.payload["call_digest"]);
    assert_eq!(line["labels"], Value::Null);
    assert_eq!(reviewer.calls(), 1);
    s.state.service().close_all().await;
}

// Given a review that is still thinking
// When the Captain answers the card
// Then the Captain's answer runs the call, the review is cancelled, and the log says so
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_captain_answers_first_and_the_review_is_cancelled() {
    let env = Env::new();
    let reviewer = Scripted::new(&[Script::Hang]);
    let s = session(&env, "on", reviewer.clone(), Duration::from_secs(20)).await;
    let request = requested(&s).await;
    let (status, body) = answer(&s, &request_id(&request), json!({"decision":"allow_once"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_events(&s.state, s.id, "task.done", 1).await;
    let resolved = events(&s.state, s.id, "permission.resolved").await;
    assert_eq!(resolved[0].payload["approved_by"], "captain");
    let line = log_lines(&env, 1).await.remove(0);
    assert_eq!(line["review"]["decision"], "cancelled");
    assert_eq!(line["captain"]["resolution"], "allow_once");
    assert_eq!(line["winner"], "captain");
    s.state.service().close_all().await;
}

// Given a review that allowed the call and is parked just before its claim
// When the Captain answers in between
// Then the Captain's answer wins, and the review's late answer is never sent
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_parked_before_its_claim_loses_to_the_captain() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Answer(ALLOW)]),
        Duration::from_secs(20),
    )
    .await;
    let barrier = parked();
    s.state.service().pause_review_before_claim(barrier.clone());
    let request = requested(&s).await;
    barrier.reached.notified().await;
    let (status, body) = answer(
        &s,
        &request_id(&request),
        json!({"decision":"reject","message":"not now"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    barrier.release.notify_one();
    wait_events(&s.state, s.id, "task.done", 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resolved = events(&s.state, s.id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["resolution"], "reject");
    assert!(!s.target.exists(), "the rejected call never ran");
    s.state.service().close_all().await;
}

// Given a review that claimed the request and is parked before its answer reaches the runtime
// When the Captain answers the card
// Then the Captain is told the call already started, by the review, and the call runs once
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_captain_answer_after_the_review_claim_is_told_already_started() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Answer(ALLOW)]),
        Duration::from_secs(20),
    )
    .await;
    let barrier = parked();
    s.state.service().pause_review_after_claim(barrier.clone());
    let request = requested(&s).await;
    barrier.reached.notified().await;
    let (status, body) = answer(
        &s,
        &request_id(&request),
        json!({"decision":"reject","message":"stop"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "permission_already_started");
    assert_eq!(body["approved_by"], "metacognition");
    barrier.release.notify_one();
    wait_events(&s.state, s.id, "task.done", 1).await;
    let resolved = events(&s.state, s.id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["approved_by"], "metacognition");
    assert!(s.target.exists());
    s.state.service().close_all().await;
}

// Given a review that allowed the call and is parked before its claim
// When the Captain holds the card
// Then the review stops, `held` is published, the card stays open, and the Captain's later
//      answer decides
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_before_the_claim_keeps_the_card_open_and_stops_the_review() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Answer(ALLOW)]),
        Duration::from_secs(20),
    )
    .await;
    let barrier = parked();
    s.state.service().pause_review_before_claim(barrier.clone());
    let request = requested(&s).await;
    barrier.reached.notified().await;
    let id = request_id(&request);
    let (status, body) = answer(&s, &id, json!({"decision":"hold"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"accepted": true, "held": true}));
    barrier.release.notify_one();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        events(&s.state, s.id, "permission.resolved")
            .await
            .is_empty()
    );
    assert!(!s.target.exists());
    assert_eq!(review_states(&s).await, ["reviewing", "held"]);
    let entries = waiting(&s).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["review_state"], "held");
    let (status, _) = answer(&s, &id, json!({"decision":"allow_once"})).await;
    assert_eq!(status, StatusCode::OK);
    wait_events(&s.state, s.id, "task.done", 1).await;
    let resolved = events(&s.state, s.id, "permission.resolved").await;
    assert_eq!(resolved[0].payload["approved_by"], "captain");
    s.state.service().close_all().await;
}

// Given a review that claimed the request
// When the Captain holds the card
// Then the hold is refused as already started, and nothing says the call was undone
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_after_the_claim_never_implies_the_call_was_undone() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Answer(ALLOW)]),
        Duration::from_secs(20),
    )
    .await;
    let barrier = parked();
    s.state.service().pause_review_after_claim(barrier.clone());
    let request = requested(&s).await;
    barrier.reached.notified().await;
    let (status, body) = answer(&s, &request_id(&request), json!({"decision":"hold"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "permission_already_started");
    assert!(!body.to_string().contains("undo"), "{body}");
    barrier.release.notify_one();
    wait_events(&s.state, s.id, "task.done", 1).await;
    assert!(s.target.exists());
    assert!(!review_states(&s).await.contains(&"held".to_owned()));
    s.state.service().close_all().await;
}

// Given a review that judges the write beyond the request
// When it answers
// Then the card stays open with its conclusion, and the Captain decides
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_that_says_wait_keeps_the_question_open_with_its_conclusion() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Answer(WAIT)]),
        Duration::from_secs(20),
    )
    .await;
    let request = requested(&s).await;
    let review = wait_events(&s.state, s.id, "permission.review", 2).await;
    assert_eq!(review[1].payload["state"], "wait");
    assert_eq!(
        review[1].payload["conclusion"],
        "The write goes beyond the request."
    );
    assert_eq!(review[1].payload["basis"], "read the notes");
    assert_eq!(review[1].payload["consequence"], "changes notes.txt");
    assert_eq!(
        review[1].payload["request_id"],
        request.payload["request_id"]
    );
    assert!(
        events(&s.state, s.id, "permission.resolved")
            .await
            .is_empty()
    );
    assert_eq!(waiting(&s).await[0]["review_state"], "wait");
    let (status, _) = answer(&s, &request_id(&request), json!({"decision":"allow_once"})).await;
    assert_eq!(status, StatusCode::OK);
    wait_events(&s.state, s.id, "task.done", 1).await;
    s.state.service().close_all().await;
}

// Given a review that never answers within its deadline, and one that answers with prose or
//       with anything but `allow_once` and `wait`
// When the review ends
// Then it is `unavailable`, and the question is exactly as without a review
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_or_malformed_review_leaves_the_question_as_today() {
    for (script, deadline) in [
        (Script::Hang, Duration::from_millis(200)),
        (Script::Answer("Sure, go ahead."), Duration::from_secs(20)),
        (
            Script::Answer(
                r#"{"decision":"allow_always","conclusion":"c","basis":"b","consequence":"x"}"#,
            ),
            Duration::from_secs(20),
        ),
    ] {
        let env = Env::new();
        let s = session(&env, "on", Scripted::new(&[script]), deadline).await;
        let request = requested(&s).await;
        let review = wait_events(&s.state, s.id, "permission.review", 2).await;
        assert_eq!(review[1].payload["state"], "unavailable");
        assert!(review[1].payload.get("conclusion").is_none());
        assert!(
            events(&s.state, s.id, "permission.resolved")
                .await
                .is_empty()
        );
        assert!(!s.target.exists());
        let (status, _) = answer(&s, &request_id(&request), json!({"decision":"allow_once"})).await;
        assert_eq!(status, StatusCode::OK);
        wait_events(&s.state, s.id, "task.done", 1).await;
        assert!(
            withnav::permission::store::PermissionStore::open(env.root.join("data"))
                .unwrap()
                .list(&Default::default())
                .unwrap()
                .is_empty(),
            "no rule is ever saved by a review"
        );
        s.state.service().close_all().await;
    }
}

// Given a review that allowed the call and is parked before its claim
// When the Captain sends a new message, or stops the session, in between
// Then the request is cancelled, and the review's late answer runs nothing
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_or_a_stop_invalidates_a_late_review_answer() {
    for stop in [false, true] {
        let env = Env::new();
        let s = session(
            &env,
            "on",
            Scripted::new(&[Script::Answer(ALLOW)]),
            Duration::from_secs(20),
        )
        .await;
        let barrier = parked();
        s.state.service().pause_review_before_claim(barrier.clone());
        requested(&s).await;
        barrier.reached.notified().await;
        if stop {
            s.state.close_session(s.id).await.unwrap();
        } else {
            say(&s.state, s.id, "actually, leave the notes alone").await;
        }
        let resolved = wait_events(&s.state, s.id, "permission.resolved", 1).await;
        assert_eq!(
            resolved[0].payload["resolution"], "cancelled",
            "stop={stop}"
        );
        barrier.release.notify_one();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(events(&s.state, s.id, "permission.resolved").await.len(), 1);
        assert!(
            !s.target.exists(),
            "stop={stop}: the late answer ran nothing"
        );
        s.state.service().close_all().await;
    }
}

// Given a pending request
// When an answer names another call digest or another turn
// Then it is refused as stale; the right digest and turn are accepted
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_for_another_digest_or_turn_is_stale() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Hang]),
        Duration::from_secs(20),
    )
    .await;
    let request = requested(&s).await;
    let id = request_id(&request);
    let digest = request.payload["call_digest"].clone();
    let turn = request.payload["turn_id"].clone();
    for body in [
        json!({"decision":"allow_once","call_digest":"0".repeat(64)}),
        json!({"decision":"allow_once","call_digest":digest,"turn_id":ulid::Ulid::generate()}),
        json!({"decision":"hold","call_digest":"0".repeat(64)}),
    ] {
        let (status, reply) = answer(&s, &id, body.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(reply["reason"], "permission_stale", "{body}");
    }
    let (status, reply) = answer(
        &s,
        &id,
        json!({"decision":"allow_once","call_digest":digest,"turn_id":turn}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    wait_events(&s.state, s.id, "task.done", 1).await;
    s.state.service().close_all().await;
}

// Given a review that allowed the call and is parked before its claim
// When a deny rule covering the call is written, then the review claims
// Then the call is decided again before it runs and the rule denies it
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deny_rule_written_during_the_review_still_denies() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Answer(ALLOW)]),
        Duration::from_secs(20),
    )
    .await;
    let barrier = parked();
    s.state.service().pause_review_before_claim(barrier.clone());
    requested(&s).await;
    barrier.reached.notified().await;
    withnav::permission::store::PermissionStore::open(env.root.join("data"))
        .unwrap()
        .add(
            "write",
            "*",
            withnav::permission::rules::Effect::Deny,
            withnav::permission::rules::RuleScope::Global,
            withnav::permission::rules::RuleOrigin::Manual,
        )
        .unwrap();
    barrier.release.notify_one();
    wait_events(&s.state, s.id, "task.done", 1).await;
    assert!(!s.target.exists(), "the deny rule stopped the call");
    let post = events(&s.state, s.id, "tool.post_call").await;
    assert_eq!(post[0].payload["status"], "denied", "{:?}", post[0].payload);
    s.state.service().close_all().await;
}

// Given `permissions.review: shadow`, the default
// When the review allows the call and the Captain rejects it
// Then nothing is published and the Captain's answer decides; the log keeps both answers
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_mode_logs_the_review_and_publishes_nothing() {
    let env = Env::new();
    let reviewer = Scripted::new(&[Script::Answer(ALLOW)]);
    let s = session(&env, "shadow", reviewer.clone(), Duration::from_secs(20)).await;
    let request = requested(&s).await;
    for _ in 0..200 {
        if reviewer.calls() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        events(&s.state, s.id, "permission.resolved")
            .await
            .is_empty()
    );
    assert!(!s.target.exists());
    assert_eq!(waiting(&s).await[0]["review_state"], Value::Null);
    let (status, _) = answer(
        &s,
        &request_id(&request),
        json!({"decision":"reject","message":"no"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_events(&s.state, s.id, "task.done", 1).await;
    assert!(
        review_states(&s).await.is_empty(),
        "shadow publishes nothing"
    );
    let line = log_lines(&env, 1).await.remove(0);
    assert_eq!(line["mode"], "shadow");
    assert_eq!(line["review"]["decision"], "allow_once");
    assert_eq!(line["review"]["model"], "scripted");
    assert_eq!(line["captain"]["resolution"], "reject");
    assert_eq!(line["winner"], "captain");
    s.state.service().close_all().await;
}

// Given `permissions.review: off`
// When the flow asks the Captain
// Then no metacognition call is made and nothing is logged
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_mode_makes_no_metacognition_call() {
    let env = Env::new();
    let reviewer = Scripted::new(&[Script::Answer(ALLOW)]);
    let s = session(&env, "off", reviewer.clone(), Duration::from_secs(20)).await;
    let request = requested(&s).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(reviewer.calls(), 0);
    let (status, _) = answer(&s, &request_id(&request), json!({"decision":"allow_once"})).await;
    assert_eq!(status, StatusCode::OK);
    wait_events(&s.state, s.id, "task.done", 1).await;
    assert!(!env.root.join("data/permission-reviews.jsonl").exists());
    s.state.service().close_all().await;
}

// Given a question under review when the process dies
// When the session is resumed
// Then the question expires, as any unresolved question does, and nothing runs
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_expires_a_question_under_review() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Hang]),
        Duration::from_secs(20),
    )
    .await;
    let request = requested(&s).await;
    let second = TestServerConfig::mock(&env.root, &s.provider.url)
        .rules_home(&env.home)
        .extra_alias("instinct", "instinct-test")
        .state();
    let (resumed, _) = second
        .create_or_resume_session(&env.ws, None, false, Some(s.id))
        .await
        .unwrap();
    assert_eq!(resumed, s.id);
    let expired = wait_events(&second, s.id, "permission.resolved", 1).await;
    assert_eq!(expired[0].payload["resolution"], "expired");
    assert_eq!(
        expired[0].payload["request_id"],
        request.payload["request_id"]
    );
    assert!(!s.target.exists());
}

// Given a call the review allowed, whose `permission.resolved` was stored but whose result was
//       not, when the process dies
// When the session is resumed and the Captain goes on
// Then the call is not dispatched again: the model reads it as settled without a result
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_allowed_call_interrupted_by_a_restart_is_not_run_again() {
    let env = Env::new();
    let marker = env.ws.join("ran");
    // The call takes long enough that the process "dies" while it runs: its result is never
    // stored by this incarnation before the resume.
    let command = format!("sleep 4 && echo once >> {}", marker.display());
    let s = session_with(
        &env,
        "on",
        Scripted::new(&[Script::Answer(ALLOW)]),
        Duration::from_secs(20),
        vec![
            http_sse(&sse_body(&[&tool_calls_chunk(&[(
                "b1",
                "bash",
                json!({"command": command}),
            )])])),
            done("after restart"),
        ],
    )
    .await;
    say(&s.state, s.id, "append the line").await;
    let resolved = wait_events(&s.state, s.id, "permission.resolved", 1).await;
    assert_eq!(resolved[0].payload["approved_by"], "metacognition");
    let second = TestServerConfig::mock(&env.root, &s.provider.url)
        .rules_home(&env.home)
        .extra_alias("instinct", "instinct-test")
        .state();
    let (resumed, _) = second
        .create_or_resume_session(&env.ws, None, false, Some(s.id))
        .await
        .unwrap();
    assert_eq!(resumed, s.id);
    say(&second, s.id, "carry on").await;
    let done_events = wait_events(&second, s.id, "task.done", 1).await;
    assert!(!done_events.is_empty());
    assert!(
        !marker.exists(),
        "the resumed session never dispatched the call again"
    );
    let body = s.provider.bodies().pop().unwrap();
    assert!(body.contains("\"tool_call_id\":\"b1\""), "{body}");
    assert!(body.contains("carry on"), "{body}");
    // Only the first incarnation's dispatch remains: one line at most, never two.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let lines = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(lines.lines().count() <= 1, "{lines}");
}

// Given a conversation with an earlier request carried out, a tool output in the current request
//       and the agent's proposal the Captain accepts
// When the review reads the call
// Then its input holds the current request and the proposal, and no output, no earlier request
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_review_input_holds_no_memory_output_or_earlier_request() {
    let env = Env::new();
    let target = env.ws.join("notes.txt");
    let provider = provider(vec![
        http_sse(&sse_body(&[&tool_calls_chunk(&[(
            "r0",
            "read",
            json!({"path": env.ws.join("seed.txt")}),
        )])])),
        done("Read it. Shall I write the notes?"),
        http_sse(&sse_body(&[&tool_calls_chunk(&[(
            "r1",
            "read",
            json!({"path": env.ws.join("seed.txt")}),
        )])])),
        write_call("w1", &target),
        done("written"),
    ])
    .await;
    std::fs::write(env.ws.join("seed.txt"), "OUTPUT_SECRET_42").unwrap();
    let state = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .extra_alias("instinct", "instinct-test")
        .state();
    set_review(&env, "on");
    let reviewer = Scripted::new(&[Script::Answer(ALLOW)]);
    state
        .service()
        .install_permission_reviewer(reviewer.clone(), Duration::from_secs(20));
    let id = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, id, "EARLIER_REQUEST read the seed").await;
    wait_events(&state, id, "turn.end", 1).await;
    say(&state, id, "CURRENT_REQUEST yes, write them").await;
    wait_events(&state, id, "task.done", 2).await;
    let inputs = reviewer.inputs();
    assert_eq!(inputs.len(), 1, "{inputs:#?}");
    let input = &inputs[0];
    assert!(input.contains("CURRENT_REQUEST yes, write them"), "{input}");
    assert!(input.contains("Shall I write the notes?"), "{input}");
    assert!(input.contains("\"tool\":\"write\""), "{input}");
    assert!(!input.contains("EARLIER_REQUEST"), "{input}");
    assert!(!input.contains("OUTPUT_SECRET_42"), "{input}");
    assert!(input.len() <= withnav::session::permission_review::MAX_INPUT_BYTES);
    state.service().close_all().await;
}

// Given a child session whose call the built-in flow puts to the Captain
// When the request is raised
// Then no review starts for it: a child's request is its parent's Captain's
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_request_is_not_reviewed() {
    let env = Env::new();
    let agents = env.ws.join(".withnav/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("writer.md"),
        "---\nmode: subagent\naccess: full\n---\nWrite what is asked.",
    )
    .unwrap();
    let target = env.ws.join("notes.txt");
    let provider = provider(vec![done("Hello."), write_call("c1", &target)]).await;
    let state = TestServerConfig::mock(&env.root, &provider.url)
        .rules_home(&env.home)
        .extra_alias("instinct", "instinct-test")
        .state();
    set_review(&env, "on");
    let reviewer = Scripted::new(&[Script::Answer(ALLOW)]);
    state
        .service()
        .install_permission_reviewer(reviewer.clone(), Duration::from_secs(20));
    let parent = state.create_session(&env.ws, None, false).await.unwrap();
    say(&state, parent, "hello").await;
    wait_events(&state, parent, "turn.end", 1).await;
    let child = state
        .create_child_session(parent, "writer", "write the notes")
        .await
        .unwrap();
    wait_events(&state, child, "permission.requested", 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(reviewer.calls(), 0);
    assert!(!target.exists());
    state.service().close_all().await;
}

// Given a request waiting for the Captain
// When the waiting list is read, then the request is answered from another client
// Then the list shows it once, keyed by session and turn, with a short summary; then it is gone
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_waiting_list_drops_a_request_answered_from_any_client() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Hang]),
        Duration::from_secs(20),
    )
    .await;
    let request = requested(&s).await;
    let entries = waiting(&s).await;
    assert_eq!(entries.len(), 1, "{entries:#?}");
    let entry = &entries[0];
    assert_eq!(entry["session_id"], s.id.to_string());
    assert_eq!(entry["request_id"], request.payload["request_id"]);
    assert_eq!(entry["turn_id"], request.payload["turn_id"]);
    assert_eq!(entry["tool"], "write");
    assert_eq!(entry["title"], "write the notes");
    assert!(entry["summary"].as_str().unwrap().starts_with("write: "));
    assert!(entry["summary"].as_str().unwrap().chars().count() <= 120);
    assert!(entry["since"].is_string());
    assert_eq!(entry["review_state"], "reviewing");
    s.state
        .answer_permission(
            s.id,
            request_id(&request).parse().unwrap(),
            withnav::agent::permissions::PermissionDecision::AllowOnce,
        )
        .await
        .unwrap();
    assert!(waiting(&s).await.is_empty());
    wait_events(&s.state, s.id, "task.done", 1).await;
    s.state.service().close_all().await;
}

// Given a review that allowed the call and is parked before its claim
// When bypass is turned on and answers the request
// Then only bypass's answer is applied; the review's claim finds the request taken
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bypass_and_the_review_cannot_both_claim() {
    let env = Env::new();
    let s = session(
        &env,
        "on",
        Scripted::new(&[Script::Answer(ALLOW)]),
        Duration::from_secs(20),
    )
    .await;
    let barrier = parked();
    s.state.service().pause_review_before_claim(barrier.clone());
    requested(&s).await;
    barrier.reached.notified().await;
    let (status, body) = request_json(
        &s.router,
        "POST",
        &format!("/v1/sessions/{}/bypass", s.id),
        Some(&json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_events(&s.state, s.id, "permission.resolved", 1).await;
    barrier.release.notify_one();
    wait_events(&s.state, s.id, "task.done", 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resolved = events(&s.state, s.id, "permission.resolved").await;
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].payload["approved_by"], "bypass");
    assert_eq!(events(&s.state, s.id, "tool.post_call").await.len(), 1);
    s.state.service().close_all().await;
}
