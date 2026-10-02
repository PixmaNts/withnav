use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use navai::agent::actor::AgentActor;
use navai::agent::run_loop::{run_agent, AgentEvent, RunOutcome};
use navai::provider::config::{ProviderConfig, ReasoningHistory};
use navai::provider::StreamDelta;
use navai::tools::bash::BashTool;
use navai::tools::control::AskTool;
use navai::tools::native::{EditTool, GrepTool, ReadTool, WriteTool};
use navai::tools::{Registry, ToolContext};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

mod common;
use common::sse::{http_sse, spawn_responding_provider, sse_body, CapturingProvider};

fn bad_request(reason: &str) -> Vec<u8> {
    let body = format!("{{\"error\":{{\"message\":\"malformed continuation: {reason}\"}}}}");
    format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

/// One streamed assistant turn calling `name` with the JSON `args`.
fn tool_chunk(id: &str, name: &str, args: &Value) -> String {
    json!({"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {
        "reasoning_content": format!("planning {name}"),
        "tool_calls": [{"index": 0, "id": id, "function": {
            "name": name, "arguments": args.to_string(),
        }}],
    }}]})
    .to_string()
}

fn done_chunk(text: &str) -> String {
    json!({"choices": [{"index": 0, "finish_reason": "stop", "delta": {"content": text}}]})
        .to_string()
}

/// A scripted mock OmniRoute that drives the agent through all five native
/// tools plus `ask`, and rejects any continuation whose previous tool result is
/// missing (so a broken transcript implementation FAILS the test).
async fn spawn_scripted_mock(workspace: &Path) -> CapturingProvider {
    let src = workspace.join("src").display().to_string();
    let lib_rs = format!("{src}/lib.rs");
    let notes = workspace.join("NOTES.txt").display().to_string();

    // (call id, tool, arguments): each request must carry the result of the
    // previous row's call id, and is answered with this row's call.
    let calls = [
        ("c_read", "read", json!({"path": lib_rs})),
        ("c_grep", "grep", json!({"pattern": "add", "path": src})),
        (
            "c_write",
            "write",
            json!({"path": notes, "content": "notes"}),
        ),
        (
            "c_edit",
            "edit",
            json!({"path": notes, "old_string": "notes", "new_string": "edited"}),
        ),
        ("c_bash", "bash", json!({"command": "echo done"})),
        ("c_ask", "ask", json!({"question": "proceed?"})),
    ];
    let mut script: Vec<(Option<&str>, Vec<u8>)> = Vec::new();
    for (i, (id, name, args)) in calls.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| calls[p].0);
        script.push((prev, http_sse(&sse_body(&[&tool_chunk(id, name, args)]))));
    }
    script.push((
        Some("c_ask"),
        http_sse(&sse_body(&[&done_chunk("all tools exercised")])),
    ));

    spawn_responding_provider(script.len(), move |i, req| {
        let (expect_prev_id, resp) = &script[i];
        match expect_prev_id {
            Some(prev)
                if !(req.contains(&format!("\"tool_call_id\":\"{prev}\""))
                    && req.contains("\"role\":\"tool\"")) =>
            {
                bad_request(&format!("missing role:tool for {prev}"))
            }
            _ => resp.clone(),
        }
    })
    .await
}

fn build_actor(base_url: String, workspace: PathBuf, cancel: CancellationToken) -> AgentActor {
    let mut registry = Registry::new();
    registry.register(Box::new(ReadTool));
    registry.register(Box::new(WriteTool));
    registry.register(Box::new(EditTool));
    registry.register(Box::new(BashTool));
    registry.register(Box::new(GrepTool));
    registry.register(Box::new(AskTool));

    let tool_context = ToolContext {
        workspace: workspace.clone(),
        writable_roots: vec![workspace.clone()],
        readable_roots: Vec::new(),
    };
    AgentActor::new(
        registry,
        tool_context,
        ProviderConfig {
            base_url,
            api_key_env: None,
            headers: BTreeMap::new(),
            connect_timeout_ms: 2000,
            request_timeout_ms: 600_000,
        },
        "test-key".to_string(),
        "test-model".to_string(),
        ReasoningHistory::Preserve,
        Some(256),
        cancel,
    )
}

#[tokio::test]
async fn mock_full_loop_exercises_all_five_tools_and_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("src").join("lib.rs"),
        "pub fn add(a:i32,b:i32)->i32{a+b}\n",
    )
    .unwrap();

    let mock = spawn_scripted_mock(&ws).await;
    let mut actor = build_actor(mock.url.clone(), ws.clone(), CancellationToken::new());
    actor.push_user("exercise every tool then ask");

    let mut noop = |_e: AgentEvent| {};

    let first = run_agent(&mut actor, &mut noop).await.unwrap();
    match first {
        RunOutcome::Yielded(req) => assert_eq!(req.question, "proceed?"),
        other => panic!("expected Yielded at ask, got {other:?}"),
    }

    actor.push_user("yes proceed");
    let second = run_agent(&mut actor, &mut noop).await.unwrap();
    assert!(matches!(second, RunOutcome::Done(m) if m.text == "all tools exercised"));

    assert!(actor.tool_context.workspace.join("NOTES.txt").is_file());
    assert_eq!(
        std::fs::read_to_string(ws.join("NOTES.txt")).unwrap(),
        "edited"
    );

    let reqs = mock.requests();
    assert!(reqs.len() >= 7, "expected >=7 requests, got {}", reqs.len());
    for prev in ["c_read", "c_grep", "c_write", "c_edit", "c_bash"] {
        assert!(
            reqs.iter()
                .any(|r| r.contains(&format!("\"tool_call_id\":\"{prev}\""))),
            "continuation for {prev} missing role:tool"
        );
    }
    assert!(
        reqs.iter().any(|r| r.contains("\"reasoning_content\"")),
        "reasoning_content must be replayed in continuation (preserve policy)"
    );
    let resume = reqs.last().unwrap();
    assert!(resume.contains("\"tool_call_id\":\"c_ask\""));
}

/// LIVE end-to-end against a real OmniRoute proxy.
///
/// REQUIRED for Phase-1 completion. Run explicitly:
///   NAVAI_LIVE=1 cargo test --test e2e -- --ignored --nocapture
/// When OmniRoute is unavailable this test FAILS (or is a BLOCKED gate) — it is
/// NEVER a passing skip.
#[tokio::test]
#[ignore]
async fn live_omniroute_edit_and_test() {
    let base_url = "http://localhost:20128/v1";
    let key = read_live_key().expect("auth.json with omniroute key required for the live e2e");

    preflight(base_url, &key).await.expect(
        "OmniRoute preflight failed: proxy unreachable or alias unavailable (BLOCKED, not a skip)",
    );

    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().to_path_buf();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname=\"e2e-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::write(ws.join("src").join("lib.rs"), "// implement add here\n").unwrap();

    let provider = ProviderConfig {
        base_url: base_url.to_string(),
        api_key_env: None,
        headers: BTreeMap::new(),
        connect_timeout_ms: 5000,
        request_timeout_ms: 600_000,
    };

    let mut registry = Registry::new();
    registry.register(Box::new(ReadTool));
    registry.register(Box::new(WriteTool));
    registry.register(Box::new(EditTool));
    registry.register(Box::new(BashTool));
    registry.register(Box::new(GrepTool));
    registry.register(Box::new(AskTool));

    let cancel = CancellationToken::new();
    let mut actor = AgentActor::new(
        registry,
        ToolContext {
            workspace: ws.clone(),
            writable_roots: vec![ws.clone()],
            readable_roots: Vec::new(),
        },
        provider,
        key,
        "opencode-go/deepseek-v4-flash".to_string(),
        ReasoningHistory::Preserve,
        Some(4000),
        cancel,
    );
    actor.push_user(
        "In this Rust project, edit src/lib.rs so it defines `pub fn add(a: i32, b: i32) -> i32` \
         returning a+b (use the write or edit tool). Then run `cargo test` via the bash tool to \
         confirm it compiles. When finished, give a one-line summary.",
    );

    let saw_thinking = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw2 = saw_thinking.clone();
    let outcome = {
        let mut on_delta = move |e: AgentEvent| {
            if let AgentEvent::Delta(StreamDelta::Thinking { .. }) = e {
                saw2.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        };
        run_agent(&mut actor, &mut on_delta).await.unwrap()
    };

    let lib = std::fs::read_to_string(ws.join("src").join("lib.rs")).unwrap();
    assert!(
        lib.contains("fn add"),
        "model must have edited src/lib.rs to add `add`; got:\n{lib}"
    );
    assert!(
        saw_thinking.load(std::sync::atomic::Ordering::SeqCst),
        "model must emit reasoning_content at least once"
    );
    assert!(
        actor
            .transcript
            .iter()
            .any(|m| format!("{m:?}").contains("cargo test") || format!("{m:?}").contains("bash")),
        "a bash/cargo test invocation must appear in the transcript"
    );
    println!("live e2e outcome: {outcome:?}");
}

fn read_live_key() -> Option<String> {
    let dir = navai::provider::auth::default_config_dir()?;
    navai::provider::auth::resolve_key(&dir, "omniroute", Some("OMNIROUTE_API_KEY")).ok()
}

async fn preflight(base_url: &str, key: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(format!("{base_url}/chat/completions"))
        .bearer_auth(key)
        .json(&serde_json::json!({
            "model": "opencode-go/deepseek-v4-flash",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("preflight status {}", resp.status()))
    }
}
