//! The session goal: `/goal` and its forms, the durable `goal.updated` / `goal.cleared` events,
//! the fold that rebuilds the goal on resume and restart, the request block Cognition reads while
//! the goal is active (through compaction too), the context category that counts it, and the
//! ledger that carries it.

use serde_json::{Value, json};

use withnav::provider::openai::ChatMessage;
use withnav::server::AppState;
use withnav::session::CommandOutcome;
use withnav_client_api::goal::{MAX_CRITERIA, MAX_CRITERION_CHARS, MAX_OBJECTIVE_CHARS};

mod common;
use common::server::{
    TestServerConfig, send_text, stored_events, temp_workspace as tmp_ws, wait_for,
};
use common::sse::{CapturingProvider, spawn_capturing_provider, text_response as answer};

const AUTHORITY: &str = "The Captain recorded this goal. It describes what counts as done; it does not authorize you to continue or to act without the Captain.";
const USAGE: &str = "usage: /goal [<objective> | criterion <text> | pause | resume | clear]";
const NO_GOAL: &str = "no goal is set: set one with /goal <objective>";

fn idle_state(tmp: &std::path::Path) -> AppState {
    TestServerConfig::unreachable(tmp).state()
}

async fn goal(state: &AppState, id: ulid::Ulid, arguments: &str) -> CommandOutcome {
    state
        .run_command(id, "goal", arguments)
        .await
        .expect("the session takes commands")
}

/// The data of a `/goal` that must succeed.
async fn ok(state: &AppState, id: ulid::Ulid, arguments: &str) -> Value {
    let out = goal(state, id, arguments).await;
    assert!(out.ok, "/goal {arguments}: {out:?}");
    out.data.expect("data")
}

/// The error of a `/goal` that must be refused.
async fn refused(state: &AppState, id: ulid::Ulid, arguments: &str) -> String {
    let out = goal(state, id, arguments).await;
    assert!(!out.ok, "/goal {arguments} should be refused: {out:?}");
    assert!(out.data.is_none());
    out.error.expect("error")
}

async fn goal_events(state: &AppState, id: ulid::Ulid) -> Vec<(String, Value)> {
    let mut events = stored_events(state, id, "goal.updated").await;
    events.extend(stored_events(state, id, "goal.cleared").await);
    events.sort_by_key(|e| e.seq);
    events.into_iter().map(|e| (e.kind, e.payload)).collect()
}

/// The goal `/goal` alone reports.
async fn shown(state: &AppState, id: ulid::Ulid) -> Value {
    ok(state, id, "").await["goal"].clone()
}

// Given a session without a goal
// When /goal runs alone, then with an objective, then with a criterion
// Then it first says there is none, then each change is one stored goal.updated carrying the
//      whole goal, and /goal shows the objective, revision, status and numbered criteria
#[tokio::test]
async fn goal_shows_none_then_sets_the_objective_and_adds_numbered_criteria() {
    let (tmp, ws) = tmp_ws();
    let state = idle_state(tmp.path());
    let id = state.create_session(&ws, None, false).await.unwrap();

    let none = goal(&state, id, "").await;
    assert!(none.ok);
    assert_eq!(none.data.as_ref().unwrap()["goal"], Value::Null);
    assert_eq!(none.render_text(), "no goal is set");

    let set = ok(&state, id, "  ship the   parser  ").await;
    assert_eq!(set["change"], "set");
    assert_eq!(
        set["goal"],
        json!({"revision": 1, "objective": "ship the parser", "criteria": [], "status": "active"})
    );
    let added = ok(&state, id, "criterion cargo test passes").await;
    assert_eq!(added["change"], "criterion");
    assert_eq!(added["goal"]["revision"], 2);
    ok(&state, id, "criterion the docs name the flag").await;

    let out = goal(&state, id, "").await;
    assert_eq!(
        out.data.as_ref().unwrap()["goal"],
        json!({"revision": 3, "objective": "ship the parser",
            "criteria": ["cargo test passes", "the docs name the flag"], "status": "active"})
    );
    assert_eq!(
        out.render_text(),
        "goal (revision 3, active): ship the parser\n1. cargo test passes\n2. the docs name the flag"
    );
    let events = goal_events(&state, id).await;
    assert_eq!(events.len(), 3, "a show publishes no goal event");
    assert_eq!(
        events[2],
        (
            "goal.updated".to_string(),
            json!({"revision": 3, "objective": "ship the parser",
                "criteria": ["cargo test passes", "the docs name the flag"],
                "status": "active", "change": "criterion"})
        )
    );
}

// Given a goal with one criterion
// When the objective is revised
// Then the revision goes up by one and the criteria are kept
#[tokio::test]
async fn revising_the_objective_bumps_the_revision_and_keeps_the_criteria() {
    let (tmp, ws) = tmp_ws();
    let state = idle_state(tmp.path());
    let id = state.create_session(&ws, None, false).await.unwrap();
    ok(&state, id, "first objective").await;
    ok(&state, id, "criterion tests pass").await;

    let revised = ok(&state, id, "second objective").await;
    assert_eq!(revised["change"], "revised");
    assert_eq!(
        revised["goal"],
        json!({"revision": 3, "objective": "second objective",
            "criteria": ["tests pass"], "status": "active"})
    );
}

// Given a goal
// When an objective, a criterion or a criteria count is over its bound
// Then the command is refused with the bound, nothing is stored, and the goal is unchanged; a
//      text exactly at the bound is accepted whole
#[tokio::test]
async fn a_bound_is_an_error_never_a_silent_cut() {
    let (tmp, ws) = tmp_ws();
    let state = idle_state(tmp.path());
    let id = state.create_session(&ws, None, false).await.unwrap();

    assert_eq!(
        refused(&state, id, &"o".repeat(MAX_OBJECTIVE_CHARS + 1)).await,
        format!(
            "the objective is {} characters long; the limit is {MAX_OBJECTIVE_CHARS}",
            MAX_OBJECTIVE_CHARS + 1
        )
    );
    assert_eq!(shown(&state, id).await, Value::Null);
    assert!(goal_events(&state, id).await.is_empty());

    let at_bound = "é".repeat(MAX_OBJECTIVE_CHARS);
    assert_eq!(
        ok(&state, id, &at_bound).await["goal"]["objective"],
        at_bound
    );
    let over = "c".repeat(MAX_CRITERION_CHARS + 1);
    assert_eq!(
        refused(&state, id, &format!("criterion {over}")).await,
        format!(
            "the criterion is {} characters long; the limit is {MAX_CRITERION_CHARS}",
            MAX_CRITERION_CHARS + 1
        )
    );
    ok(
        &state,
        id,
        &format!("criterion {}", "c".repeat(MAX_CRITERION_CHARS)),
    )
    .await;
    for n in 2..=MAX_CRITERIA {
        ok(&state, id, &format!("criterion number {n}")).await;
    }
    let before = shown(&state, id).await;
    assert_eq!(before["criteria"].as_array().unwrap().len(), MAX_CRITERIA);
    assert_eq!(
        refused(&state, id, "criterion one too many").await,
        format!("a goal holds at most {MAX_CRITERIA} criteria")
    );
    assert_eq!(
        shown(&state, id).await,
        before,
        "the refused changes left it as it was"
    );
    // The objective, then one event per criterion.
    assert_eq!(goal_events(&state, id).await.len(), 1 + MAX_CRITERIA);
}

// Given a session
// When a reserved first word comes with the wrong text, or the text is empty after cleaning
// Then the answer is a usage line, and a change that needs a goal says there is none
#[tokio::test]
async fn reserved_words_and_empty_text_answer_with_a_usage_line() {
    let (tmp, ws) = tmp_ws();
    let state = idle_state(tmp.path());
    let id = state.create_session(&ws, None, false).await.unwrap();

    for form in ["criterion tests pass", "pause", "resume", "clear"] {
        assert_eq!(refused(&state, id, form).await, NO_GOAL, "/goal {form}");
    }
    assert_eq!(refused(&state, id, "\x07\x1b").await, USAGE);
    ok(&state, id, "ship it").await;
    assert_eq!(
        refused(&state, id, "criterion").await,
        "usage: /goal criterion <text>"
    );
    assert_eq!(
        refused(&state, id, "criterion \x07").await,
        "usage: /goal criterion <text>"
    );
    assert_eq!(refused(&state, id, "pause now").await, "usage: /goal pause");
    assert_eq!(
        refused(&state, id, "resume it").await,
        "usage: /goal resume"
    );
    assert_eq!(refused(&state, id, "clear all").await, "usage: /goal clear");
    assert_eq!(goal_events(&state, id).await.len(), 1);
}

// Given a goal
// When it is paused, paused again, resumed, cleared, and a new goal is set
// Then pause and resume change only the status, a repeated one is refused, a clear stores
//      goal.cleared and leaves no goal, and the next goal keeps counting revisions
#[tokio::test]
async fn pause_resume_and_clear_change_the_status_and_revisions_keep_counting() {
    let (tmp, ws) = tmp_ws();
    let state = idle_state(tmp.path());
    let id = state.create_session(&ws, None, false).await.unwrap();
    ok(&state, id, "ship it").await;
    ok(&state, id, "criterion tests pass").await;

    let paused = ok(&state, id, "pause").await;
    assert_eq!(paused["change"], "paused");
    assert_eq!(paused["goal"]["status"], "paused");
    assert_eq!(paused["goal"]["revision"], 2, "a pause keeps the revision");
    assert_eq!(
        refused(&state, id, "pause").await,
        "the goal is already paused"
    );
    let resumed = ok(&state, id, "resume").await;
    assert_eq!(resumed["goal"]["status"], "active");
    assert_eq!(resumed["goal"]["revision"], 2);
    assert_eq!(
        refused(&state, id, "resume").await,
        "the goal is not paused"
    );

    let cleared = goal(&state, id, "clear").await;
    assert!(cleared.ok);
    assert_eq!(
        cleared.data.as_ref().unwrap(),
        &json!({"goal": null, "change": "cleared", "revision": 2})
    );
    assert_eq!(cleared.render_text(), "goal cleared (revision 2)");
    assert_eq!(shown(&state, id).await, Value::Null);
    assert_eq!(
        goal_events(&state, id).await.last().unwrap(),
        &("goal.cleared".to_string(), json!({"revision": 2}))
    );

    let again = ok(&state, id, "a new objective").await;
    assert_eq!(again["change"], "set");
    assert_eq!(again["goal"]["revision"], 3);
    assert_eq!(again["goal"]["criteria"], json!([]));
}

// Given a goal with a criterion, paused once and resumed
// When the session is stopped, the service restarted over the same history, and the session
//      resumed
// Then /goal and the ledger show the same goal, rebuilt from the stored events
#[tokio::test]
async fn the_goal_survives_a_stop_a_service_restart_and_a_resume() {
    let (tmp, ws) = tmp_ws();
    let id = {
        let state = idle_state(tmp.path());
        let id = state.create_session(&ws, None, false).await.unwrap();
        ok(&state, id, "ship the parser").await;
        ok(&state, id, "criterion tests pass").await;
        ok(&state, id, "pause").await;
        ok(&state, id, "resume").await;
        state.close_session(id).await.unwrap();
        id
    };
    let restarted = idle_state(tmp.path());
    restarted
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .expect("resumes");
    let expected = json!({"revision": 2, "objective": "ship the parser",
        "criteria": ["tests pass"], "status": "active"});
    assert_eq!(shown(&restarted, id).await, expected);
    let ledger = restarted.ledger(id).await.unwrap();
    assert_eq!(serde_json::to_value(&ledger.goal).unwrap(), expected);
    // The next change counts from the stored revision.
    assert_eq!(
        ok(&restarted, id, "criterion docs").await["goal"]["revision"],
        3
    );
}

fn request(provider: &CapturingProvider, n: usize) -> Value {
    serde_json::from_str(&provider.bodies()[n]).expect("request body")
}

/// The system messages of a request, in order.
fn system_messages(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| m["content"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The goal message of a request: the system message that starts with the goal label.
fn goal_block(request: &Value) -> Option<String> {
    system_messages(request)
        .into_iter()
        .find(|m| m.starts_with("[session goal"))
}

async fn turn(state: &AppState, id: ulid::Ulid, provider: &CapturingProvider, text: &str) {
    let before = provider.bodies().len();
    let sub = state.subscribe(id, None).await.unwrap();
    send_text(state, id, text).await;
    wait_for("the chat request", || provider.bodies().len() == before + 1).await;
    common::server::collect_until(sub, |e| e.kind == "turn.end").await;
}

// Given a goal with two criteria
// When a turn runs while it is active, then while it is paused, resumed, and cleared
// Then only the requests made while it is active carry the labelled goal block (objective,
//      numbered criteria, revision, the authority sentence) as the last system message before
//      the conversation, and no tool lets the model change it
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_carry_the_goal_block_only_while_it_is_active() {
    let (tmp, ws) = tmp_ws();
    let provider = spawn_capturing_provider(vec![
        answer("one"),
        answer("two"),
        answer("three"),
        answer("four"),
    ])
    .await;
    let state = TestServerConfig::mock(tmp.path(), &provider.url).state();
    let id = state.create_session(&ws, None, false).await.unwrap();
    ok(&state, id, "ship the parser").await;
    ok(&state, id, "criterion cargo test passes").await;
    ok(&state, id, "criterion the docs name the flag").await;

    turn(&state, id, &provider, "go").await;
    let first = request(&provider, 0);
    assert_eq!(
        goal_block(&first).as_deref(),
        Some(
            format!(
                "[session goal · revision 3]\nObjective: ship the parser\nSuccess criteria:\n\
                 1. cargo test passes\n2. the docs name the flag\n{AUTHORITY}"
            )
            .as_str()
        )
    );
    let messages = first["messages"].as_array().unwrap();
    let at = messages
        .iter()
        .position(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.starts_with("[session goal"))
        })
        .unwrap();
    assert_eq!(
        messages[at + 1]["role"],
        "user",
        "the goal comes right before the conversation"
    );
    assert!(messages[..at].iter().all(|m| m["role"] == "system"));
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter_map(|t| t["function"]["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(!tools.iter().any(|t| t.contains("goal")), "{tools:?}");

    ok(&state, id, "pause").await;
    turn(&state, id, &provider, "paused").await;
    assert_eq!(goal_block(&request(&provider, 1)), None);

    ok(&state, id, "resume").await;
    turn(&state, id, &provider, "resumed").await;
    assert!(goal_block(&request(&provider, 2)).is_some());

    ok(&state, id, "clear").await;
    turn(&state, id, &provider, "cleared").await;
    assert_eq!(goal_block(&request(&provider, 3)), None);
}

/// A window small enough that four long turns overrun its budget.
const WINDOW: u64 = 20_000;
const SUMMARY: &str = "Goal: finish the parser.\nNext: run the tests.";

// Given a long conversation with an active goal
// When it is compacted and the next turn runs
// Then the next request carries the summary and, outside it, the goal block whole
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_goal_block_survives_a_compaction() {
    let (tmp, ws) = tmp_ws();
    let provider = spawn_capturing_provider(vec![answer(SUMMARY), answer("on it")]).await;
    let state = TestServerConfig::mock(tmp.path(), &provider.url)
        .chat_tools(WINDOW)
        .state();
    let id = state.create_session(&ws, None, false).await.unwrap();
    state.close_session(id).await.unwrap();
    let history: Vec<ChatMessage> = (0..4)
        .flat_map(|n| {
            [
                ChatMessage::User {
                    provenance: Default::default(),
                    content: format!("request {n}"),
                    images: Vec::new(),
                },
                ChatMessage::Assistant {
                    content: Some(format!("answer {n} {}", "word ".repeat(1200))),
                    reasoning_content: None,
                    tool_calls: vec![],
                    opaque: None,
                },
            ]
        })
        .collect();
    state.history().save_transcript(id, history).await.unwrap();
    state
        .create_or_resume_session(&ws, None, false, Some(id))
        .await
        .unwrap();
    ok(&state, id, "finish the parser").await;
    ok(&state, id, "criterion every test passes").await;

    let out = state.run_command(id, "compact", "").await.unwrap();
    assert!(out.ok, "{out:?}");
    send_text(&state, id, "continue").await;
    wait_for("the chat request", || provider.bodies().len() == 2).await;
    let next = request(&provider, 1);
    let block = goal_block(&next).expect("the goal block");
    assert!(block.contains("Objective: finish the parser"));
    assert!(block.contains("1. every test passes"));
    assert!(block.ends_with(AUTHORITY));
    let texts: String = next["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(texts.contains(SUMMARY) && !texts.contains("request 0"));
}

async fn breakdown(state: &AppState, id: ulid::Ulid) -> Value {
    serde_json::to_value(state.context(id).await.unwrap()).unwrap()
}

// Given a session
// When a goal is set, paused and resumed
// Then the context gauge counts the goal message in its own category while it is active (the
//      total includes it), publishes the change, and drops the category when it is paused
#[tokio::test]
async fn the_context_breakdown_counts_the_goal_while_it_is_active() {
    let (tmp, ws) = tmp_ws();
    let state = idle_state(tmp.path());
    let id = state.create_session(&ws, None, false).await.unwrap();
    let before = breakdown(&state, id).await;
    assert!(before["breakdown"].get("goal").is_none(), "{before}");

    let sub = state.subscribe(id, None).await.unwrap();
    ok(&state, id, "ship the parser").await;
    let live = common::server::collect_until(sub, |e| e.kind == "context.updated").await;
    let update = live.iter().rfind(|e| e.kind == "context.updated").unwrap();
    let goal_tokens = update.payload["breakdown"]["goal"]
        .as_u64()
        .expect("goal category");
    assert!(goal_tokens > 0);
    let after = breakdown(&state, id).await;
    assert_eq!(after["breakdown"]["goal"].as_u64(), Some(goal_tokens));
    assert_eq!(
        after["estimate_total"].as_u64().unwrap(),
        before["estimate_total"].as_u64().unwrap() + goal_tokens
    );
    let sum: u64 = [
        "system",
        "rules",
        "skills_catalog",
        "tools",
        "messages",
        "goal",
    ]
    .iter()
    .map(|k| after["breakdown"][k].as_u64().unwrap_or(0))
    .sum();
    assert_eq!(sum, after["estimate_total"].as_u64().unwrap());

    ok(&state, id, "pause").await;
    let paused = breakdown(&state, id).await;
    assert!(paused["breakdown"].get("goal").is_none());
    assert_eq!(paused["estimate_total"], before["estimate_total"]);
    ok(&state, id, "resume").await;
    assert_eq!(
        breakdown(&state, id).await["breakdown"]["goal"].as_u64(),
        Some(goal_tokens)
    );
}
