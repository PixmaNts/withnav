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
use withnav::session::goal_check::{
    CHECK_DEADLINE, LOG_FILE, MAX_CHECKS_PER_WINDOW, MAX_EVIDENCE_CHARS, MAX_INPUT_BYTES,
    MAX_OUTPUT_TOKENS,
};
use withnav::session::subagents::watchdog::Clock;
use withnav_client_api::goal::{MAX_CRITERIA, MAX_CRITERION_CHARS, MAX_OBJECTIVE_CHARS};
use withnav_client_api::ledger::{
    ClaimStatus, CriterionStatus, GoalCheck, MAX_CLAIM_CHARS, MAX_CLAIMS, MAX_TEXT_CHARS,
};

mod common;
use common::server::{TestServerConfig, collect_until, send_text, temp_workspace, wait_for};
use common::sse::{sse_response, text_chunk, tool_calls_chunk};

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
    scripted_env(meta, layer, Vec::new()).await
}

/// [`env`] whose cognition first gives the `script` answers, one per request, in order, with
/// every tool call allowed; "done" after them.
async fn scripted_env(meta: ResponseTemplate, layer: bool, script: Vec<ResponseTemplate>) -> Env {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({ "model": META })))
        .respond_with(meta)
        .with_priority(1)
        .mount(&server)
        .await;
    let scripted = !script.is_empty();
    for answer in script {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(answer)
            .up_to_n_times(1)
            .with_priority(2)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&[&text_chunk("done: all tests pass")]))
        .mount(&server)
        .await;
    let (tmp, ws) = temp_workspace();
    let mut config = TestServerConfig::mock(tmp.path(), &server.uri());
    if scripted {
        config = config.allow_everything();
    }
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
    let long = "x".repeat(MAX_EVIDENCE_CHARS + 180);
    let answer = format!(
        r#"Here you go: {{"criteria":[{{"index":1,"status":"contradicted","evidence":"{long}"}},{{"index":2,"status":"not_checked","evidence":"exit 101"}},{{"index":7,"status":"supported","evidence":""}}]}}"#
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
            {"index": 1, "status": "contradicted", "evidence": "x".repeat(MAX_EVIDENCE_CHARS)},
            {"index": 2, "status": "not_checked", "evidence": "exit 101"},
        ])
    );

    let requests = meta_requests(&env.server).await;
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request["max_tokens"], MAX_OUTPUT_TOKENS);
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

// Given a goal whose objective and criteria, all at their bounds, fill the digest
// When the turn ends
// Then the request's input stays within its byte bound and still opens with the goal
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_ledger_is_cut_to_the_input_bound() {
    let env = env(meta_answer(VALID), true).await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, &"o".repeat(MAX_OBJECTIVE_CHARS)).await;
    for n in 0..MAX_CRITERIA {
        goal(
            &env.state,
            id,
            &format!("criterion {n} {}", "c".repeat(MAX_CRITERION_CHARS - 10)),
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
    assert!(digest.contains(&format!("- objective: {}", "o".repeat(MAX_OBJECTIVE_CHARS))));
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

// Given as many checks started within the hour as the rate allows
// When two more turns end
// Then the first is one `skipped_rate` line without a call, the second adds no line (one rate
//      line per window keeps the log from growing with every turn); an hour after the first
//      check, a check runs again
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_past_the_rate_within_the_hour_is_skipped() {
    let env = env(meta_answer(VALID), true).await;
    let clock = Arc::new(ManualClock(Mutex::new(Instant::now())));
    env.state
        .install_goal_check_test(clock.clone(), CHECK_DEADLINE);
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, "ship the parser").await;
    goal(&env.state, id, "criterion cargo test passes").await;

    let max = MAX_CHECKS_PER_WINDOW;
    for n in 1..=max {
        turn(&env.state, id, &format!("turn {n}")).await;
        let logged = wait_lines(&env.log, n).await;
        assert_eq!(logged[n - 1]["outcome"], "ok");
        *clock.0.lock().unwrap() += Duration::from_secs(60);
    }
    turn(&env.state, id, "one past the rate").await;
    let logged = wait_lines(&env.log, max + 1).await;
    assert_eq!(logged[max]["outcome"], "skipped_rate");
    assert_eq!(meta_requests(&env.server).await.len(), max);
    turn(&env.state, id, "two past the rate").await;
    settle().await;
    assert_eq!(
        lines(&env.log).len(),
        max + 1,
        "a second rate skip in the window is not logged"
    );

    *clock.0.lock().unwrap() += Duration::from_secs(60 * 60);
    turn(&env.state, id, "an hour later").await;
    let logged = wait_lines(&env.log, max + 2).await;
    assert_eq!(logged[max + 1]["outcome"], "ok");
    assert_eq!(meta_requests(&env.server).await.len(), max + 1);
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

/// One assistant message calling `bash` once per command, in order.
fn bash_calls(commands: &[String]) -> ResponseTemplate {
    let calls: Vec<(String, Value)> = commands
        .iter()
        .enumerate()
        .map(|(n, command)| (format!("call{n}"), json!({ "command": command })))
        .collect();
    let calls: Vec<(&str, &str, Value)> = calls
        .iter()
        .map(|(id, args)| (id.as_str(), "bash", args.clone()))
        .collect();
    sse_response(&[&tool_calls_chunk(&calls)])
}

/// The ledger digest the metacognition request carried.
async fn sent_digest(server: &MockServer) -> String {
    meta_requests(server).await[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

// Given a turn that ran `echo cargo-test-passed` and then claimed that all tests pass
// When the model marks five criteria supported: with no evidence, with a quote the ledger does
//      not hold, with a quote of the agent's claim, with a quote of the goal itself, and with a
//      quote of the command line
// Then only the last stays supported; the others are logged as not checked, their quote kept
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supported_needs_a_quote_of_a_command_or_a_file_change() {
    let answer = json!({"criteria": [
        {"index": 1, "status": "supported", "evidence": ""},
        {"index": 2, "status": "supported", "evidence": "pytest exited 0"},
        {"index": 3, "status": "supported", "evidence": "all tests pass, everything is done"},
        {"index": 4, "status": "supported", "evidence": "criterion 1: cargo test passes"},
        {"index": 5, "status": "supported", "evidence": "exit 0 · echo cargo-test-passed"},
    ]})
    .to_string();
    let env = scripted_env(
        meta_answer(&answer),
        true,
        vec![
            bash_calls(&["echo cargo-test-passed".to_string()]),
            sse_response(&[&text_chunk("all tests pass, everything is done")]),
        ],
    )
    .await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, "ship the parser").await;
    for criterion in [
        "cargo test passes",
        "pytest passes",
        "lint",
        "docs",
        "release",
    ] {
        goal(&env.state, id, &format!("criterion {criterion}")).await;
    }
    turn(&env.state, id, "go").await;
    let logged = wait_lines(&env.log, 1).await;
    assert_eq!(logged[0]["outcome"], "ok");
    let digest = sent_digest(&env.server).await;
    assert!(
        digest.contains("- turn 1 · exit 0 · echo cargo-test-passed"),
        "{digest}"
    );
    assert!(
        digest.contains("- all tests pass, everything is done"),
        "{digest}"
    );
    assert_eq!(
        logged[0]["statuses"],
        json!([
            {"index": 1, "status": "not_checked", "evidence": ""},
            {"index": 2, "status": "not_checked", "evidence": "pytest exited 0"},
            {"index": 3, "status": "not_checked", "evidence": "all tests pass, everything is done"},
            {"index": 4, "status": "not_checked", "evidence": "criterion 1: cargo test passes"},
            {"index": 5, "status": "supported", "evidence": "exit 0 · echo cargo-test-passed"},
        ])
    );
}

// Given a goal of as many criteria as it may hold, each at its length bound, and a turn that ran
//      twenty long commands, then claimed at length that everything is done
// When the turn ends
// Then the goal takes at most half of the digest (the criteria that did not fit are named as
//      omitted), the newest commands are still there, the agent's claim is dropped before any
//      command, and a status for a criterion that was not sent is dropped
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_goal_leaves_room_for_command_evidence_and_claims_go_first() {
    let commands: Vec<String> = (0..20)
        .map(|n| format!("echo cmd{n:02}-{}", "e".repeat(MAX_TEXT_CHARS - 20)))
        .collect();
    let claim = format!(
        "I checked everything. {}",
        "All criteria are met. ".repeat(12)
    );
    let newest = format!("exit 0 · {}", commands[19]);
    let answer = json!({"criteria": [
        {"index": 1, "status": "supported", "evidence": newest.chars().take(MAX_EVIDENCE_CHARS).collect::<String>()},
        {"index": MAX_CRITERIA, "status": "supported", "evidence": newest.chars().take(MAX_EVIDENCE_CHARS).collect::<String>()},
    ]})
    .to_string();
    let env = scripted_env(
        meta_answer(&answer),
        true,
        vec![bash_calls(&commands), sse_response(&[&text_chunk(&claim)])],
    )
    .await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, "ship the parser").await;
    for n in 0..MAX_CRITERIA {
        goal(
            &env.state,
            id,
            &format!("criterion {n}{}", "c".repeat(MAX_CRITERION_CHARS - 2)),
        )
        .await;
    }
    turn(&env.state, id, "go").await;
    let logged = wait_lines(&env.log, 1).await;
    let requests = meta_requests(&env.server).await;
    let system = requests[0]["messages"][0]["content"].as_str().unwrap();
    let digest = sent_digest(&env.server).await;
    assert!(system.len() + digest.len() <= MAX_INPUT_BYTES);
    let budget = MAX_INPUT_BYTES - system.len();
    let goal_bytes: usize = digest
        .lines()
        .skip_while(|line| !line.starts_with("[goal:"))
        .take_while(|line| line.starts_with("[goal:") || line.starts_with("- "))
        .map(|line| line.len() + 1)
        .sum();
    assert!(goal_bytes <= budget / 2, "goal {goal_bytes} of {budget}");
    let sent = digest.matches("- criterion ").count();
    assert!((1..MAX_CRITERIA).contains(&sent), "{sent} criteria sent");
    assert!(
        digest.contains(&format!("- criteria {}-{MAX_CRITERIA} omitted", sent + 1)),
        "{digest}"
    );
    assert!(digest.contains(&commands[19]), "{digest}");
    assert!(!digest.contains("All criteria are met"), "{digest}");
    assert_eq!(
        logged[0]["statuses"],
        json!([{"index": 1, "status": "supported",
            "evidence": newest.chars().take(MAX_EVIDENCE_CHARS).collect::<String>()}])
    );
}

// Given a check waiting on a slow model
// When the session stops, then is resumed and runs a turn
// Then the stopped check is never logged, its slot is free (the next check calls the model),
//      and the resumed session has one watcher (one line, one call per turn end)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_check_is_not_logged_and_a_resume_keeps_one_watcher() {
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
    turn(&env.state, id, "one").await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while meta_requests(&env.server).await.is_empty() {
        assert!(Instant::now() < deadline, "the first check never called");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    env.state.close_session(id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert!(lines(&env.log).is_empty(), "{:?}", lines(&env.log));
    let (resumed_id, resumed) = env
        .state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    assert!(resumed);
    assert_eq!(resumed_id, id);
    turn(&env.state, id, "two").await;
    let logged = wait_lines(&env.log, 1).await;
    assert_eq!(logged[0]["outcome"], "ok");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        lines(&env.log).len(),
        1,
        "one line per turn end after resume"
    );
    assert_eq!(meta_requests(&env.server).await.len(), 2);
}

// Given as many checks within the hour as the rate allows
// When the session stops, is resumed and one more turn ends
// Then that check is `skipped_rate`: a resume does not reset the window
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_keeps_the_rate_window() {
    let env = env(meta_answer(VALID), true).await;
    let clock = Arc::new(ManualClock(Mutex::new(Instant::now())));
    env.state
        .install_goal_check_test(clock.clone(), CHECK_DEADLINE);
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, "ship the parser").await;
    goal(&env.state, id, "criterion cargo test passes").await;
    let max = MAX_CHECKS_PER_WINDOW;
    for n in 1..=max {
        turn(&env.state, id, &format!("turn {n}")).await;
        wait_lines(&env.log, n).await;
    }
    env.state.close_session(id).await.unwrap();
    env.state
        .create_or_resume_session(&env.ws, None, false, Some(id))
        .await
        .unwrap();
    turn(&env.state, id, "one past the rate").await;
    let logged = wait_lines(&env.log, max + 1).await;
    assert_eq!(logged[max]["outcome"], "skipped_rate");
    assert_eq!(meta_requests(&env.server).await.len(), max);
}

// Given the largest goal (objective, criteria count and criteria at their bounds)
// When the turn ends
// Then the request stays within its byte bound, keeps the whole objective, and names the
//      omitted criteria
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_largest_goal_stays_within_the_input_bound() {
    let env = env(meta_answer(VALID), true).await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    goal(&env.state, id, &"o".repeat(MAX_OBJECTIVE_CHARS)).await;
    for n in 0..MAX_CRITERIA {
        goal(
            &env.state,
            id,
            &format!("criterion {n}{}", "c".repeat(MAX_CRITERION_CHARS - 2)),
        )
        .await;
    }
    turn(&env.state, id, "go").await;
    let logged = wait_lines(&env.log, 1).await;
    let request = &meta_requests(&env.server).await[0];
    let system = request["messages"][0]["content"].as_str().unwrap();
    let digest = request["messages"][1]["content"].as_str().unwrap();
    assert!(system.len() + digest.len() <= MAX_INPUT_BYTES);
    assert_eq!(logged[0]["input_bytes"], system.len() + digest.len());
    assert!(digest.contains(&format!("- objective: {}", "o".repeat(MAX_OBJECTIVE_CHARS))));
    assert!(digest.matches("- criterion ").count() < MAX_CRITERIA);
    assert!(digest.contains("omitted"), "{digest}");
}

// Given eight sessions whose checks end at about the same time
// When their lines are appended to the one log
// Then every line stays one whole JSON object
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_keep_log_lines_whole() {
    let env = env(meta_answer(VALID), true).await;
    let mut ids = Vec::new();
    for _ in 0..8 {
        let id = env
            .state
            .create_session(&env.ws, None, false)
            .await
            .unwrap();
        goal(&env.state, id, "ship the parser").await;
        goal(&env.state, id, "criterion cargo test passes").await;
        ids.push(id);
    }
    let mut set = tokio::task::JoinSet::new();
    for id in ids {
        let state = env.state.clone();
        set.spawn(async move { turn(&state, id, "go").await });
    }
    while set.join_next().await.is_some() {}
    let logged = wait_lines(&env.log, 8).await;
    assert_eq!(logged.len(), 8);
}

/// The `goal_check` results a session published, oldest first.
async fn published(state: &AppState, id: ulid::Ulid) -> Vec<Value> {
    common::server::stored_events(state, id, "command.result")
        .await
        .into_iter()
        .map(|e| e.payload)
        .filter(|payload| payload["name"] == "goal_check")
        .collect()
}

/// Waits until `n` results are published.
async fn wait_published(state: &AppState, id: ulid::Ulid, n: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let results = published(state, id).await;
        if results.len() >= n {
            return results;
        }
        assert!(Instant::now() < deadline, "no {n} published goal checks");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A goal of revision 2 with one criterion, as most of these checks need.
async fn one_criterion(state: &AppState, id: ulid::Ulid) {
    goal(state, id, "ship the parser").await;
    goal(state, id, "criterion cargo test passes").await;
}

// Given a turn that ran `echo cargo-test-passed` and then claimed that all tests pass
// When the model gives three claims it cannot back: with no quote, with a command line the ledger
//      does not hold (and calls the claim false), and with a quote of the agent's own words
// Then each claim is logged and published `unverified`, never contradicted, its quote kept
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_without_ledger_evidence_is_unverified() {
    let answer = json!({
        "criteria": [{"index": 1, "status": "not_checked", "evidence": ""}],
        "claims": [
            {"claim": "all tests pass", "status": "supported", "evidence": ""},
            {"claim": "the build passes", "status": "contradicted", "evidence": "exit 1 · cargo build"},
            {"claim": "committed", "status": "supported", "evidence": "all tests pass, everything is done"},
        ],
    })
    .to_string();
    let env = scripted_env(
        meta_answer(&answer),
        true,
        vec![
            bash_calls(&["echo cargo-test-passed".to_string()]),
            sse_response(&[&text_chunk("all tests pass, everything is done")]),
        ],
    )
    .await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    one_criterion(&env.state, id).await;
    turn(&env.state, id, "go").await;
    let logged = wait_lines(&env.log, 1).await;
    let expected = json!([
        {"claim": "all tests pass", "status": "unverified", "evidence": ""},
        {"claim": "the build passes", "status": "unverified", "evidence": "exit 1 · cargo build"},
        {"claim": "committed", "status": "unverified", "evidence": "all tests pass, everything is done"},
    ]);
    assert_eq!(logged[0]["claims"], expected);
    let results = wait_published(&env.state, id, 1).await;
    assert_eq!(results[0]["data"]["claims"], expected);
}

// Given a turn that ran `echo built` and then `false`
// When the model backs one claim with the quoted line of each command, a third with a quote of
//      the agent's own claim, and gives more claims than the check keeps, some too long
// Then only the quoted command lines settle a claim, the list and each text stay within their
//      bounds
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_is_supported_only_by_a_quoted_command_line() {
    let long = "w".repeat(MAX_CLAIM_CHARS + 40);
    let answer = json!({
        "criteria": [{"index": 1, "status": "not_checked", "evidence": ""}],
        "claims": [
            {"claim": "it builds", "status": "supported", "evidence": "exit 0 · echo built"},
            {"claim": "lint is clean", "status": "contradicted", "evidence": "exit 1 · false"},
            {"claim": "docs updated", "status": "supported", "evidence": "docs are done"},
            {"claim": long, "status": "supported", "evidence": "exit 0 · echo built"},
            {"claim": "five", "status": "unverified", "evidence": ""},
            {"claim": "six", "status": "unverified", "evidence": ""},
        ],
    })
    .to_string();
    let env = scripted_env(
        meta_answer(&answer),
        true,
        vec![
            bash_calls(&["echo built".to_string(), "false".to_string()]),
            sse_response(&[&text_chunk("docs are done")]),
        ],
    )
    .await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    one_criterion(&env.state, id).await;
    turn(&env.state, id, "go").await;
    let logged = wait_lines(&env.log, 1).await;
    let digest = sent_digest(&env.server).await;
    assert!(digest.contains("- turn 1 · exit 1 · false"), "{digest}");
    let claims = logged[0]["claims"].as_array().unwrap();
    assert_eq!(claims.len(), MAX_CLAIMS);
    let statuses: Vec<&str> = claims
        .iter()
        .map(|c| c["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        [
            "supported",
            "contradicted",
            "unverified",
            "supported",
            "unverified"
        ]
    );
    assert_eq!(claims[0]["evidence"], "exit 0 · echo built");
    assert_eq!(
        claims[3]["claim"].as_str().unwrap().chars().count(),
        MAX_CLAIM_CHARS
    );
}

// Given the setting `goal_watch.notes` at its default (off) and an active goal
// When a turn ends and the check finishes
// Then exactly one `goal_check` result is published, the ledger holds it, and a change of the
//      goal's revision clears it from the ledger
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_check_is_published_once_and_folded_into_the_ledger() {
    let answer = json!({
        "criteria": [
            {"index": 1, "status": "supported", "evidence": "turn 1 · exit 0 · cargo test"},
            {"index": 2, "status": "not_checked", "evidence": ""},
        ],
        "claims": [{"claim": "tests pass", "status": "unverified", "evidence": ""}],
    })
    .to_string();
    let env = env(meta_answer(&answer), true).await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    one_criterion(&env.state, id).await;
    let turn_id = turn(&env.state, id, "go").await;
    wait_lines(&env.log, 1).await;
    let results = wait_published(&env.state, id, 1).await;
    settle().await;
    let results_after = published(&env.state, id).await;
    assert_eq!(results_after.len(), 1, "one result per finished check");

    let result = &results[0];
    assert_eq!(result["ok"], true);
    let data = &result["data"];
    assert_eq!(data["revision"], 2);
    assert_eq!(data["turn_id"], turn_id.as_str());
    assert_eq!(data["note_sent"], false);
    assert!(data.get("note").is_none(), "{data}");
    assert_eq!(data["statuses"][0]["index"], 1);
    // The model's `supported` quoted a line the ledger does not hold.
    assert_eq!(data["statuses"][0]["status"], "not_checked");
    assert_eq!(
        data["claims"],
        json!([{"claim": "tests pass", "status": "unverified", "evidence": ""}])
    );

    let ledger = env.state.ledger(id).await.unwrap();
    let check: GoalCheck = serde_json::from_value(data.clone()).unwrap();
    assert_eq!(ledger.goal_check, Some(check));
    let check = ledger.goal_check.unwrap();
    assert_eq!(check.statuses[0].status, CriterionStatus::NotChecked);
    assert_eq!(check.claims[0].status, ClaimStatus::Unverified);

    goal(&env.state, id, "criterion docs").await;
    assert_eq!(env.state.ledger(id).await.unwrap().goal_check, None);
}

// Given a first check whose published claim and quote carry a marker
// When the next turn ends and its check reads the ledger
// Then the digest the model receives holds nothing of the earlier check: a check never takes
//      its own previous verdict for evidence
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_digest_never_contains_an_earlier_check() {
    let answer = json!({
        "criteria": [{"index": 1, "status": "contradicted", "evidence": "PRIOR-QUOTE"}],
        "claims": [{"claim": "PRIOR-CLAIM", "status": "unverified", "evidence": "PRIOR-CLAIM-QUOTE"}],
    })
    .to_string();
    let env = env(meta_answer(&answer), true).await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    one_criterion(&env.state, id).await;
    turn(&env.state, id, "one").await;
    wait_published(&env.state, id, 1).await;
    assert!(env.state.ledger(id).await.unwrap().goal_check.is_some());
    turn(&env.state, id, "two").await;
    wait_published(&env.state, id, 2).await;

    let requests = meta_requests(&env.server).await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let sent = request.to_string();
        assert!(!sent.contains("PRIOR"), "{sent}");
        assert!(!sent.contains("goal_check"), "{sent}");
    }
}

// Given checks that end `malformed` and `provider_error`, and a second turn end skipped as in flight
// When they finish
// Then nothing is published for them: only the check that finished `ok` has a result
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_skipped_check_publishes_nothing() {
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
        one_criterion(&env.state, id).await;
        turn(&env.state, id, "go").await;
        let logged = wait_lines(&env.log, 1).await;
        assert_eq!(logged[0]["outcome"], outcome);
        settle().await;
        assert!(published(&env.state, id).await.is_empty(), "{outcome}");
        assert_eq!(env.state.ledger(id).await.unwrap().goal_check, None);
    }

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
    one_criterion(&env.state, id).await;
    let first = turn(&env.state, id, "one").await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while meta_requests(&env.server).await.is_empty() {
        assert!(Instant::now() < deadline, "the first check never called");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    turn(&env.state, id, "two").await;
    let logged = wait_lines(&env.log, 2).await;
    assert_eq!(logged[0]["outcome"], "skipped_in_flight");
    let results = wait_published(&env.state, id, 1).await;
    settle().await;
    let results_after = published(&env.state, id).await;
    assert_eq!(results_after.len(), 1);
    assert_eq!(results[0]["data"]["turn_id"], first.as_str());
}

// Given a model that answers with criteria only, as every answer did before claims existed
// When the turn ends
// Then the statuses are read as before and the check has no claims
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_without_claims_is_read_as_before() {
    let env = env(meta_answer(VALID), true).await;
    let id = env
        .state
        .create_session(&env.ws, None, false)
        .await
        .unwrap();
    one_criterion(&env.state, id).await;
    goal(&env.state, id, "criterion docs").await;
    turn(&env.state, id, "go").await;
    let logged = wait_lines(&env.log, 1).await;
    assert_eq!(logged[0]["outcome"], "ok");
    assert_eq!(logged[0]["claims"], json!([]));
    assert_eq!(
        logged[0]["statuses"],
        json!([
            {"index": 1, "status": "not_checked", "evidence": "turn 1 · exit 0 · cargo test"},
            {"index": 2, "status": "not_checked", "evidence": ""},
        ])
    );
    let results = wait_published(&env.state, id, 1).await;
    assert_eq!(results[0]["data"]["claims"], json!([]));
}
