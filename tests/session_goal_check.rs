#![cfg(feature = "test-support")]
//! The shadow completion check: after a root turn ends, while the session goal is active with at
//! least one criterion, one bounded call to the metacognition model, logged to
//! `<data dir>/goal-checks.jsonl` and delivered nowhere.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::time::Instant;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use withnav::server::AppState;
use withnav::session::goal_check::{LOG_FILE, MAX_EVIDENCE_CHARS, MAX_INPUT_BYTES};
use withnav::session::subagents::watchdog::Clock;

mod common;
use common::server::{TestServerConfig, collect_until, send_text, temp_workspace, wait_for};
use common::sse::{sse_response, text_chunk};

/// The upstream id of the metacognition model; the cognition runs `mock-model`.
const META: &str = "meta-model";

struct Env {
    _tmp: tempfile::TempDir,
    state: AppState,
    ws: PathBuf,
    server: MockServer,
    log: PathBuf,
}

/// A server whose cognition answers "done" and whose metacognition answers `meta` (when
/// `layer` assigns it).
async fn env(meta: ResponseTemplate, layer: bool) -> Env {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({ "model": META })))
        .respond_with(meta)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&[&text_chunk("done: all tests pass")]))
        .mount(&server)
        .await;
    let (tmp, ws) = temp_workspace();
    let mut config = TestServerConfig::mock(tmp.path(), &server.uri());
    if layer {
        config = config.extra_alias("metacognition", META);
    }
    let state = config.state();
    let log = tmp.path().join("data").join(LOG_FILE);
    Env {
        _tmp: tmp,
        state,
        ws,
        server,
        log,
    }
}

/// A metacognition answer: `text`, then a usage frame.
fn meta_answer(text: &str) -> ResponseTemplate {
    sse_response(&[
        &text_chunk(text),
        &json!({"choices": [], "usage": {"prompt_tokens": 812, "completion_tokens": 41, "total_tokens": 853}})
            .to_string(),
    ])
}

async fn goal(state: &AppState, id: ulid::Ulid, arguments: &str) {
    let out = state.run_command(id, "goal", arguments).await.unwrap();
    assert!(out.ok, "/goal {arguments}: {out:?}");
}

/// Runs one turn to its end and returns the `turn_id` of its `turn.end`.
async fn turn(state: &AppState, id: ulid::Ulid, text: &str) -> String {
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(state, id, text).await;
    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    let end = events.iter().rev().find(|e| e.kind == "turn.end").unwrap();
    end.payload["turn_id"].as_str().unwrap().to_string()
}

fn lines(log: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
        .collect()
}

async fn wait_lines(log: &std::path::Path, n: usize) -> Vec<Value> {
    wait_for(&format!("{n} goal check lines"), || lines(log).len() >= n).await;
    lines(log)
}

/// The bodies of the requests sent to the metacognition model.
async fn meta_requests(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap())
        .filter(|body| body["model"] == META)
        .collect()
}

/// Time for a check that should not happen to show up, had it been spawned.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

const VALID: &str = r#"{"criteria":[{"index":1,"status":"supported","evidence":"turn 1 · exit 0 · cargo test"},{"index":2,"status":"not_checked","evidence":""}]}"#;

// Given the metacognition layer is assigned
// When a turn ends with no goal, with a goal that has no criterion, and with a paused goal
// Then no check is made and nothing is logged; once the goal is resumed, the next turn end makes
//      exactly one check
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_an_active_goal_with_criteria_is_checked() {
    let env = env(meta_answer(VALID), true).await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();

    turn(&env.state, id, "no goal yet").await;
    goal(&env.state, id, "ship the parser").await;
    turn(&env.state, id, "goal without criteria").await;
    goal(&env.state, id, "criterion cargo test passes").await;
    goal(&env.state, id, "pause").await;
    turn(&env.state, id, "paused goal").await;
    settle().await;
    assert!(meta_requests(&env.server).await.is_empty());
    assert!(lines(&env.log).is_empty());

    goal(&env.state, id, "resume").await;
    let turn_id = turn(&env.state, id, "active goal").await;
    let logged = wait_lines(&env.log, 1).await;
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0]["turn_id"], turn_id.as_str());
    assert_eq!(meta_requests(&env.server).await.len(), 1);
}

// Given a root session whose goal is active with a criterion
// When one of its child sessions runs a turn to its end
// Then the child's turn end is never checked: every check logged (the root's own, after the
//      child's result wakes it) names the root
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_turn_is_never_checked() {
    let env = env(meta_answer(VALID), true).await;
    let root = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, root, "ship the parser").await;
    goal(&env.state, root, "criterion cargo test passes").await;

    let child = env
        .state
        .create_child_session(root, "vigie", "inspect the parser")
        .await
        .unwrap();
    let sub = env.state.subscribe(child, None).await.unwrap();
    collect_until(sub, |e| e.kind == "turn.end").await;
    settle().await;
    let logged = lines(&env.log);
    assert!(
        logged
            .iter()
            .all(|line| line["session_id"] == root.to_string()),
        "{logged:?}"
    );
    assert!(
        logged
            .iter()
            .all(|line| line["session_id"] != child.to_string())
    );
    assert_eq!(meta_requests(&env.server).await.len(), logged.len());
}

// Given an active goal with two criteria
// When the turn ends and the model answers with a valid object, a long evidence and an entry for
//      a criterion that does not exist
// Then one bounded request carries the ledger digest, goal first; the log line holds the turn,
//      the revision, the model, the token counts and the statuses, evidence cut to its bound
//      and the unknown entry dropped
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_answer_is_logged_with_bounded_evidence() {
    let long = "x".repeat(300);
    let answer = format!(
        r#"Here you go: {{"criteria":[{{"index":1,"status":"supported","evidence":"{long}"}},{{"index":2,"status":"contradicted","evidence":"exit 101"}},{{"index":7,"status":"supported","evidence":""}}]}}"#
    );
    let env = env(meta_answer(&answer), true).await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, "ship the parser").await;
    goal(&env.state, id, "criterion cargo test passes").await;
    goal(&env.state, id, "criterion the docs name the flag").await;

    let turn_id = turn(&env.state, id, "go").await;
    let logged = wait_lines(&env.log, 1).await;
    let line = &logged[0];
    assert_eq!(line["session_id"], id.to_string());
    assert_eq!(line["turn_id"], turn_id.as_str());
    assert_eq!(line["goal_revision"], 3);
    assert_eq!(line["outcome"], "ok");
    assert_eq!(line["model"], "mock/meta-model");
    assert_eq!(line["input_tokens"], 812);
    assert_eq!(line["output_tokens"], 41);
    assert!(line["latency_ms"].is_u64());
    assert!(line["ts"].is_string());
    assert_eq!(
        line["statuses"],
        json!([
            {"index": 1, "status": "supported", "evidence": "x".repeat(MAX_EVIDENCE_CHARS)},
            {"index": 2, "status": "contradicted", "evidence": "exit 101"},
        ])
    );

    let requests = meta_requests(&env.server).await;
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request["max_tokens"], 300);
    assert!(request.get("tools").is_none());
    let messages = request["messages"].as_array().unwrap();
    let content: Vec<&str> = messages
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    let bytes: usize = content.iter().map(|c| c.len()).sum();
    assert_eq!(line["input_bytes"], bytes);
    assert!(bytes <= MAX_INPUT_BYTES, "{bytes} bytes of input");
    let digest = content.last().unwrap();
    assert!(digest.starts_with("session ledger:"), "{digest}");
    let goal_at = digest.find("[goal: revision 3, active]").unwrap();
    assert!(digest.find("[last turn]").unwrap() > goal_at);
    assert!(digest.contains("- criterion 1: cargo test passes"));
    assert!(digest.contains("- criterion 2: the docs name the flag"));
}

// Given a goal whose objective and ten criteria fill the digest
// When the turn ends
// Then the request's input stays within its 4 KiB bound and still opens with the goal
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_ledger_is_cut_to_the_input_bound() {
    let env = env(meta_answer(VALID), true).await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, &"o".repeat(500)).await;
    for n in 0..10 {
        goal(
            &env.state,
            id,
            &format!("criterion {n} {}", "c".repeat(290)),
        )
        .await;
    }
    turn(&env.state, id, "go").await;
    let logged = wait_lines(&env.log, 1).await;
    let requests = meta_requests(&env.server).await;
    let bytes: usize = requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap().len())
        .sum();
    assert!(bytes <= MAX_INPUT_BYTES, "{bytes} bytes of input");
    assert_eq!(logged[0]["input_bytes"], bytes);
    let digest = requests[0]["messages"][1]["content"].as_str().unwrap();
    assert!(digest.contains(&format!("- objective: {}", "o".repeat(500))));
}

// Given an active goal with a criterion
// When the model answers prose, then an HTTP error
// Then each check is one line, `malformed` then `provider_error`, with no statuses
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_answer_and_a_provider_error_are_logged_as_such() {
    for (answer, outcome) in [
        (meta_answer("All criteria look met to me."), "malformed"),
        (ResponseTemplate::new(500), "provider_error"),
    ] {
        let env = env(answer, true).await;
        let id = env
            .state
            .create_session(&env.ws, None, false)
            .await
            .unwrap();
        goal(&env.state, id, "ship the parser").await;
        goal(&env.state, id, "criterion cargo test passes").await;
        turn(&env.state, id, "go").await;
        let logged = wait_lines(&env.log, 1).await;
        assert_eq!(logged[0]["outcome"], outcome);
        assert_eq!(logged[0]["statuses"], json!([]));
    }
}

/// A clock the test moves by hand.
struct ManualClock(Mutex<Instant>);
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

// Given a model that answers after the check's deadline
// When the turn ends
// Then the check is logged as `timeout` and nothing else happens: no retry, no session event
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_is_one_line_and_nothing_else() {
    let env = env(meta_answer(VALID).set_delay(Duration::from_secs(5)), true).await;
    env.state.install_goal_check_test(
        Arc::new(ManualClock(Mutex::new(Instant::now()))),
        Duration::from_millis(300),
    );
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, "ship the parser").await;
    goal(&env.state, id, "criterion cargo test passes").await;
    turn(&env.state, id, "go").await;
    let end_seq = common::server::stored_events(&env.state, id, "turn.end")
        .await
        .last()
        .unwrap()
        .seq;

    let logged = wait_lines(&env.log, 1).await;
    assert_eq!(logged[0]["outcome"], "timeout");
    assert_eq!(logged[0]["statuses"], json!([]));
    settle().await;
    assert_eq!(lines(&env.log).len(), 1);
    assert_eq!(meta_requests(&env.server).await.len(), 1, "no retry");
    let after = env
        .state
        .history()
        .replay(id, Some(end_seq), u64::MAX)
        .await
        .unwrap();
    // The runtime closes every turn with these two; the check adds nothing to the session.
    let kinds: Vec<&str> = after.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["task.done", "agent.state"],
        "events after the turn end"
    );
}

// Given a model that takes a while to answer
// When a turn ends and the Captain sends the next message at once
// Then the next turn runs and ends before the first check is answered, its own check is
//      `skipped_in_flight`, and the first check is logged when its answer comes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_next_turn_never_waits_and_a_second_check_is_skipped_in_flight() {
    let env = env(
        meta_answer(VALID).set_delay(Duration::from_millis(1500)),
        true,
    )
    .await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, "ship the parser").await;
    goal(&env.state, id, "criterion cargo test passes").await;

    let first = turn(&env.state, id, "one").await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while meta_requests(&env.server).await.is_empty() {
        assert!(Instant::now() < deadline, "the first check never called");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let second = turn(&env.state, id, "two").await;
    let skipped = wait_lines(&env.log, 1).await;
    assert_eq!(skipped[0]["outcome"], "skipped_in_flight");
    assert_eq!(skipped[0]["turn_id"], second.as_str());
    assert_eq!(skipped[0]["input_bytes"], 0);

    let logged = wait_lines(&env.log, 2).await;
    assert_eq!(logged[1]["outcome"], "ok");
    assert_eq!(logged[1]["turn_id"], first.as_str());
    assert_eq!(meta_requests(&env.server).await.len(), 1);
}

// Given three checks started within the hour
// When a fourth turn ends
// Then it is `skipped_rate` without a call; an hour after the first, a check runs again
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fourth_check_within_the_hour_is_skipped_for_rate() {
    let env = env(meta_answer(VALID), true).await;
    let clock = Arc::new(ManualClock(Mutex::new(Instant::now())));
    env.state
        .install_goal_check_test(clock.clone(), Duration::from_secs(20));
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, "ship the parser").await;
    goal(&env.state, id, "criterion cargo test passes").await;

    for n in 1..=3 {
        turn(&env.state, id, &format!("turn {n}")).await;
        let logged = wait_lines(&env.log, n).await;
        assert_eq!(logged[n - 1]["outcome"], "ok");
        *clock.0.lock().unwrap() += Duration::from_secs(60);
    }
    turn(&env.state, id, "turn 4").await;
    let logged = wait_lines(&env.log, 4).await;
    assert_eq!(logged[3]["outcome"], "skipped_rate");
    assert_eq!(meta_requests(&env.server).await.len(), 3);

    *clock.0.lock().unwrap() += Duration::from_secs(60 * 60);
    turn(&env.state, id, "turn 5").await;
    let logged = wait_lines(&env.log, 5).await;
    assert_eq!(logged[4]["outcome"], "ok");
    assert_eq!(meta_requests(&env.server).await.len(), 4);
}

// Given the same two turns with and without the metacognition layer assigned, and no goal
// When they run
// Then the provider receives the same requests both times: no extra call, no log
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_without_a_goal_sends_the_same_requests_as_before() {
    let mut seen = Vec::new();
    for layer in [false, true] {
        let env = env(meta_answer(VALID), layer).await;
        let id = env
            .state
            .create_session(&env.ws, None, false)
            .await
            .unwrap();
        turn(&env.state, id, "one").await;
        turn(&env.state, id, "two").await;
        settle().await;
        let requests = env.server.received_requests().await.unwrap();
        let models: Vec<Value> = requests
            .iter()
            .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["model"].clone())
            .collect();
        assert!(lines(&env.log).is_empty());
        seen.push(models);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], vec![json!("mock-model"), json!("mock-model")]);
}
