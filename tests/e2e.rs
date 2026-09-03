use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rust_agent::agent::actor::AgentActor;
use rust_agent::agent::run_loop::{run_agent, RunOutcome};
use rust_agent::provider::config::{ProviderConfig, ReasoningHistory};
use rust_agent::provider::StreamDelta;
use rust_agent::tools::bash::BashTool;
use rust_agent::tools::control::AskTool;
use rust_agent::tools::native::{EditTool, GrepTool, ReadTool, WriteTool};
use rust_agent::tools::{Registry, ToolContext};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

fn sse(events: &[&str]) -> String {
    let mut b = String::new();
    for e in events {
        b.push_str("data: ");
        b.push_str(e);
        b.push_str("\n\n");
    }
    b.push_str("data: [DONE]\n\n");
    b
}

fn ok_response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

fn bad_request(reason: &str) -> Vec<u8> {
    let body = format!("{{\"error\":{{\"message\":\"malformed continuation: {reason}\"}}}}");
    format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes()
}

fn tool_chunk(id: &str, name: &str, args: &str) -> String {
    format!(
        r#"{{"choices":[{{"index":0,"delta":{{"reasoning_content":"planning {name}","tool_calls":[{{"index":0,"id":"{id}","function":{{"name":"{name}","arguments":{args}}}}}]}},"finish_reason":"tool_calls"}}]}}"#
    )
}

fn done_chunk(text: &str) -> String {
    format!(
        r#"{{"choices":[{{"index":0,"delta":{{"content":"{text}"}},"finish_reason":"stop"}}]}}"#
    )
}

async fn read_http(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = sock.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let s = String::from_utf8_lossy(&buf);
        if let Some(idx) = s.find("\r\n\r\n") {
            let header = &s[..idx];
            let clen = header
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if buf.len() >= idx + 4 + clen {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// A scripted mock OmniRoute that drives the agent through all five native
/// tools plus `ask`, and rejects any continuation whose previous tool result is
/// missing (so a broken transcript implementation FAILS the test).
async fn spawn_scripted_mock(workspace: PathBuf) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reqs = requests.clone();

    let src = workspace.join("src");
    let lib_rs = src.join("lib.rs").to_string_lossy().into_owned();
    let created = workspace.join("NOTES.txt").to_string_lossy().into_owned();

    let script: Vec<(String, Vec<u8>)> = vec![
        (
            String::new(),
            ok_response(&sse(&[&tool_chunk(
                "c_read",
                "read",
                &format!("\"{{\\\"path\\\":\\\"{}\\\"}}\"", lib_rs.replace('\\', "\\\\")),
            )])),
        ),
        (
            "c_read".into(),
            ok_response(&sse(&[&tool_chunk(
                "c_grep",
                "grep",
                &format!(
                    "\"{{\\\"pattern\\\":\\\"add\\\",\\\"path\\\":\\\"{}\\\"}}\"",
                    src.to_string_lossy().replace('\\', "\\\\")
                ),
            )])),
        ),
        (
            "c_grep".into(),
            ok_response(&sse(&[&tool_chunk(
                "c_write",
                "write",
                &format!(
                    "\"{{\\\"path\\\":\\\"{}\\\",\\\"content\\\":\\\"notes\\\"}}\"",
                    created.replace('\\', "\\\\")
                ),
            )])),
        ),
        (
            "c_write".into(),
            ok_response(&sse(&[&tool_chunk(
                "c_edit",
                "edit",
                &format!(
                    "\"{{\\\"path\\\":\\\"{}\\\",\\\"old_string\\\":\\\"notes\\\",\\\"new_string\\\":\\\"edited\\\"}}\"",
                    created.replace('\\', "\\\\")
                ),
            )])),
        ),
        (
            "c_edit".into(),
            ok_response(&sse(&[&tool_chunk(
                "c_bash",
                "bash",
                "\"{\\\"command\\\":\\\"echo done\\\"}\"",
            )])),
        ),
        (
            "c_bash".into(),
            ok_response(&sse(&[&tool_chunk(
                "c_ask",
                "ask",
                "\"{\\\"question\\\":\\\"proceed?\\\"}\"",
            )])),
        ),
        ("c_ask".into(), ok_response(&sse(&[&done_chunk("all tools exercised")]))),
    ];

    tokio::spawn(async move {
        for (expect_prev_id, resp) in script {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let req = read_http(&mut sock).await;
            reqs.lock().await.push(req.clone());

            let response = if !expect_prev_id.is_empty()
                && !(req.contains(&format!("\"tool_call_id\":\"{expect_prev_id}\""))
                    && req.contains("\"role\":\"tool\""))
            {
                bad_request(&format!("missing role:tool for {expect_prev_id}"))
            } else {
                resp
            };
            let _ = sock.write_all(&response).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });

    (format!("http://{addr}/v1"), requests)
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

    let (url, requests) = spawn_scripted_mock(ws.clone()).await;
    let cancel = CancellationToken::new();
    let mut actor = build_actor(url, ws.clone(), cancel);
    actor.push_user("exercise every tool then ask");

    let mut noop = |_d: StreamDelta| {};

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

    let reqs = requests.lock().await;
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
///   RUST_AGENT_LIVE=1 cargo test --test e2e -- --ignored --nocapture
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
        let mut on_delta = move |d: StreamDelta| {
            if let StreamDelta::Thinking(_) = d {
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
    let dir = rust_agent::provider::auth::default_config_dir()?;
    rust_agent::provider::auth::resolve_key(&dir, "omniroute", Some("OMNIROUTE_API_KEY")).ok()
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
