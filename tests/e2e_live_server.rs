//! Todo-10 REQUIRED live e2e THROUGH the server path. Starts the real Axum
//! server on a loopback port, creates a session in a temp Rust project, posts
//! a message over HTTP, and consumes the SSE event stream to `turn.end{done}`,
//! asserting: an `llm.delta` `reasoning_content` (thinking) observed >= 1, a
//! real file mutation, a `bash`/`cargo test` tool invocation in the
//! `tool.post_call` events, and `turn.end.outcome == done`.
//!
//! REQUIRED for Phase-2 completion. Run explicitly:
//!   cargo test --test e2e_live_server -- --ignored --nocapture
//! When OmniRoute is unavailable this test FAILS (a BLOCKED gate) — it is
//! NEVER a passing skip. The offline suite uses a mock provider; only THIS
//! test exercises the real proxy through the whole server stack.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use rust_agent::server::session::ServerConfig;
use rust_agent::server::AppState;

const OMNIROUTE: &str = "http://localhost:20128/v1";
const MODEL: &str = "opencode-go/deepseek-v4-flash";

#[tokio::test]
#[ignore]
async fn live_server_edit_and_test_through_the_bus() {
    // 1. Preflight: the proxy + alias must resolve, else BLOCKED (not a skip).
    let key = read_live_key().expect("auth.json with omniroute key required for the live e2e");
    preflight(&key)
        .await
        .expect("OmniRoute preflight failed: proxy unreachable or alias unavailable (BLOCKED)");

    // 2. A temp Rust project + a models.json pointing the `default` alias at
    //    the real OmniRoute proxy, and an auth.json with the live key.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let ws = root.join("ws");
    std::fs::create_dir_all(ws.join("src")).expect("ws src");
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname=\"e2e-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .expect("Cargo.toml");
    std::fs::write(ws.join("src").join("lib.rs"), "// implement add here\n").expect("lib.rs");

    let config_dir = root.join("cfg");
    std::fs::create_dir_all(&config_dir).expect("cfg dir");
    write_live_models(root);
    write_live_auth(&config_dir, &key);

    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().expect("bind"),
        models_path: Some(root.join("models.json")),
        config_dir,
        allow_non_loopback: false,
        bearer_token: None,
        history_db: None,
    };
    let state = AppState::new(config);
    let (addr, server_task) = spawn_server(state).await;
    let base = format!("http://{addr}");

    // 3. Create a session in the temp Rust project.
    let created: Value = reqwest::Client::new()
        .post(format!("{base}/v1/sessions"))
        .json(&json!({ "workspace": ws.to_str().expect("utf8 ws") }))
        .send()
        .await
        .expect("create session")
        .json()
        .await
        .expect("create json");
    let id = created["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    // 4. Open the SSE stream (before posting) so no early event is missed.
    let sse = reqwest::Client::new()
        .get(format!("{base}/v1/sessions/{id}/events"))
        .send()
        .await
        .expect("sse connect");
    assert_eq!(sse.status(), 200);

    // 5. Post the task message over HTTP.
    let posted = reqwest::Client::new()
        .post(format!("{base}/v1/sessions/{id}/messages"))
        .json(&json!({
            "target_actor_id": "agent:main",
            "mode": "next-turn",
            "content": [{ "type": "text", "text":
                "In this Rust project, edit src/lib.rs so it defines \
                 `pub fn add(a: i32, b: i32) -> i32` returning a+b (use the write or edit tool). \
                 Then run `cargo test` via the bash tool to confirm it compiles. \
                 When finished, give a one-line summary." }]
        }))
        .send()
        .await
        .expect("post message");
    assert_eq!(posted.status(), 200, "message accepted");

    // 6. Consume the SSE stream to turn.end{done} (bounded), collecting the
    //    durable + delta events we assert on.
    let events = collect_events_until_turn_end(sse, Duration::from_secs(180)).await;

    let outcome = events
        .iter()
        .find(|e| e["kind"] == "turn.end")
        .map(|e| e["payload"]["outcome"].clone());
    assert_eq!(
        outcome,
        Some(json!("done")),
        "the turn completed with outcome=done through the server; events={}",
        summarize(&events)
    );

    // reasoning_content observed at least once (a thinking llm.delta).
    let saw_reasoning = events
        .iter()
        .any(|e| e["kind"] == "llm.delta" && e["payload"]["delta"]["delta_type"] == "thinking");
    assert!(
        saw_reasoning,
        "at least one reasoning_content (thinking) delta must appear; events={}",
        summarize(&events)
    );

    // A REAL `cargo test` invocation: correlate a bash tool.pre_call whose
    // args contain the cargo-test command with a completed tool.post_call on
    // the SAME tool_call_id (a `bash ls` must NOT satisfy this).
    let cargo_pre = events.iter().find(|e| {
        e["kind"] == "tool.pre_call"
            && e["payload"]["name"] == "bash"
            && format!("{}", e["payload"]["args"]).contains("cargo test")
    });
    let cargo_pre = cargo_pre.unwrap_or_else(|| {
        panic!(
            "no bash tool.pre_call running `cargo test` — the model must actually run it; events={}",
            summarize(&events)
        )
    });
    let tool_call_id = cargo_pre["payload"]["tool_call_id"].clone();
    let ran_cargo_test = events
        .iter()
        .any(|e| e["kind"] == "tool.post_call" && e["payload"]["tool_call_id"] == tool_call_id);
    assert!(
        ran_cargo_test,
        "the `cargo test` bash call must have a matching tool.post_call (tool_call_id={tool_call_id}); events={}",
        summarize(&events)
    );

    // A real file mutation: src/lib.rs now defines `add`.
    let lib = std::fs::read_to_string(ws.join("src").join("lib.rs")).expect("read lib.rs");
    assert!(
        lib.contains("fn add"),
        "the model must have edited src/lib.rs to add `add`; got:\n{lib}"
    );

    server_task.abort();
    println!("live server e2e: PASS (outcome=done, reasoning+bash+file-mutation through the bus)");
}

// ---- helpers ----

fn read_live_key() -> Option<String> {
    let dir = rust_agent::provider::auth::default_config_dir()?;
    rust_agent::provider::auth::resolve_key(&dir, "omniroute", Some("OMNIROUTE_API_KEY")).ok()
}

async fn preflight(key: &str) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(format!("{OMNIROUTE}/chat/completions"))
        .bearer_auth(key)
        .json(&json!({
            "model": MODEL,
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

fn write_live_models(dir: &Path) {
    std::fs::write(
        dir.join("models.json"),
        json!({
            "providers": {
                "omniroute": {
                    "base_url": OMNIROUTE,
                    "api_key_env": "OMNIROUTE_API_KEY",
                    "headers": {},
                    "connect_timeout_ms": 5000,
                    "request_timeout_ms": 600000
                }
            },
            "models": {
                "default": {
                    "provider": "omniroute",
                    "model": MODEL,
                    "reasoning_history": "preserve",
                    "max_output_tokens": 4000
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
}

fn write_live_auth(config_dir: &Path, key: &str) {
    let auth = config_dir.join("auth.json");
    std::fs::write(
        &auth,
        json!({ "omniroute": { "api_key": key } }).to_string(),
    )
    .expect("write auth.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
    }
}

async fn spawn_server(state: AppState) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind");
    let addr = listener.local_addr().expect("addr");
    let task = tokio::spawn(async move {
        let _ = rust_agent::server::routes::serve_state_on(state, listener).await;
    });
    for _ in 0..100 {
        if reqwest::get(format!("http://{addr}/v1/health"))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (addr, task)
}

/// Parse the SSE body, collecting each event envelope until `turn.end` (or the
/// budget elapses). Returns the collected envelopes.
async fn collect_events_until_turn_end(resp: reqwest::Response, budget: Duration) -> Vec<Value> {
    let mut stream = resp;
    let mut buf = String::new();
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(idx) = buf.find("\n\n") {
                    let raw = buf[..idx].to_string();
                    buf.drain(..idx + 2);
                    for line in raw.lines() {
                        if let Some(data) = line.strip_prefix("data:") {
                            if let Ok(env) = serde_json::from_str::<Value>(data.trim()) {
                                let is_end = env["kind"] == "turn.end";
                                out.push(env);
                                if is_end {
                                    return out;
                                }
                            }
                        }
                    }
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    out
}

fn summarize(events: &[Value]) -> String {
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
    format!("{kinds:?}")
}
