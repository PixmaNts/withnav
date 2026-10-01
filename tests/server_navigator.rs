//! The navigator profile: creating a session with `profile: "navigator"`
//! resolves the `cognition` alias and sends the cognition role prompt as a
//! SECOND system message after the agent loop's own contract — observed on
//! the wire as the request the provider receives (wiremock).
//!
//! Also the conversation-feed seam: a feed shared with the AppState means
//! a navigator session registers itself, driver events build the digest,
//! and a wake posted through the feed lands as a navigator-sourced
//! `message.received` that drives a real turn — queued behind a pending
//! `ask` when the cognition is yielded, delivered right after the driver's
//! answer.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer};

use rust_agent::server::AppState;

mod common;
use common::server::{collect_until, send_text, wait_for, TestServerConfig};
use common::sse::sse_response;

fn test_state(tmp: &Path, base_url: &str) -> AppState {
    test_state_with_feed(tmp, base_url, None)
}

fn test_state_with_feed(
    tmp: &Path,
    base_url: &str,
    conversation: Option<rust_agent::navigator::ConversationFeed>,
) -> AppState {
    TestServerConfig::mock(tmp, base_url)
        .model_alias("cognition", "mock-opus")
        .navigator_state_with_conversation(conversation)
}

// Given a session created with profile "navigator" against a scripted
// wiremock provider
// When a message is posted and the turn completes
// Then the request the provider received carries TWO system messages — the
//      agent loop's own contract FIRST, COGNITION_PROMPT SECOND — before
//      the user message; the session resolved the `cognition` alias; and the
//      `intent` tool is offered (no conversation feed is involved).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn navigator_profile_sends_the_cognition_prompt_as_second_system_message() {
    let server = MockServer::start().await;
    let done =
        r#"{"choices":[{"index":0,"delta":{"content":"guide answer"},"finish_reason":"stop"}]}"#;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(sse_response(&[done]))
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let state = test_state(&root, &server.uri());
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws dir");

    let id = state
        .create_session(&ws, None, Some("navigator"))
        .await
        .expect("create navigator session");
    assert_eq!(
        state.session_summary(id).await.expect("summary").model,
        "cognition",
        "the navigator profile resolves the cognition alias"
    );
    let sub = state.subscribe(id, None).await.expect("subscribe");

    // Post the message through the same route core POST /messages uses.
    let ack = send_text(&state, id, "hello there").await;
    assert!(ack.accepted);

    let events = collect_until(sub, |e| e.kind == "turn.end").await;
    assert!(
        events
            .iter()
            .any(|e| e.kind == "turn.end" && e.payload["outcome"] == "done"),
        "the scripted turn completed: {:?}",
        events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>()
    );

    // The wire request: the cognition prompt rides as the SECOND system
    // message, behind the agent loop's own contract, ahead of the user turn.
    let requests = server.received_requests().await;
    let requests = requests.expect("wiremock retains requests");
    assert_eq!(requests.len(), 1, "exactly one provider call");
    let body: Value =
        serde_json::from_slice(&requests[0].body).expect("request body is the chat request");
    assert_eq!(body["model"], "mock-opus");
    let messages = body["messages"].as_array().expect("messages array");
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap_or(""))
        .collect();
    let system_count = roles.iter().filter(|r| **r == "system").count();
    assert_eq!(
        system_count, 2,
        "two system messages (loop contract + cognition role): {roles:?}"
    );
    assert_eq!(messages[0]["role"], "system", "loop contract first");
    assert_eq!(messages[1]["role"], "system", "cognition prompt second");
    assert_eq!(
        messages[1]["content"],
        rust_agent::navigator::COGNITION_PROMPT,
        "the second system message IS the cognition prompt"
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], "hello there");
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(tools.contains(&"intent"), "intent tool offered: {tools:?}");
}

// Given a navigator session created over an AppState sharing a
// ConversationFeed with a scripted wiremock cognition
// When a wake is posted through the feed
// Then the active conversation is the session, the wake lands as
//      `message.received` with source `navigator`, and a real turn runs
//      whose provider request carries the wake text as the user message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_through_the_feed_lands_as_a_navigator_sourced_message_and_turn() {
    let server = MockServer::start().await;
    let done = r#"{"choices":[{"index":0,"delta":{"content":"noted, let's discuss"},"finish_reason":"stop"}]}"#;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(sse_response(&[done]))
        .expect(2)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let feed = rust_agent::navigator::ConversationFeed::new();
    let state = test_state_with_feed(&root, &server.uri(), Some(feed.clone()));
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws dir");

    let id = state
        .create_session(&ws, None, Some("navigator"))
        .await
        .expect("create navigator session");

    // No driver message yet: the session is not the active conversation.
    assert_eq!(feed.active_session(), None);
    assert_eq!(
        feed.post_wake("wake: drift?").await,
        rust_agent::navigator::WakeOutcome::NoSession
    );

    let sub = state.subscribe(id, None).await.expect("subscribe");

    send_text(&state, id, "add an auth remove command").await;
    // The subscriber task needs a beat to observe the driver message.
    wait_for("the driver message to activate the session", || {
        feed.active_session() == Some(id)
    })
    .await;
    assert_eq!(feed.active_session(), Some(id), "driver message activates");

    let outcome = feed
        .post_wake("wake: this edit drifts from the intent")
        .await;
    assert_eq!(
        outcome,
        rust_agent::navigator::WakeOutcome::Posted(id),
        "no pending ask: the wake posts immediately"
    );

    // The wake drives a REAL turn that ends done, and the durable event
    // carries the navigator as its source.
    let events = collect_until(sub, |e| {
        e.kind == "turn.end" && e.payload["outcome"] == "done"
    })
    .await;
    let wake_received = events
        .iter()
        .find(|e| e.kind == "message.received" && e.payload["source"] == "navigator")
        .expect("navigator-sourced message.received");
    assert!(wake_received.payload["content"]
        .as_str()
        .expect("content")
        .contains("drifts from the intent"));
    // Turn order: the wake message precedes ITS turn (the one starting
    // after the wake's seq, not the driver's earlier turn).
    let wake_pos = events
        .iter()
        .position(|e| e.seq == wake_received.seq)
        .expect("position");
    let turn_pos = events
        .iter()
        .position(|e| e.kind == "turn.start" && e.seq > wake_received.seq)
        .expect("turn.start after the wake");
    assert!(wake_pos < turn_pos, "message.received before turn.start");

    // The provider saw the wake as the second turn's user message.
    let requests = server.received_requests().await.expect("wiremock retains");
    let body: Value = serde_json::from_slice(&requests[1].body).expect("second request body");
    let last_user = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .expect("a user message");
    assert!(last_user["content"]
        .as_str()
        .expect("content")
        .contains("drifts from the intent"));

    // The feed digest now holds both sides, navigator included.
    wait_for("the digest to hold the wake", || {
        feed.digest().contains("navigator: wake:")
    })
    .await;
    let digest = feed.digest();
    assert!(
        digest.contains("driver: add an auth remove command"),
        "{digest}"
    );
    assert!(digest.contains("navigator: wake:"), "{digest}");
}

// Given the same seam with a cognition that yields on `ask`
// When a wake is posted while the cognition waits on the driver
// Then the wake is queued (not posted), the driver's next message answers
//      the ask as the TOOL result (the provider sees role "tool"), and the
//      queued wake arrives after it as a navigator-sourced user message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_while_the_cognition_yields_is_queued_behind_the_ask_answer() {
    let server = MockServer::start().await;
    // Turn 1: the cognition asks a question (tool call `ask`). Turn 2 (the
    // resumed turn after the driver's answer) and turn 3 (the queued wake)
    // finish. Each mount matches its turn by what the transcript already
    // contains; wiremock prefers the LATEST matching mount, so they are
    // registered least-specific first.
    let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"remove which scope?\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let done = r#"{"choices":[{"index":0,"delta":{"content":"done after the answer"},"finish_reason":"stop"}]}"#;
    let done2 = r#"{"choices":[{"index":0,"delta":{"content":"done after the wake"},"finish_reason":"stop"}]}"#;
    // Transcripts accumulate, so the turn-2 and turn-3 bodies also contain
    // the earlier markers: the later mounts need HIGHER priority to win
    // over the earlier ones (same priority would fall back to insertion
    // order and the ask script would answer every turn).
    for (body, marker, priority) in [
        (ask, "add an auth remove command", 5u8),
        (done, "the local scope", 4),
        (done2, "latest wake", 3),
    ] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .and(wiremock::matchers::body_string_contains(marker.to_string()))
            .respond_with(sse_response(&[body]))
            .with_priority(priority)
            .mount(&server)
            .await;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let feed = rust_agent::navigator::ConversationFeed::new();
    let state = test_state_with_feed(&root, &server.uri(), Some(feed.clone()));
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws dir");

    let id = state
        .create_session(&ws, None, Some("navigator"))
        .await
        .expect("create");

    // Turn 1: the cognition yields on ask.
    // Subscribe before sending: `collect_until` matches live events only,
    // and a fast turn would otherwise be over (in the replay) before the
    // subscription existed.
    let sub = subscribe(&state, id).await;
    send_text(&state, id, "add an auth remove command").await;
    let yielded = collect_until(sub, |e| {
        e.kind == "agent.state" && e.payload["to"] == "yielded"
    })
    .await;
    assert!(
        yielded.iter().any(|e| e.kind == "agent.yield"),
        "the cognition asked: {:?}",
        yielded.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>()
    );
    wait_for("the feed to see agent.yield", || {
        feed.is_waiting_on_driver()
    })
    .await;

    // The wake while yielded: queued, coalesced, never the ask answer.
    assert_eq!(
        feed.post_wake("first wake").await,
        rust_agent::navigator::WakeOutcome::Queued(id)
    );
    assert_eq!(
        feed.post_wake("latest wake").await,
        rust_agent::navigator::WakeOutcome::Queued(id)
    );

    // The driver answers the ask. Their message must become the TOOL
    // result; only then does the queued wake arrive, as a user message.
    let sub = subscribe(&state, id).await;
    send_text(&state, id, "the local scope").await;
    let events = collect_until(sub, |e| {
        e.kind == "message.received"
            && e.payload["source"] == "navigator"
            && e.payload["content"]
                .as_str()
                .is_some_and(|c| c.contains("latest wake"))
    })
    .await;
    let sources: Vec<(String, String)> = events
        .iter()
        .filter(|e| e.kind == "message.received")
        .map(|e| {
            (
                e.payload["source"].as_str().unwrap_or("").to_string(),
                e.payload["content"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let answer_pos = sources
        .iter()
        .position(|(s, c)| s == "user:http" && c.contains("the local scope"))
        .expect("the driver's answer was received");
    let wake_pos = sources
        .iter()
        .position(|(s, c)| s == "navigator" && c.contains("latest wake"))
        .expect("the queued wake arrived after the answer");
    assert!(
        answer_pos < wake_pos,
        "driver answer before the wake: {sources:?}"
    );
    assert!(
        !sources
            .iter()
            .any(|(s, c)| s == "navigator" && c.contains("first wake")),
        "coalesced: only the latest wake is delivered"
    );

    // The driver's answer must reach the provider as the ask's TOOL result,
    // and the wake as a later USER message, never the other way round. The
    // wake is flushed as soon as the answer is accepted, so it either rides
    // the same inbox batch as the answer (one resumed turn) or starts a turn
    // of its own; both keep the guard. Wait for the request that carries the
    // wake rather than for a request count (observed 2026-10-01 under load:
    // only two provider requests arrived, so waiting for a third timed out).
    let requests = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let requests = server.received_requests().await.expect("wiremock retains");
            if requests
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains("latest wake"))
            {
                break requests;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a provider request carrying the queued wake should arrive");
    let wake_request = requests
        .iter()
        .find(|r| String::from_utf8_lossy(&r.body).contains("latest wake"))
        .expect("the request carrying the wake");
    let body: Value = serde_json::from_slice(&wake_request.body).expect("json body");
    let messages = body["messages"].as_array().expect("messages");
    let text = |m: &Value| m["content"].as_str().unwrap_or("").to_string();
    let answer_at = messages
        .iter()
        .position(|m| {
            m["role"] == "tool"
                && m["tool_call_id"] == "ask_1"
                && text(m).contains("the local scope")
        })
        .expect("the driver's answer became the ask tool result");
    let wake_at = messages
        .iter()
        .position(|m| m["role"] == "user" && text(m).contains("latest wake"))
        .expect("the wake as a user message");
    assert!(
        answer_at < wake_at,
        "the answer precedes the wake: {messages:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|m| m["role"] == "user" && text(m).contains("the local scope")),
        "the answer is never a plain user message"
    );
    assert!(
        !messages.iter().any(|m| text(m).contains("first wake")),
        "coalesced: only the latest wake reaches the provider"
    );
}

/// A fresh subscription: replay (the whole retained history) then live, in
/// the same shape `collect_until` drains.
async fn subscribe(state: &AppState, id: ulid::Ulid) -> rust_agent::session::Subscription {
    state.subscribe(id, None).await.expect("subscribe")
}

// Given a navigator conversation with one completed driver turn, stopped
// When it is resumed under a feed that never saw it
// Then the feed's digest holds that turn (driver and cognition), and no
//      `TurnEnded` is announced for it: replay rebuilds, it does not announce
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_feed_rebuilds_without_announcing_old_turns() {
    let server = MockServer::start().await;
    let done = r#"{"choices":[{"index":0,"delta":{"content":"agreed, scope it locally"},"finish_reason":"stop"}]}"#;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_response(&[done]))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("ws dir");
    let state = TestServerConfig::mock(&root, &server.uri())
        .model_alias("cognition", "mock-opus")
        .history_db(root.join("sessions.db"))
        .navigator_state_with_conversation(Some(rust_agent::navigator::ConversationFeed::new()));

    let id = state
        .create_session(&ws, None, Some("navigator"))
        .await
        .expect("create navigator session");
    let sub = state.subscribe(id, None).await.expect("subscribe");
    send_text(&state, id, "scope the auth removal").await;
    collect_until(sub, |e| e.kind == "turn.end").await;
    state.close_session(id).await.expect("close");

    // A second state over the same database: its feed has never seen the
    // conversation.
    let feed = rust_agent::navigator::ConversationFeed::new();
    let resumed_state = TestServerConfig::mock(&root, &server.uri())
        .model_alias("cognition", "mock-opus")
        .history_db(root.join("sessions.db"))
        .navigator_state_with_conversation(Some(feed.clone()));
    let mut announced = feed.subscribe();
    let (resumed_id, resumed) = resumed_state
        .create_or_resume_session(&ws, None, None, Some(id))
        .await
        .expect("resume");
    assert_eq!((resumed_id, resumed), (id, true));

    let mut digest = String::new();
    for _ in 0..40 {
        digest = feed.digest();
        if digest.contains("agreed, scope it locally") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        digest.contains("driver: scope the auth removal"),
        "{digest}"
    );
    assert!(
        digest.contains("cognition: agreed, scope it locally"),
        "{digest}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        announced.try_recv().is_err(),
        "the replayed turn is not announced"
    );
}
