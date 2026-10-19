//! Phase-1 golden wire-body lock: the provider request bodies must not drift.
//!
//! Drives `run_agent` against a scripted mock provider (same technique as the
//! Phase-1 loop tests) and locks the raw outgoing provider request bodies.
//! Requires checked-in fixtures and asserts byte-equality, so a
//! Phase-2 seam that changes Phase-1 request building FAILS here. Test-only:
//! no Phase-1 logic is touched.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;
use withnav::agent::actor::AgentActor;
use withnav::agent::run_loop::{AgentEvent, RunOutcome, run_agent};
use withnav::provider::config::{ProviderConfig, ReasoningHistory};
use withnav::tools::bash::BashTool;
use withnav::tools::control::AskTool;
use withnav::tools::native::ReadTool;
use withnav::tools::{Registry, ToolContext};

mod common;
use common::sse::{http_sse, spawn_capturing_provider, sse_body};

const FIXTURE_DIR: &str = "tests/fixtures/phase1_wire";

fn registry_all() -> Registry {
    let mut r = Registry::new();
    r.register(Box::new(ReadTool));
    r.register(Box::new(BashTool));
    r.register(Box::new(AskTool));
    r
}

struct TestActor {
    actor: AgentActor,
    _tmp: tempfile::TempDir,
}

async fn make_actor(base_url: String, registry: Registry) -> TestActor {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    let actor = AgentActor::new(
        registry,
        ToolContext {
            binding: None,
            workspace: ws.clone(),
            writable_roots: vec![ws],
            readable_roots: Vec::new(),
        },
        ProviderConfig {
            base_url,
            api_key_env: None,
            headers: BTreeMap::new(),
            connect_timeout_ms: 2000,
            request_timeout_ms: 600_000,
            catalog_format: None,
        },
        "test-key".to_string(),
        "test-model".to_string(),
        ReasoningHistory::Preserve,
        Some(256),
        CancellationToken::new(),
    );
    // The wire is the subject here, not permissions: every call may run.
    actor
        .permission_store
        .as_ref()
        .unwrap()
        .add(
            "*",
            "*",
            withnav::permission::rules::Effect::Allow,
            withnav::permission::rules::RuleScope::Global,
            withnav::permission::rules::RuleOrigin::Manual,
        )
        .unwrap();
    TestActor { actor, _tmp: tmp }
}

/// Assert byte-equality against required checked-in raw wire bodies.
///
/// `bodies` are the exact request-body strings the provider received; the
/// fixture file stores them as a pretty-printed JSON array of raw strings so
/// the wire bytes are preserved verbatim (serde string escaping is stable).
fn lock_fixture(name: &str, bodies: &[String]) {
    assert!(!bodies.is_empty(), "no wire bodies captured for {name}");
    for body in bodies {
        let parsed: serde_json::Value =
            serde_json::from_str(body).expect("captured body must be valid JSON");
        assert!(parsed.is_object(), "captured body must be a JSON object");
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    let path = dir.join(format!("{name}.json"));
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("required golden fixture {}: {e}", path.display()));
    let locked: Vec<String> = serde_json::from_slice(&bytes).unwrap();
    let expected: Vec<String> = locked.iter().map(|body| expected_wire(body)).collect();
    assert_eq!(
        expected.as_slice(),
        bodies,
        "wire bodies drifted from locked fixture {name}"
    );
}

/// The bytes a locked fixture body is expected to match. The built-in client writes
/// fields in declaration order. The genai transport writes the same JSON value with
/// object keys sorted. Its expected bytes are the fixture's value re-serialized, so
/// the fixture stays the reference and any other drift still fails.
fn expected_wire(locked: &str) -> String {
    if withnav::provider::client::GENAI_CHAT {
        let value: serde_json::Value = serde_json::from_str(locked).expect("fixture is JSON");
        serde_json::to_string(&value).unwrap()
    } else {
        locked.to_string()
    }
}

async fn run_once(actor: &mut AgentActor) -> RunOutcome {
    let mut noop = |_e: AgentEvent| {};
    run_agent(actor, &mut noop).await.unwrap()
}

#[tokio::test]
async fn golden_no_tool_completion() {
    let done = r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
    let server = spawn_capturing_provider(vec![http_sse(&sse_body(&[done]))]).await;

    let mut t = make_actor(server.url.clone(), Registry::new()).await;
    t.actor.push_user("hello");
    let outcome = run_once(&mut t.actor).await;
    assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "all done"));

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 1);
    lock_fixture("no_tool_completion", &bodies);
}

#[tokio::test]
async fn golden_tool_continuation_args_and_result_ordering() {
    let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let done = r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
    let server = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[toolcall])),
        http_sse(&sse_body(&[done])),
    ])
    .await;

    let mut t = make_actor(server.url.clone(), registry_all()).await;
    t.actor.push_user("run echo hi");
    let outcome = run_once(&mut t.actor).await;
    assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "all done"));

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[1].contains("\"tool_call_id\":\"call_1\""));
    lock_fixture("tool_continuation", &bodies);
}

#[tokio::test]
async fn golden_unicode_and_large_integer_values() {
    let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_u1","function":{"name":"bash","arguments":"{\"command\":\"echo 世界 ✓\",\"timeout\":9007199254740993}"}}]},"finish_reason":"tool_calls"}]}"#;
    let done =
        r#"{"choices":[{"index":0,"delta":{"content":"世界 ✓ 完成"},"finish_reason":"stop"}]}"#;
    let server = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[toolcall])),
        http_sse(&sse_body(&[done])),
    ])
    .await;

    let mut t = make_actor(server.url.clone(), registry_all()).await;
    t.actor.push_user("echo 世界 — héllo ✓");
    let outcome = run_once(&mut t.actor).await;
    assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "世界 ✓ 完成"));

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("héllo ✓"));
    assert!(bodies[1].contains("9007199254740993"));
    assert!(bodies[1].contains("世界 ✓"));
    lock_fixture("unicode_and_bigint", &bodies);
}

#[tokio::test]
async fn golden_deepseek_reasoning_content_replay() {
    let toolcall = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"step 1: plan 抉择","tool_calls":[{"index":0,"id":"call_r1","function":{"name":"bash","arguments":"{\"command\":\"echo planned\"}"}}]},"finish_reason":"tool_calls"}]}"#;
    let done = r#"{"choices":[{"index":0,"delta":{"content":"reasoning preserved"},"finish_reason":"stop"}]}"#;
    let server = spawn_capturing_provider(vec![
        http_sse(&sse_body(&[toolcall])),
        http_sse(&sse_body(&[done])),
    ])
    .await;

    let mut t = make_actor(server.url.clone(), registry_all()).await;
    t.actor.push_user("plan then act");
    let outcome = run_once(&mut t.actor).await;
    assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "reasoning preserved"));

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies[1].contains("\"reasoning_content\":\"step 1: plan 抉择\""),
        "Preserve policy must replay reasoning_content in the continuation"
    );
    lock_fixture("reasoning_content_replay", &bodies);
}
